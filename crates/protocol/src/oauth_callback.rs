//! Client-side loopback listener for MCP OAuth redirects.
//!
//! The daemon may run on another machine, so the browser cannot reach a
//! callback it binds. A client binds this listener on its own loopback, sends
//! [`OAuthCallbackListener::redirect_uri`] in `mcp.auth.begin`, and forwards
//! the redirect it receives to `mcp.auth.complete`; the daemon keeps the PKCE
//! verifier and exchanges the code itself.

use std::io;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const CALLBACK_PATH: &str = "/callback";
const MAX_REQUEST_BYTES: usize = 16 * 1024;

pub struct OAuthCallbackListener {
    listener: TcpListener,
    port: u16,
}

impl OAuthCallbackListener {
    /// Binds an ephemeral port on `127.0.0.1` (RFC 8252 §7.3).
    pub async fn bind() -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    #[must_use]
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{CALLBACK_PATH}", self.port)
    }

    /// Waits for the browser's `GET` on the callback path. Any other request
    /// is answered with a failure page and skipped.
    pub async fn accept(&self) -> io::Result<OAuthCallback> {
        loop {
            let (mut stream, _) = self.listener.accept().await?;
            match read_target(&mut stream).await {
                Some(target) if target.split('?').next() == Some(CALLBACK_PATH) => {
                    return Ok(OAuthCallback {
                        stream,
                        url: format!("http://127.0.0.1:{}{target}", self.port),
                    });
                }
                _ => {
                    let _ = respond(&mut stream, false).await;
                }
            }
        }
    }
}

/// A browser redirect waiting for its page until the daemon has answered.
pub struct OAuthCallback {
    stream: TcpStream,
    url: String,
}

impl OAuthCallback {
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Tells the browser whether the daemon accepted the authorization.
    pub async fn respond(mut self, success: bool) -> io::Result<()> {
        respond(&mut self.stream, success).await
    }
}

async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut request = Vec::new();
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let mut buffer = [0_u8; 1024];
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if request.len() > MAX_REQUEST_BYTES {
            return None;
        }
    }
    let request = std::str::from_utf8(&request).ok()?;
    let (target, _) = request
        .lines()
        .next()?
        .strip_prefix("GET ")?
        .split_once(' ')?;
    target.starts_with('/').then(|| target.to_owned())
}

async fn respond(stream: &mut TcpStream, success: bool) -> io::Result<()> {
    let (status, body) = if success {
        (
            "200 OK",
            "Authorization complete. You can close this window.",
        )
    } else {
        (
            "400 Bad Request",
            "Authorization failed. Return to Cookie Agent and try again.",
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::OAuthCallbackListener;

    async fn get(port: u16, target: &str) -> tokio::net::TcpStream {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .await
            .expect("request");
        stream
    }

    async fn page(mut stream: tokio::net::TcpStream) -> String {
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("response");
        response
    }

    #[tokio::test]
    async fn returns_the_callback_with_the_listener_origin_and_skips_other_paths() {
        let listener = OAuthCallbackListener::bind().await.expect("bind");
        let port = listener.port;
        assert_eq!(
            listener.redirect_uri(),
            format!("http://127.0.0.1:{port}/callback")
        );
        let stray = get(port, "/favicon.ico").await;
        let browser = get(port, "/callback?code=abc&state=xyz").await;
        let callback = listener.accept().await.expect("callback");
        assert!(page(stray).await.starts_with("HTTP/1.1 400"));
        assert_eq!(
            callback.url(),
            format!("http://127.0.0.1:{port}/callback?code=abc&state=xyz")
        );
        callback.respond(true).await.expect("respond");
        assert!(page(browser).await.starts_with("HTTP/1.1 200"));
    }
}
