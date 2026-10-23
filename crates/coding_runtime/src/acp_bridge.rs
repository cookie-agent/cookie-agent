//! The local sockets an ACP agent's MCP client reaches cookie's tools
//! through (`docs/site/specs/acp-agents.md` section 9).
//!
//! A runtime opens one [`BridgeListener`] per ACP session: a Unix socket in
//! a directory only the runtime's user may enter, or a named pipe on
//! Windows that refuses remote clients. The agent runs
//! `cookie runtime mcp-bridge --socket <path>` as a stdio MCP server, and
//! [`run_bridge`] copies its standard input to the socket and the socket to
//! its standard output, one MCP message per line. The listener is removed
//! when it drops.

use std::{io, path::Path};

use tokio::io::{AsyncRead, AsyncWrite};

/// One connection to a bridge socket.
pub trait BridgeStream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> BridgeStream for T {}

/// A bridge socket the runtime listens on.
pub struct BridgeListener {
    path: String,
    #[cfg(unix)]
    listener: tokio::net::UnixListener,
    /// The private directory the socket lives in, removed on drop.
    #[cfg(unix)]
    directory: std::path::PathBuf,
    /// The pipe instance the next client connects to.
    #[cfg(windows)]
    next: tokio::sync::Mutex<tokio::net::windows::named_pipe::NamedPipeServer>,
}

impl BridgeListener {
    /// Creates a socket with an unguessable name, reachable only by this
    /// user.
    pub fn bind() -> io::Result<Self> {
        let mut bytes = [0_u8; 16];
        getrandom::getrandom(&mut bytes).map_err(io::Error::other)?;
        let name = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            let directory = std::env::temp_dir().join(format!("cookie-acp-{}", &name[..16]));
            std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
            let socket = directory.join("mcp.sock");
            let listener = match tokio::net::UnixListener::bind(&socket) {
                Ok(listener) => listener,
                Err(error) => {
                    let _ = std::fs::remove_dir_all(&directory);
                    return Err(error);
                }
            };
            Ok(Self {
                path: socket.to_string_lossy().into_owned(),
                listener,
                directory,
            })
        }
        #[cfg(windows)]
        {
            let path = format!(r"\\.\pipe\cookie-acp-{name}");
            let first = tokio::net::windows::named_pipe::ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&path)?;
            Ok(Self {
                path,
                next: tokio::sync::Mutex::new(first),
            })
        }
    }

    /// The path [`connect`] takes.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The next client that connects.
    pub async fn accept(&self) -> io::Result<Box<dyn BridgeStream>> {
        #[cfg(unix)]
        {
            let (stream, _) = self.listener.accept().await?;
            Ok(Box::new(stream))
        }
        #[cfg(windows)]
        {
            let mut next = self.next.lock().await;
            next.connect().await?;
            let following = tokio::net::windows::named_pipe::ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.path)?;
            let connected = std::mem::replace(&mut *next, following);
            Ok(Box::new(connected))
        }
    }
}

impl Drop for BridgeListener {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
}

/// Connects to the bridge socket at `path`.
pub async fn connect(path: &Path) -> io::Result<Box<dyn BridgeStream>> {
    #[cfg(unix)]
    {
        Ok(Box::new(tokio::net::UnixStream::connect(path).await?))
    }
    #[cfg(windows)]
    {
        Ok(Box::new(
            tokio::net::windows::named_pipe::ClientOptions::new().open(path)?,
        ))
    }
}

/// `cookie runtime mcp-bridge --socket <path>`: copies standard input to
/// the bridge socket and the socket to standard output until either ends.
pub async fn run_bridge(path: &Path) -> io::Result<()> {
    let stream = connect(path).await?;
    let (mut socket_read, mut socket_write) = tokio::io::split(stream);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let upstream = async {
        tokio::io::copy(&mut stdin, &mut socket_write).await?;
        tokio::io::AsyncWriteExt::shutdown(&mut socket_write).await
    };
    let downstream = async {
        tokio::io::copy(&mut socket_read, &mut stdout).await?;
        tokio::io::AsyncWriteExt::flush(&mut stdout).await
    };
    tokio::select! {
        result = upstream => result,
        result = downstream => result,
    }
}
