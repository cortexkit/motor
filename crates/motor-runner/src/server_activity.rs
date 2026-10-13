use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::watch,
    time::{Instant, Sleep},
};

#[derive(Clone, Copy, Default)]
pub(super) struct Activity {
    pub chunk_at: Option<Instant>,
    pub partial_at: Option<Instant>,
}

/// Track bytes inside an incomplete frame provisionally. Once it decodes,
/// heartbeats discard that provisional progress; nonempty chunks retain it.
/// This lets a slowly fragmented chunk exceed 60 seconds in total without
/// letting complete transport heartbeats hide an idle upload.
pub(super) struct ActivityRead<R> {
    pub read: R,
    pub activity: watch::Sender<Activity>,
    pub idle: Pin<Box<Sleep>>,
}
impl<R: AsyncRead + Unpin> AsyncRead for ActivityRead<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.read).poll_read(cx, buf) {
            Poll::Ready(result) => {
                if result.is_ok() && buf.filled().len() > before {
                    let now = Instant::now();
                    self.activity.send_modify(|p| p.partial_at = Some(now));
                    self.idle.as_mut().reset(now + Duration::from_secs(60));
                }
                Poll::Ready(result)
            }
            Poll::Pending => {
                use std::future::Future;
                match self.idle.as_mut().poll(cx) {
                    Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "wire inactivity",
                    ))),
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }
}
