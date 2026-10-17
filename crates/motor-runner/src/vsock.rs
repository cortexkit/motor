//! Nonblocking AF_VSOCK listeners restricted to the hypervisor's host CID.
use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{unix::AsyncFd, AsyncRead, AsyncWrite, ReadBuf};

pub(crate) struct Listener(AsyncFd<OwnedFd>);
pub(crate) struct Stream(AsyncFd<OwnedFd>);

impl Listener {
    pub(crate) fn bind(port: u32) -> io::Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_VSOCK,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut address: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
        address.svm_family = libc::AF_VSOCK as _;
        address.svm_cid = libc::VMADDR_CID_ANY;
        address.svm_port = port;
        let result = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &address as *const _ as *const libc::sockaddr,
                std::mem::size_of_val(&address) as _,
            )
        };
        if result < 0 || unsafe { libc::listen(fd.as_raw_fd(), 128) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(AsyncFd::new(fd)?))
    }
    pub(crate) async fn accept(&self) -> io::Result<Stream> {
        loop {
            let mut ready = self.0.readable().await?;
            let accepted = ready.try_io(|fd| {
                let mut peer: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
                let mut len = std::mem::size_of_val(&peer) as libc::socklen_t;
                let socket = unsafe {
                    libc::accept4(
                        fd.as_raw_fd(),
                        &mut peer as *mut _ as *mut libc::sockaddr,
                        &mut len,
                        libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    )
                };
                if socket < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok((unsafe { OwnedFd::from_raw_fd(socket) }, peer.svm_cid))
            });
            match accepted {
                Ok(Ok((socket, cid))) if host_peer(cid) => {
                    return Ok(Stream(AsyncFd::new(socket)?))
                }
                Ok(Ok(_)) => continue,
                Ok(Err(error)) => return Err(error),
                Err(_) => continue,
            }
        }
    }
}

fn host_peer(cid: u32) -> bool {
    cid == libc::VMADDR_CID_HOST
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut ready = std::task::ready!(self.0.poll_read_ready(cx))?;
            match ready.try_io(|fd| {
                let dst = buf.initialize_unfilled();
                let n = unsafe { libc::read(fd.as_raw_fd(), dst.as_mut_ptr().cast(), dst.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(result) => {
                    buf.advance(result?);
                    return Poll::Ready(Ok(()));
                }
                Err(_) => continue,
            }
        }
    }
}
impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut ready = std::task::ready!(self.0.poll_write_ready(cx))?;
            match ready.try_io(|fd| {
                let n = unsafe {
                    libc::send(
                        fd.as_raw_fd(),
                        buf.as_ptr().cast(),
                        buf.len(),
                        libc::MSG_NOSIGNAL,
                    )
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(result) => return Poll::Ready(result),
                Err(_) => continue,
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = unsafe { libc::shutdown(self.0.as_raw_fd(), libc::SHUT_WR) };
        Poll::Ready(if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_host_cid_is_allowed() {
        assert!(super::host_peer(2));
        for cid in [0, 1, 3, 42, u32::MAX] {
            assert!(!super::host_peer(cid));
        }
    }
}
