use super::{fs, LinuxMachine, SOCKET};
use crate::{Error, Helper, Peer, Reply, Request, Response, Result};
use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

const MAX_REQUEST: usize = 4096;
pub fn peer(stream: &UnixStream) -> Result<Peer> {
    let mut credential: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credential as *mut libc::ucred).cast(),
            &mut len,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(Error("invalid SO_PEERCRED length".into()));
    }
    Ok(Peer {
        uid: credential.uid,
        gid: credential.gid,
    })
}

fn initialize_helper<M: crate::Machine>(
    machine: M,
    count: usize,
    allowed: Vec<u32>,
    socket: &Path,
) -> Result<(Mutex<Helper<M>>, UnixListener)> {
    // Qualification deliberately holds provision.lock while starting a helper.
    // Startup must not acquire it; the helper mutex serializes online removal.
    let helper = Mutex::new(Helper::new(machine, count, allowed)?);
    // Cleanup and trust checks run before the socket is bound. An invalid
    // uplink may still leave a fenced helper serving status, stop and removal.
    match std::fs::symlink_metadata(socket) {
        Ok(m) if m.file_type().is_socket() => std::fs::remove_file(socket)?,
        Ok(_) => return Err(Error("helper socket path is not a socket".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok((helper, UnixListener::bind(socket)?))
}

pub fn serve(config: crate::config::Config) -> Result<()> {
    let count = config.slots.len();
    let allowed = config.allowed_uids.clone();
    let socket_gid = config.socket_gid;
    let machine = LinuxMachine::new(config)?;
    // Before any thread exists, so every thread inherits the blocked mask.
    let signals = crate::signals::StopSignals::block()?;
    let (helper, listener) = initialize_helper(machine, count, allowed, Path::new(SOCKET))?;
    let helper = Arc::new(helper);
    let path = c"/run/motor-vm-helper.sock";
    if unsafe { libc::chown(path.as_ptr(), 0, socket_gid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o660))?;
    let maintenance = helper.clone();
    std::thread::spawn(move || loop {
        std::thread::park_timeout(Duration::from_millis(100));
        maintenance.lock().unwrap().maintain();
    });
    // Removed tenants' quarantined data is deleted here once it is old
    // enough. Only removal writes that folder, so this needs no helper lock
    // and a long deletion never holds up tenant operations.
    std::thread::spawn(|| loop {
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Err(e) = crate::removal::purge(&crate::removal::Paths::live(), now_s) {
            eprintln!("removed-tenant purge failed: {e}");
        }
        std::thread::sleep(Duration::from_secs(3600));
    });
    let stopping = helper.clone();
    std::thread::spawn(move || {
        // A tenant that did not stop cleanly is already recorded as unclean,
        // so its next start runs the guest's recovery check. Exit 0 either
        // way: the stop was requested, and a failed unit would make
        // `provision.py switch-image`, which needs both services inactive,
        // refuse until the failure is reset by hand.
        match crate::signals::stop_on_signal(&stopping, &signals) {
            Ok(signal) => eprintln!("motor-vm-helper: stopped after signal {signal}"),
            Err(e) => eprintln!("motor-vm-helper: {e}"),
        }
        std::process::exit(0);
    });
    for connection in listener.incoming() {
        let stream = connection?;
        let peer = peer(&stream)?;
        let id = match helper.lock().unwrap().connect(peer) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let helper = helper.clone();
        std::thread::spawn(move || {
            let result = connection_loop(&helper, id, stream);
            helper.lock().unwrap().disconnect(id);
            if let Err(e) = result {
                eprintln!("helper connection: {e}");
            }
        });
    }
    Ok(())
}
fn connection_loop<M: crate::Machine>(
    helper: &Mutex<Helper<M>>,
    id: u64,
    mut stream: UnixStream,
) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    loop {
        // fill_buf/consume bounds the allocation even if a peer never sends a
        // newline. read_line with a later length check would not be a bound.
        let mut line = Vec::new();
        loop {
            let buf = reader.fill_buf()?;
            if buf.is_empty() {
                return Ok(());
            }
            let end = buf
                .iter()
                .position(|b| *b == b'\n')
                .map(|n| n + 1)
                .unwrap_or(buf.len());
            if line.len() + end > MAX_REQUEST {
                return Err(Error("helper request exceeds 4096 bytes".into()));
            }
            line.extend_from_slice(&buf[..end]);
            reader.consume(end);
            if line.last() == Some(&b'\n') {
                break;
            }
        }
        let result = serde_json::from_slice::<Request>(&line)
            .map_err(|e| Error(e.to_string()))
            .and_then(|request| helper.lock().unwrap().handle(id, request));
        let response = match result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("helper refusal: {e}");
                Response {
                    reply: Reply::Error { message: e.0 },
                    fds: Vec::new(),
                }
            }
        };
        send_response(&mut stream, response)?;
    }
}
pub fn send_response(stream: &mut UnixStream, response: Response) -> Result<()> {
    let mut bytes = serde_json::to_vec(&response.reply).map_err(|e| Error(e.to_string()))?;
    bytes.push(b'\n');
    if response.fds.is_empty() {
        stream.write_all(&bytes)?;
        return Ok(());
    }
    let fds: Vec<i32> = response.fds.iter().map(|fd| fd.as_raw_fd()).collect();
    let control_len =
        unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds.as_slice()) as u32) } as usize;
    // usize storage provides the cmsghdr alignment required by sendmsg.
    let mut control = vec![0usize; control_len.div_ceil(std::mem::size_of::<usize>())];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = control_len;
    let sent = unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds.as_slice()) as u32) as usize;
        std::ptr::copy_nonoverlapping(
            fds.as_ptr().cast::<u8>(),
            libc::CMSG_DATA(cmsg),
            std::mem::size_of_val(fds.as_slice()),
        );
        libc::sendmsg(stream.as_raw_fd(), &header, libc::MSG_NOSIGNAL)
    };
    if sent < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if sent == 0 {
        return Err(Error("SCM_RIGHTS send made no progress".into()));
    }
    // Ancillary data goes with the first byte, exactly once, even on a short
    // write. Sending it again would duplicate descriptors in the receiver.
    stream.write_all(&bytes[sent as usize..])?;
    Ok(())
}
pub fn load_config(path: &Path) -> Result<crate::config::Config> {
    let file: File = fs::trusted_file(path)?;
    let config: crate::config::Config =
        serde_json::from_reader(file).map_err(|e| Error(e.to_string()))?;
    config.validate_resources()?;
    Ok(config)
}

#[cfg(test)]
mod tests;
