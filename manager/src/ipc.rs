//! Local IPC: Unix domain socket on Unix, named pipe on Windows (design §3.1).

use crate::lifecycle;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};

#[cfg(windows)]
use interprocess::local_socket::traits::tokio::{Listener as ListenerTrait, Stream as StreamTrait};

/// Bind the well-known listener for this home.
pub async fn bind(home: &Path) -> io::Result<Listener> {
    #[cfg(unix)]
    {
        let sock = lifecycle::socket_path(home);
        Ok(Listener {
            inner: tokio::net::UnixListener::bind(&sock)?,
        })
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ListenerOptions, ToNsName};
        let ident = lifecycle::windows_pipe_ident(home);
        let name = ident
            .to_ns_name::<GenericNamespaced>()
            .map_err(io::Error::other)?;
        Ok(Listener {
            inner: ListenerOptions::new()
                .name(name)
                .create_tokio()
                .map_err(io::Error::other)?,
        })
    }
}

/// Connect to a running daemon (CLI path; uses interprocess on both OSes).
pub async fn connect(home: &Path) -> io::Result<interprocess::local_socket::tokio::Stream> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::tokio::prelude::*;
        use interprocess::local_socket::tokio::Stream;
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        let sock = lifecycle::socket_path(home);
        let name = sock
            .as_os_str()
            .to_fs_name::<GenericFilePath>()
            .map_err(io::Error::other)?;
        Stream::connect(name).await
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        let ident = lifecycle::windows_pipe_ident(home);
        let name = ident
            .to_ns_name::<GenericNamespaced>()
            .map_err(io::Error::other)?;
        <interprocess::local_socket::tokio::Stream as StreamTrait>::connect(name).await
    }
}

pub struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    inner: interprocess::local_socket::tokio::Listener,
}

impl Listener {
    #[cfg(unix)]
    pub fn from_unix(inner: tokio::net::UnixListener) -> Self {
        Self { inner }
    }

    pub async fn accept(&self) -> io::Result<Incoming> {
        #[cfg(unix)]
        {
            let (stream, _) = self.inner.accept().await?;
            Ok(Incoming { inner: stream })
        }
        #[cfg(windows)]
        {
            let stream = ListenerTrait::accept(&self.inner)
                .await
                .map_err(io::Error::other)?;
            Ok(Incoming { inner: stream })
        }
    }

    #[cfg(unix)]
    pub fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

pub struct Incoming {
    #[cfg(unix)]
    inner: tokio::net::UnixStream,
    #[cfg(windows)]
    inner: interprocess::local_socket::tokio::Stream,
}

impl AsyncRead for Incoming {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Incoming {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
