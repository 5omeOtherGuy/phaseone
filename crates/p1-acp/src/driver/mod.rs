//! The `p1 acp` driver: the stdio session loop over the front-end port (D7).

mod front_end;
mod hold;
pub mod io;
mod session;

pub use front_end::{AcpFrontEnd, Reader, Writer};

use p1_contracts::CancellationToken;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

/// The client's half of the stream, which says when the client is gone: the transport
/// keeps its handlers alive past EOF while their prompts run, so a loop cannot wait for
/// the handler to go away.
pub(crate) struct Watched {
    inner: Reader,
    gone: CancellationToken,
}

impl Watched {
    pub(crate) fn new(inner: Reader, gone: CancellationToken) -> Self {
        Self { inner, gone }
    }
}

impl AsyncRead for Watched {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let room = buf.remaining() > 0;
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        match &polled {
            Poll::Ready(Ok(())) if room && buf.filled().len() == before => self.gone.cancel(),
            Poll::Ready(Err(_)) => self.gone.cancel(),
            _ => {}
        }
        polled
    }
}
