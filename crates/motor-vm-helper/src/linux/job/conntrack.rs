//! Retire per-allocation IPv4 conntrack state, including its attached NAT state.
//! Use NETLINK_NETFILTER directly; no conntrack userspace package is required.
use crate::{Error, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const CT_GET: u16 = 0x101;
const CT_DELETE: u16 = 0x102;
const HEADER: usize = 16;
const NFGEN: usize = 4;

fn invalid() -> Error {
    Error("invalid or interrupted conntrack netlink response".into())
}
fn align(n: usize) -> usize {
    (n + 3) & !3
}
fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_ne_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or_else(invalid)?
            .try_into()
            .unwrap(),
    ))
}
fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_ne_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(invalid)?
            .try_into()
            .unwrap(),
    ))
}
fn attributes(mut bytes: &[u8]) -> Result<Vec<(u16, &[u8])>> {
    let mut result = Vec::new();
    while !bytes.is_empty() {
        let len = usize::from(u16_at(bytes, 0)?);
        if len < 4 || align(len) > bytes.len() {
            return Err(invalid());
        }
        result.push((u16_at(bytes, 2)? & 0x3fff, &bytes[..len]));
        bytes = &bytes[align(len)..];
    }
    Ok(result)
}

fn deletion(entry: &[u8], address: [u8; 4]) -> Result<Option<Vec<u8>>> {
    let attrs = attributes(entry.get(NFGEN..).ok_or_else(invalid)?)?;
    let mut matched = false;
    for (_, tuple) in attrs.iter().filter(|(kind, _)| *kind == 1 || *kind == 2) {
        for (_, ip) in attributes(&tuple[4..])?
            .into_iter()
            .filter(|(kind, _)| *kind == 1)
        {
            for (kind, value) in attributes(&ip[4..])? {
                if (kind == 1 || kind == 2) && value[4..] == address {
                    matched = true;
                }
            }
        }
    }
    if !matched {
        return Ok(None);
    }
    let mut delete = vec![libc::AF_INET as u8, 0, 0, 0];
    // Original tuple plus zone identifies the entry. Delete the entire entry,
    // not only its original direction, so reverse NAT cannot survive reuse.
    if !attrs.iter().any(|(kind, _)| *kind == 1) {
        return Err(invalid());
    }
    for (_, attr) in attrs
        .into_iter()
        .filter(|(kind, _)| *kind == 1 || *kind == 18)
    {
        delete.extend_from_slice(attr);
        delete.resize(align(delete.len()), 0);
    }
    Ok(Some(delete))
}

struct Netlink {
    fd: OwnedFd,
    sequence: u32,
}
impl Netlink {
    fn open() -> Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_NETFILTER,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as u16;
        let timeout = libc::timeval {
            tv_sec: 10,
            tv_usec: 0,
        };
        if unsafe {
            libc::connect(
                fd.as_raw_fd(),
                &address as *const _ as *const libc::sockaddr,
                std::mem::size_of_val(&address) as libc::socklen_t,
            )
        } != 0
            || unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    &timeout as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&timeout) as libc::socklen_t,
                )
            } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self { fd, sequence: 0 })
    }

    fn request(&mut self, kind: u16, flags: u16, payload: &[u8]) -> Result<()> {
        self.sequence += 1;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&((HEADER + payload.len()) as u32).to_ne_bytes());
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(&(flags | 1).to_ne_bytes()); // NLM_F_REQUEST
        bytes.extend_from_slice(&self.sequence.to_ne_bytes());
        bytes.extend_from_slice(&0u32.to_ne_bytes());
        bytes.extend_from_slice(payload);
        if unsafe { libc::send(self.fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) }
            != bytes.len() as isize
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn receive(&self, mut entry: impl FnMut(&[u8]) -> Result<()>, deleting: bool) -> Result<()> {
        loop {
            let mut bytes = vec![0u8; 256 * 1024];
            let size = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                    libc::MSG_TRUNC,
                )
            };
            if size < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if size == 0 || size as usize > bytes.len() {
                return Err(invalid());
            }
            let mut messages = &bytes[..size as usize];
            while !messages.is_empty() {
                let len = u32_at(messages, 0)? as usize;
                if len < HEADER
                    || align(len) > messages.len()
                    || u32_at(messages, 8)? != self.sequence
                    || u16_at(messages, 6)? & 0x10 != 0
                {
                    return Err(invalid());
                }
                let payload = &messages[HEADER..len];
                match u16_at(messages, 4)? {
                    2 => {
                        // NLMSG_ERROR, including successful ACKs
                        let code = u32_at(payload, 0)? as i32;
                        if code == 0 || (deleting && code == -libc::ENOENT) {
                            return Ok(());
                        }
                        return Err(std::io::Error::from_raw_os_error(-code).into());
                    }
                    3 => {
                        // NLMSG_DONE can itself carry a dump error.
                        if payload.len() >= 4 && u32_at(payload, 0)? != 0 {
                            return Err(invalid());
                        }
                        return Ok(());
                    }
                    0x100 => entry(payload)?, // IPCTNL_MSG_CT_NEW
                    _ => return Err(invalid()),
                }
                messages = &messages[align(len)..];
            }
        }
    }
}

pub(super) fn flush(index: u8) -> Result<()> {
    if !(1..=254).contains(&index) {
        return Err(Error("invalid conntrack VM address".into()));
    }
    let mut netlink = Netlink::open()?;
    netlink.request(CT_GET, 0x300, &[libc::AF_INET as u8, 0, 0, 0])?; // NLM_F_DUMP
    let mut deletions = Vec::new();
    netlink.receive(
        |entry| {
            if let Some(delete) = deletion(entry, [10, 200, index, 2])? {
                deletions.push(delete);
            }
            Ok(())
        },
        false,
    )?;
    for delete in deletions {
        netlink.request(CT_DELETE, 4, &delete)?; // NLM_F_ACK
        netlink.receive(|_| Err(invalid()), true)?;
    }
    Ok(())
}

#[cfg(all(test, feature = "linux-root-tests"))]
pub(super) fn count(index: u8) -> Result<usize> {
    let mut netlink = Netlink::open()?;
    netlink.request(CT_GET, 0x300, &[libc::AF_INET as u8, 0, 0, 0])?;
    let mut count = 0;
    netlink.receive(
        |entry| {
            count += usize::from(deletion(entry, [10, 200, index, 2])?.is_some());
            Ok(())
        },
        false,
    )?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(kind: u16, data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&((data.len() + 4) as u16).to_ne_bytes());
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(data);
        bytes.resize(align(bytes.len()), 0);
        bytes
    }
    fn tuple(src: [u8; 4], dst: [u8; 4]) -> Vec<u8> {
        attr(0x8001, &[attr(1, &src), attr(2, &dst)].concat())
    }
    #[test]
    fn retirement_matches_both_directions_and_preserves_zone_without_foreign_flows() {
        for direction in [1, 2] {
            for source in [true, false] {
                let vm = [10, 200, 17, 2];
                let public = [8, 8, 4, 2];
                let mut entry = vec![libc::AF_INET as u8, 0, 0, 0];
                let own = tuple(
                    if source { vm } else { public },
                    if source { public } else { vm },
                );
                let other = tuple(public, public);
                let orig = attr(0x8001, if direction == 1 { &own } else { &other });
                entry.extend_from_slice(&orig);
                entry.extend_from_slice(&attr(0x8002, if direction == 2 { &own } else { &other }));
                let zone = attr(18, &[0, 42]);
                entry.extend_from_slice(&zone);
                let expected = [vec![libc::AF_INET as u8, 0, 0, 0], orig, zone].concat();
                assert_eq!(deletion(&entry, vm).unwrap(), Some(expected));
                assert_eq!(deletion(&entry, [10, 200, 18, 2]).unwrap(), None);
            }
        }
        assert!(deletion(&[2, 0, 0, 0, 3, 0, 1, 0], [10, 200, 17, 2]).is_err());
    }

    #[cfg(feature = "linux-root-tests")]
    mod root {
        use super::*;
        use crate::linux::job::namespace::{
            self,
            root_tests::{job_link, public_link, Fixture, Node},
        };
        use crate::linux::tenant_network::Tap;

        #[test]
        fn isolated_delayed_udp_reply_is_not_delivered_after_tap_index_reuse() {
            let _fixture = Fixture::new();
            let mut remote = Node::new(
                r#"
import socket,sys
print('ready'); sys.stdin.readline()
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.settimeout(5)
s.bind(('8.8.4.2',42000)); print('bound')
data,peer=s.recvfrom(64); assert data==b'job-A'
s.sendto(b'warmup',peer); print('flow-established')
sys.stdin.readline(); s.sendto(b'delayed-job-A',peer); print('sent-delayed')
"#,
            );
            public_link(&remote);
            let a = "udp-predecessor";
            job_link(a);
            let mut guest = Node::guest(a);
            remote.send("serve");
            assert_eq!(remote.line(), "bound");
            guest.send("send 8.8.4.2 job-A");
            assert_eq!(remote.line(), "flow-established");
            assert_eq!(guest.line(), "warmup");
            assert!(
                namespace::in_namespace(a, || count(17)).unwrap() > 0,
                "fixture did not track guest forwarding inside namespace"
            );
            drop(guest);
            let tap = Tap {
                tap_index: 17,
                uid: 500000,
            };
            namespace::disconnect(a, &tap).unwrap();
            assert_eq!(
                namespace::in_namespace(a, || count(17)).unwrap(),
                0,
                "namespace retained predecessor conntrack state"
            );
            // The job namespace is retired, but the live host uplink retains
            // the reverse-NAT mapping until the host is flushed separately.
            let matches = count(17).unwrap();
            assert!(matches > 0, "fixture did not retain predecessor NAT state");
            flush(17).unwrap();
            assert_eq!(count(17).unwrap(), 0, "host retained predecessor NAT state");
            namespace::remove(a).unwrap();
            let b = "udp-successor";
            job_link(b);
            let mut guest = Node::guest(b);
            // Resolve the successor's ARP before the delayed packet is sent.
            namespace::root_tests::command(
                "/usr/bin/nsenter",
                &[
                    &format!("--net={}", namespace::path(b).unwrap().display()),
                    "--",
                    "/usr/sbin/ip",
                    "neigh",
                    "replace",
                    "10.200.17.2",
                    "lladdr",
                    "02:fc:00:00:11:02",
                    "dev",
                    "mtap17",
                ],
            );
            guest.send("receive");
            remote.send("reply-to-A");
            assert_eq!(remote.line(), "sent-delayed");
            assert_eq!(
                guest.line(),
                "timeout",
                "successor received predecessor traffic"
            );
        }
    }
}
