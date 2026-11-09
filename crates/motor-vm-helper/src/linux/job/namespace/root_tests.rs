use super::*;
use crate::linux::tenant_network;
use std::{
    io::{BufRead, BufReader},
    os::linux::net::SocketAddrExt,
    os::unix::net::{SocketAddr, UnixListener, UnixStream},
    process::{ChildStdin, ChildStdout},
};

// Both namespaces are private so these tests cannot discover or remove real
// helper namespaces, policy tables or interfaces on the operator's machine.
pub(in crate::linux::job) struct Fixture {
    net: File,
    mount: File,
    scratch: PathBuf,
}
impl Fixture {
    pub(in crate::linux::job) fn new() -> Self {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        let net = File::open("/proc/thread-self/ns/net").unwrap();
        let mount = File::open("/proc/thread-self/ns/mnt").unwrap();
        assert_eq!(
            unsafe { libc::unshare(libc::CLONE_NEWNET | libc::CLONE_NEWNS) },
            0
        );
        command("/usr/bin/mount", &["--make-rprivate", "/"]);
        let scratch = super::super::tests::scratch();
        std::fs::create_dir_all(ROOT).unwrap();
        command(
            "/usr/bin/mount",
            &["--bind", scratch.to_str().unwrap(), ROOT],
        );
        Self {
            net,
            mount,
            scratch,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Named mounts must be unmounted before leaving the private mount ns.
        let _ = reconcile(&BTreeSet::new());
        assert_eq!(
            unsafe { libc::setns(self.net.as_raw_fd(), libc::CLONE_NEWNET) },
            0
        );
        assert_eq!(
            unsafe { libc::setns(self.mount.as_raw_fd(), libc::CLONE_NEWNS) },
            0
        );
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

pub(in crate::linux::job) fn command(program: &str, args: &[&str]) {
    let output = Command::new(program).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(in crate::linux::job) struct Node {
    pub(in crate::linux::job) child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Node {
    pub(in crate::linux::job) fn new(script: &str) -> Self {
        let mut command = Command::new("/usr/bin/unshare");
        command.args(["--net", "/usr/bin/python3", "-u", "-c", script]);
        Self::spawn(command)
    }
    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut node = Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        assert_eq!(node.line(), "ready");
        node
    }
    pub(in crate::linux::job) fn guest(id: &str) -> Self {
        let mut command = inside(id, "/usr/bin/python3").unwrap();
        command.args(["-u", "-c", include_str!("tap_peer.py")]);
        Self::spawn(command)
    }
    pub(in crate::linux::job) fn send(&mut self, text: &str) {
        writeln!(self.input, "{text}").unwrap();
        self.input.flush().unwrap();
    }
    pub(in crate::linux::job) fn line(&mut self) -> String {
        let mut line = String::new();
        assert!(
            self.output.read_line(&mut line).unwrap() > 0,
            "network fixture exited"
        );
        line.trim().to_string()
    }
    pub(in crate::linux::job) fn ip(&self, args: &[&str]) {
        command(
            "/usr/bin/nsenter",
            &[
                &[
                    &format!("--net=/proc/{}/ns/net", self.child.id()),
                    "--",
                    "/usr/sbin/ip",
                ][..],
                args,
            ]
            .concat(),
        );
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(in crate::linux::job) fn public_link(remote: &Node) {
    command(
        "/usr/sbin/ip",
        &[
            "link", "add", "public0", "type", "veth", "peer", "name", "remote0",
        ],
    );
    command(
        "/usr/sbin/ip",
        &[
            "link",
            "set",
            "remote0",
            "netns",
            &remote.child.id().to_string(),
        ],
    );
    command(
        "/usr/sbin/ip",
        &["addr", "add", "8.8.4.1/24", "dev", "public0"],
    );
    command("/usr/sbin/ip", &["link", "set", "public0", "up"]);
    remote.ip(&["addr", "add", "8.8.4.2/24", "dev", "remote0"]);
    remote.ip(&["link", "set", "remote0", "up"]);
    let policy = tenant_network::test_job_policy("public0", 17);
    let mut nft = Command::new("/usr/sbin/nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    nft.stdin
        .take()
        .unwrap()
        .write_all(policy.as_bytes())
        .unwrap();
    assert!(nft.wait().unwrap().success());
}

pub(in crate::linux::job) fn job_link(id: &str) {
    create(id).unwrap();
    connect(
        id,
        &Tap {
            tap_index: 17,
            uid: 500000,
        },
    )
    .unwrap();
    assert!(
        links().unwrap().iter().any(|link| owned(
            link,
            id,
            &Tap {
                tap_index: 17,
                uid: 500000
            }
        )
        .unwrap()),
        "host veth must carry the job ownership alias after moving namespaces"
    );
}

#[test]
fn isolated_job_namespace_cannot_connect_to_host_abstract_socket() {
    let _fixture = Fixture::new();
    let address = SocketAddr::from_abstract_name(b"motor-job-abstract-boundary").unwrap();
    let _listener = UnixListener::bind_addr(&address).unwrap();
    assert!(
        UnixStream::connect_addr(&address).is_ok(),
        "host control must connect"
    );
    let id = "abstract-test";
    // Recovery VMs have the same abstract-socket boundary, with only down lo.
    create(id).unwrap();
    let output = inside(id, "/usr/bin/python3")
        .unwrap()
        .args([
            "-c",
            r#"
import socket
s=socket.socket(socket.AF_UNIX)
try: s.connect('\0motor-job-abstract-boundary')
except ConnectionRefusedError: pass
else: raise AssertionError('job reached a host abstract socket')
"#,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = inside(id, "/usr/sbin/ip")
        .unwrap()
        .args(["-j", "link"])
        .output()
        .unwrap();
    let links: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["ifname"], "lo");
    assert!(!links[0]["flags"].as_array().unwrap().contains(&json!("UP")));
    remove(id).unwrap();
    remove(id).unwrap();
}

#[test]
fn isolated_job_egress_is_counted_and_transit_addresses_are_unreachable() {
    let _fixture = Fixture::new();
    let mut remote = Node::new(
        r#"
import socket,sys
print('ready'); sys.stdin.readline()
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.settimeout(5)
s.bind(('8.8.4.2',42000)); print('bound')
data,peer=s.recvfrom(64); assert data==b'public'; s.sendto(b'public-reply',peer); print('replied')
t=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); t.bind(('10.201.18.2',42000))
while True:
 data,peer=t.recvfrom(64); t.sendto(b'forbidden-other',peer)
"#,
    );
    public_link(&remote);
    remote.ip(&["addr", "add", "10.201.18.2/32", "dev", "remote0"]);
    command(
        "/usr/sbin/ip",
        &[
            "route",
            "add",
            "10.201.18.2/32",
            "via",
            "8.8.4.2",
            "dev",
            "public0",
        ],
    );
    let id = "egress-test";
    job_link(id);
    let mut guest = Node::guest(id);
    let before = tenant_network::egress_bytes(&Tap {
        tap_index: 17,
        uid: 500000,
    })
    .unwrap();
    remote.send("serve");
    assert_eq!(remote.line(), "bound");
    guest.send("send 8.8.4.2 public");
    assert_eq!(guest.line(), "public-reply");
    assert_eq!(remote.line(), "replied");
    let after = tenant_network::egress_bytes(&Tap {
        tap_index: 17,
        uid: 500000,
    })
    .unwrap();
    assert_eq!(
        after - before,
        20 + 8 + 6,
        "counter must count original guest IPv4 bytes exactly once"
    );
    let mut ns_server = inside(id, "/usr/bin/python3").unwrap();
    ns_server.args(["-u", "-c", r#"
import socket
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.bind(('10.201.17.2',42000)); s.settimeout(3); print('ready')
try: s.recvfrom(64); print('namespace-input-reached')
except socket.timeout: print('no-namespace-input')
"#]);
    let mut ns_server = Node::spawn(ns_server);
    guest.send("send 10.201.17.2 probe");
    assert_eq!(guest.line(), "timeout");
    assert_eq!(
        ns_server.line(),
        "no-namespace-input",
        "namespace input chain admitted guest traffic"
    );
    // Bind host and another-job transit addresses to a real responder. A
    // timeout therefore proves refusal, not merely the absence of a service.
    command(
        "/usr/sbin/ip",
        &["addr", "add", "10.201.18.1/32", "dev", "public0"],
    );
    let mut host_server = Command::new("/usr/bin/python3");
    host_server.args([
        "-u",
        "-c",
        r#"
import socket,select
ss=[]
for ip in ['10.201.17.1','10.201.18.1']:
 s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.bind((ip,42000)); ss.append(s)
print('ready')
while True:
 for s in select.select(ss,[],[])[0]:
  data,peer=s.recvfrom(64); s.sendto(b'forbidden-host',peer)
"#,
    ]);
    let _host_server = Node::spawn(host_server);
    for address in ["10.201.17.1", "10.201.18.1", "10.201.18.2"] {
        guest.send(&format!("send {address} probe"));
        assert_eq!(guest.line(), "timeout", "guest reached transit {address}");
    }
    let output = inside(id, "/usr/sbin/ip")
        .unwrap()
        .args(["-j", "link"])
        .output()
        .unwrap();
    let links: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    let names: BTreeSet<_> = links
        .iter()
        .map(|link| link["ifname"].as_str().unwrap())
        .collect();
    assert_eq!(names, BTreeSet::from(["lo", "mtap17", "uplink"]));
    for link in links.iter().filter(|link| link["ifname"] != "lo") {
        assert_eq!(link["mtu"], 1500);
    }
}

#[test]
fn isolated_startup_removes_orphan_namespaces_and_veths_idempotently() {
    let _fixture = Fixture::new();
    let live = "retained-job";
    let orphan = "orphan-job";
    let partial = "partially-created-job";
    create(live).unwrap();
    job_link(orphan);
    create(partial).unwrap();
    std::fs::write(path("unmounted-job").unwrap(), "").unwrap();
    let retained = BTreeSet::from([live.to_string()]);
    reconcile(&retained).unwrap();
    reconcile(&retained).unwrap();
    assert!(path(live).unwrap().exists());
    assert!(!path(orphan).unwrap().exists());
    assert!(!path(partial).unwrap().exists());
    assert!(!path("unmounted-job").unwrap().exists());
    assert!(!links()
        .unwrap()
        .iter()
        .any(|link| link["ifname"] == "mtap17"));
    remove(live).unwrap();
    remove(live).unwrap();
}

#[test]
fn isolated_startup_reclaims_aliasless_veths_after_interrupted_creation() {
    let _fixture = Fixture::new();
    let id = "aliasless-job";
    create(id).unwrap();
    // Model a crash after the two peers are created but before the explicit
    // alias assignment and move to the host namespace can run.
    ns_ip(
        id,
        &[
            "link", "add", "mtap17", "type", "veth", "peer", "name", UPLINK,
        ],
    )
    .unwrap();
    let output = inside(id, "/usr/sbin/ip")
        .unwrap()
        .args(["-j", "-d", "link", "show", "mtap17"])
        .output()
        .unwrap();
    let internal: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert!(internal[0]["ifalias"].is_null());
    assert!(!links()
        .unwrap()
        .iter()
        .any(|link| link["ifname"] == "mtap17"));
    reconcile(&BTreeSet::new()).unwrap();
    reconcile(&BTreeSet::new()).unwrap();
    assert!(!path(id).unwrap().exists());
    // Also reclaim the old host-first creation layout: the namespace mount
    // identifies the untagged peer, so deleting it must remove the host veth.
    create(id).unwrap();
    ip(&[
        "link",
        "add",
        "mtap17",
        "type",
        "veth",
        "peer",
        "name",
        UPLINK,
        "netns",
        &name(id).unwrap(),
    ])
    .unwrap();
    let host = links()
        .unwrap()
        .into_iter()
        .find(|link| link["ifname"] == "mtap17")
        .unwrap();
    assert!(host["ifalias"].is_null());
    reconcile(&BTreeSet::new()).unwrap();
    assert!(!path(id).unwrap().exists());
    assert!(!links()
        .unwrap()
        .iter()
        .any(|link| link["ifname"] == "mtap17"));
}
