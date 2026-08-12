//! A hand-rolled RFC6455 WebSocket frame codec.
//!
//! The server never has to send masked frames, and clients must always mask,
//! which keeps the codec small. The TCP stream is shared between the reader
//! and writer tasks on one executor thread through [`SharedStream`], an
//! `Rc<RefCell<_>>` wrapper; the borrow lives only inside each `poll_*` call
//! so concurrent read/write polling on the single thread is safe.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;

/// Reject frames larger than this to bound memory.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;
/// Aggregate fragmented-message cap (continuation frames).
pub const MAX_MESSAGE: usize = 8 * 1024 * 1024;

/// A decoded WebSocket frame payload.
#[derive(Debug)]
pub enum Frame {
    Text(Vec<u8>),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close { code: u16, reason: String },
}

/// A `glommio::net::TcpStream` shared between reader and writer tasks.
///
/// Sound because glommio executors are single-threaded: `poll_read` and
/// `poll_write` take a `RefCell` borrow that is released before returning, and
/// the executor never runs two tasks at once.
#[derive(Clone)]
pub struct SharedStream(Rc<RefCell<glommio::net::TcpStream>>);

impl SharedStream {
    pub fn new(stream: glommio::net::TcpStream) -> Self {
        Self(Rc::new(RefCell::new(stream)))
    }
}

impl AsyncRead for SharedStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut inner = self.get_mut().0.borrow_mut();
        Pin::new(&mut *inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for SharedStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut inner = self.get_mut().0.borrow_mut();
        Pin::new(&mut *inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = self.get_mut().0.borrow_mut();
        Pin::new(&mut *inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = self.get_mut().0.borrow_mut();
        Pin::new(&mut *inner).poll_close(cx)
    }
}

/// Reads WebSocket frames from `io`, reassembling fragmented messages.
pub struct WsReader<S> {
    io: S,
    buf: VecDeque<u8>,
    /// The opcode of the message currently being reassembled, if fragmented.
    frag_opcode: Option<u8>,
    frag: Vec<u8>,
}

impl<S: AsyncRead + Unpin> WsReader<S> {
    /// `initial` bytes already consumed from the socket (e.g. bytes that
    /// arrived immediately after the HTTP upgrade header) are prepended to the
    /// read stream.
    pub fn new(io: S, initial: Vec<u8>) -> Self {
        Self {
            io,
            buf: initial.into(),
            frag_opcode: None,
            frag: Vec::new(),
        }
    }

    /// Read the next complete message. Ping frames are surfaced to the caller
    /// (which should reply with a pong); the caller is responsible for routing
    /// pong/close replies through the writer.
    pub async fn read_frame(&mut self) -> io::Result<Frame> {
        loop {
            let b0 = self.read_u8().await?;
            let b1 = self.read_u8().await?;
            let fin = b0 & 0x80 != 0;
            let opcode = b0 & 0x0f;
            let masked = b1 & 0x80 != 0;
            let mut len = u64::from(b1 & 0x7f);
            if len == 126 {
                len = u64::from(u16::from_be_bytes([
                    self.read_u8().await?,
                    self.read_u8().await?,
                ]));
            } else if len == 127 {
                let mut bytes = [0u8; 8];
                for b in &mut bytes {
                    *b = self.read_u8().await?;
                }
                len = u64::from_be_bytes(bytes);
            }
            if len > MAX_FRAME as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ws frame exceeds maximum size",
                ));
            }
            let mut mask = [0u8; 4];
            if masked {
                for b in &mut mask {
                    *b = self.read_u8().await?;
                }
            }
            let mut payload = self.read_bytes(len as usize).await?;
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
            match opcode {
                OP_PING => return Ok(Frame::Ping(payload)),
                OP_PONG => return Ok(Frame::Pong(payload)),
                OP_CLOSE => {
                    let (code, reason) = if payload.len() >= 2 {
                        (
                            u16::from_be_bytes([payload[0], payload[1]]),
                            String::from_utf8_lossy(&payload[2..]).into_owned(),
                        )
                    } else {
                        (1000, String::new())
                    };
                    return Ok(Frame::Close { code, reason });
                }
                OP_TEXT | OP_BINARY => {
                    if fin {
                        return Ok(if opcode == OP_TEXT {
                            Frame::Text(payload)
                        } else {
                            Frame::Binary(payload)
                        });
                    }
                    self.frag_opcode = Some(opcode);
                    self.frag = payload;
                    if self.frag.len() > MAX_MESSAGE {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "ws fragmented message exceeds maximum size",
                        ));
                    }
                }
                0 => {
                    let is_text = self.frag_opcode == Some(OP_TEXT);
                    if self.frag.len() + payload.len() > MAX_MESSAGE {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "ws fragmented message exceeds maximum size",
                        ));
                    }
                    self.frag.extend_from_slice(&payload);
                    if fin {
                        let out = std::mem::take(&mut self.frag);
                        self.frag_opcode = None;
                        return Ok(if is_text {
                            Frame::Text(out)
                        } else {
                            Frame::Binary(out)
                        });
                    }
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unsupported ws opcode {opcode}"),
                    ));
                }
            }
        }
    }

    async fn read_u8(&mut self) -> io::Result<u8> {
        let b = self.read_bytes(1).await?;
        Ok(b[0])
    }

    /// Read exactly `n` bytes, pulling from the buffered prefix first.
    async fn read_bytes(&mut self, n: usize) -> io::Result<Vec<u8>> {
        while self.buf.len() < n {
            let mut tmp = [0u8; 8192];
            let got = self.io.read(&mut tmp).await?;
            if got == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "ws connection closed",
                ));
            }
            self.buf.extend(tmp[..got].iter().copied());
        }
        let out: Vec<u8> = self.buf.drain(..n).collect();
        Ok(out)
    }
}

/// Writes WebSocket frames to `io`. Server frames are never masked.
pub struct WsWriter<S> {
    io: S,
}

impl<S: AsyncWrite + Unpin> WsWriter<S> {
    pub fn new(io: S) -> Self {
        Self { io }
    }

    pub async fn write_text(&mut self, text: &str) -> io::Result<()> {
        self.write_frame(OP_TEXT, text.as_bytes()).await
    }

    pub async fn write_pong(&mut self, payload: &[u8]) -> io::Result<()> {
        self.write_frame(OP_PONG, payload).await
    }

    pub async fn write_close(&mut self, code: u16, reason: &str) -> io::Result<()> {
        let mut payload = code.to_be_bytes().to_vec();
        payload.extend_from_slice(reason.as_bytes());
        self.write_frame(OP_CLOSE, &payload).await
    }

    pub async fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut head = Vec::with_capacity(14);
        head.push(0x80 | opcode);
        let len = payload.len();
        if len < 126 {
            head.push(len as u8);
        } else if len <= 0xffff {
            head.push(126);
            head.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            head.push(127);
            head.extend_from_slice(&(len as u64).to_be_bytes());
        }
        head.extend_from_slice(payload);
        let mut written = 0;
        while written < head.len() {
            let n = self.io.write(&head[written..]).await?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "ws write stalled"));
            }
            written += n;
        }
        Ok(())
    }
}

/// Compute the `Sec-WebSocket-Accept` value for the given client key.
pub fn accept_key(key: &str) -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    STANDARD.encode(hasher.finalize())
}
