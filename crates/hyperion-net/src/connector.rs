//! Bridges `glommio::net::TcpStream` (and TLS on top of it) into hyper's
//! runtime traits.
//!
//! # Why an `unsafe impl Send`
//!
//! glommio's `TcpStream` is `!Send` because its internal `Source` is backed by
//! `Rc<RefCell<_>>`. hyper's `http1::Builder::handshake` requires the stream
//! to be `Send + 'static`. This is safe here because the stream is created,
//! used and dropped on a single Glommio executor thread and is never moved
//! across threads: hyper's `Connection` future is driven on the same thread
//! via `glommio::spawn_local`.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use futures_lite::io::{AsyncRead, AsyncWrite};

/// A glommio TCP stream wrapped for hyper.
pub struct GlommioStream {
    inner: glommio::net::TcpStream,
}

// SAFETY: see module docs. Never moved off the owning glommio executor thread.
unsafe impl Send for GlommioStream {}

impl GlommioStream {
    pub fn new(inner: glommio::net::TcpStream) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &glommio::net::TcpStream {
        &self.inner
    }
}

// Delegate futures-io traits so `GlommioStream` can be handed to
// futures-rustls, which needs `AsyncRead + AsyncWrite + Unpin` rather than
// hyper's runtime traits.
impl futures_lite::io::AsyncRead for GlommioStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl futures_lite::io::AsyncWrite for GlommioStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

impl hyper::rt::Read for GlommioStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        // hyper's `ReadBufCursor` and futures-io's `poll_read` take different
        // buffer types, so read through a temporary buffer.
        let this = self.get_mut();
        let mut tmp = vec![0u8; buf.remaining()];
        let n = ready!(Pin::new(&mut this.inner).poll_read(cx, &mut tmp))?;
        buf.put_slice(&tmp[..n]);
        Poll::Ready(Ok(()))
    }
}

impl hyper::rt::Write for GlommioStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

/// A TLS stream over a glommio TCP stream, wrapped for hyper.
pub struct TlsStream {
    inner: futures_rustls::client::TlsStream<GlommioStream>,
}

// SAFETY: see module docs; `TlsStream` is only ever moved within the owning
// glommio executor thread.
unsafe impl Send for TlsStream {}

impl TlsStream {
    pub fn new(inner: futures_rustls::client::TlsStream<GlommioStream>) -> Self {
        Self { inner }
    }
}

impl hyper::rt::Read for TlsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        // SAFETY: `self` is already pinned by hyper; projecting to the `inner`
        // field keeps that pin contract. `inner` is `!Unpin` (it pins its own
        // I/O), so we must use `new_unchecked` rather than `Pin::new`.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        let mut tmp = vec![0u8; buf.remaining()];
        let n = ready!(inner.poll_read(cx, &mut tmp))?;
        buf.put_slice(&tmp[..n]);
        Poll::Ready(Ok(()))
    }
}

impl hyper::rt::Write for TlsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // SAFETY: pinned projection as above.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        inner.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // SAFETY: pinned projection as above.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // SAFETY: pinned projection as above.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        inner.poll_close(cx)
    }
}
