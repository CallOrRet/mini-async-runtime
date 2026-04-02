//! Async TCP networking primitives.
//!
//! Provides [`TcpListener`] and [`TcpStream`] that integrate with the
//! runtime's reactor for non-blocking I/O.

use std::future::Future;
use std::io::{self, Read, Write};
use std::net::{self, SocketAddr, ToSocketAddrs};
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::reactor::SharedReactor;

/// Set a file descriptor to non-blocking mode.
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let ret = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// TcpListener
// ---------------------------------------------------------------------------

/// An async TCP listener, analogous to `tokio::net::TcpListener`.
///
/// # Examples
///
/// ```ignore
/// let listener = TcpListener::bind("127.0.0.1:8080").await?;
/// loop {
///     let (stream, addr) = listener.accept().await?;
///     mini_async_runtime::spawn(async move {
///         handle_connection(stream).await;
///     });
/// }
/// ```
pub struct TcpListener {
    inner: net::TcpListener,
    reactor: SharedReactor,
}

impl TcpListener {
    /// Bind to the given address and return an async `TcpListener`.
    ///
    /// The underlying socket is set to non-blocking mode automatically.
    pub(crate) fn bind_with_reactor<A: ToSocketAddrs>(
        addr: A,
        reactor: SharedReactor,
    ) -> io::Result<Self> {
        let listener = net::TcpListener::bind(addr)?;
        set_nonblocking(listener.as_raw_fd())?;
        Ok(TcpListener {
            inner: listener,
            reactor,
        })
    }

    /// Accept a new incoming connection.
    ///
    /// This is an async operation: if no connection is pending, the task
    /// will yield until the reactor signals readability.
    pub fn accept(&self) -> Accept<'_> {
        Accept { listener: self }
    }

    /// Return the local address this listener is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// Future returned by [`TcpListener::accept`].
pub struct Accept<'a> {
    listener: &'a TcpListener,
}

impl<'a> Future for Accept<'a> {
    type Output = io::Result<(TcpStream, SocketAddr)>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.listener.inner.accept() {
            Ok((stream, addr)) => {
                set_nonblocking(stream.as_raw_fd())?;
                let tcp_stream = TcpStream {
                    inner: stream,
                    reactor: self.listener.reactor.clone(),
                };
                Poll::Ready(Ok((tcp_stream, addr)))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                let fd = self.listener.inner.as_raw_fd();
                self.listener
                    .reactor
                    .register_readable(fd, cx.waker().clone())?;
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        let fd = self.inner.as_raw_fd();
        let _ = self.reactor.deregister(fd);
    }
}

// ---------------------------------------------------------------------------
// TcpStream
// ---------------------------------------------------------------------------

/// An async TCP stream, analogous to `tokio::net::TcpStream`.
pub struct TcpStream {
    inner: net::TcpStream,
    reactor: SharedReactor,
}

impl TcpStream {
    /// Connect to a remote address asynchronously.
    #[allow(dead_code)] // Used in tests
    pub(crate) async fn connect_with_reactor<A: ToSocketAddrs>(
        addr: A,
        reactor: SharedReactor,
    ) -> io::Result<Self> {
        // For simplicity, we do a blocking connect and then set non-blocking.
        // A production runtime would use a non-blocking connect + reactor.
        let stream = net::TcpStream::connect(addr)?;
        set_nonblocking(stream.as_raw_fd())?;
        Ok(TcpStream {
            inner: stream,
            reactor,
        })
    }

    /// Read data from the stream into `buf`.
    ///
    /// Returns the number of bytes read, or 0 at EOF.
    pub fn async_read<'a>(&'a mut self, buf: &'a mut [u8]) -> AsyncRead<'a> {
        AsyncRead { stream: self, buf }
    }

    /// Write data to the stream from `buf`.
    ///
    /// Returns the number of bytes written.
    pub fn async_write<'a>(&'a mut self, buf: &'a [u8]) -> AsyncWrite<'a> {
        AsyncWrite { stream: self, buf }
    }

    /// Return the peer's socket address.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.inner.peer_addr()
    }

    /// Return the local socket address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// Future for an async read operation.
pub struct AsyncRead<'a> {
    stream: &'a mut TcpStream,
    buf: &'a mut [u8],
}

impl<'a> Future for AsyncRead<'a> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = &mut *self;
        match me.stream.inner.read(me.buf) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                let fd = me.stream.inner.as_raw_fd();
                me.stream
                    .reactor
                    .register_readable(fd, cx.waker().clone())?;
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

/// Future for an async write operation.
pub struct AsyncWrite<'a> {
    stream: &'a mut TcpStream,
    buf: &'a [u8],
}

impl<'a> Future for AsyncWrite<'a> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = &mut *self;
        match me.stream.inner.write(me.buf) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                let fd = me.stream.inner.as_raw_fd();
                me.stream
                    .reactor
                    .register_writable(fd, cx.waker().clone())?;
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let fd = self.inner.as_raw_fd();
        let _ = self.reactor.deregister(fd);
    }
}
