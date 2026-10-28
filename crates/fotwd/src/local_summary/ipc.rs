use std::{path::Path, time::Duration};

use http_body_util::{BodyExt, Full, Limited};
use hyper::{Request, body::Bytes, client::conn::http1};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::UnixStream;

const MAX_BODY: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Endpoint {
    Health,
    Tokenize,
    Completion,
    #[cfg(target_os = "macos")]
    ApplyTemplate,
}

impl Endpoint {
    fn path(self) -> &'static str {
        match self {
            Self::Health => "/health",
            Self::Tokenize => "/tokenize",
            Self::Completion => "/completion",
            #[cfg(target_os = "macos")]
            Self::ApplyTemplate => "/apply-template",
        }
    }
}

// A dropped generation must close its real socket, not leave a detached
// Hyper driver consuming a model answer after the policy gate has cancelled.
struct Driver(tokio::task::JoinHandle<()>);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) async fn request(
    socket: &Path,
    pid: u32,
    endpoint: Endpoint,
    body: Option<Value>,
    deadline: Duration,
) -> Result<Value, String> {
    tokio::time::timeout(deadline, async {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|_| "local socket unavailable")?;
        if peer_pid(&stream)? != pid {
            return Err("local runtime ownership mismatch".into());
        }
        let bytes = body
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| "local request encoding failed")?
            .unwrap_or_default();
        if bytes.len() > MAX_BODY {
            return Err("local request exceeds byte limit".into());
        }
        let (mut sender, connection) = http1::Builder::new()
            .max_headers(32)
            .max_buf_size(32 * 1024)
            .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
            .await
            .map_err(|_| "local HTTP handshake failed")?;
        let _driver = Driver(tokio::spawn(async {
            let _ = connection.await;
        }));
        let request = Request::builder()
            .method(if body.is_some() { "POST" } else { "GET" })
            .uri(endpoint.path())
            .header("Host", "fotw-local.invalid")
            .header("Content-Type", "application/json")
            .header("Connection", "close")
            .body(Full::new(Bytes::from(bytes)))
            .map_err(|_| "local request construction failed")?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|_| "local HTTP request failed")?;
        if !response.status().is_success() {
            // Never echo native errors: they can contain prompts or paths.
            return Err(format!(
                "local runtime returned HTTP {}",
                response.status().as_u16()
            ));
        }
        if response
            .headers()
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .is_some_and(|n| n > MAX_BODY as u64)
        {
            return Err("local response exceeds byte limit".into());
        }
        let bytes = Limited::new(response.into_body(), MAX_BODY)
            .collect()
            .await
            .map_err(|_| "local response exceeds limit or is incomplete")?
            .to_bytes();
        serde_json::from_slice(&bytes).map_err(|_| "local response is invalid JSON".into())
    })
    .await
    .map_err(|_| "local runtime request timed out".to_string())?
}

#[cfg(target_os = "macos")]
fn peer_pid(stream: &UnixStream) -> Result<u32, String> {
    use std::os::fd::AsRawFd;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of_val(&pid) as libc::socklen_t;
    // Darwin sys/un.h: SOL_LOCAL=0, LOCAL_PEERPID=2. Check the connected
    // descriptor that Hyper will use, before handing it any prompt bytes.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            0,
            2,
            (&raw mut pid).cast(),
            &raw mut len,
        )
    };
    if result != 0 || len as usize != std::mem::size_of_val(&pid) || pid <= 0 {
        return Err("local runtime ownership could not be verified".into());
    }
    Ok(pid as u32)
}

#[cfg(target_os = "linux")]
fn peer_pid(stream: &UnixStream) -> Result<u32, String> {
    stream
        .peer_cred()
        .ok()
        .and_then(|cred| cred.pid())
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| "local runtime ownership could not be verified".into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peer_pid(_: &UnixStream) -> Result<u32, String> {
    Err("local runtime ownership is unsupported on this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixListener,
    };

    async fn fixture(response: &'static [u8], expected_pid: u32) -> Result<Value, String> {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fixture.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0; 8192];
            let n = stream.read(&mut buf).await.unwrap();
            if n > 0 {
                stream.write_all(response).await.unwrap();
            }
        });
        let result = request(
            &socket,
            expected_pid,
            Endpoint::Health,
            None,
            Duration::from_secs(1),
        )
        .await;
        task.abort();
        result
    }

    #[tokio::test]
    async fn authenticates_the_stream_and_reads_bounded_json() {
        let result = fixture(
            b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}",
            std::process::id(),
        )
        .await;
        assert_eq!(result.unwrap()["status"], "ok");
    }

    #[tokio::test]
    async fn wrong_peer_receives_no_prompt_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fixture.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0; 1];
            stream.read(&mut buf).await.unwrap()
        });
        let result = request(
            &socket,
            std::process::id() + 1,
            Endpoint::Completion,
            Some(serde_json::json!({"prompt":"synthetic secret"})),
            Duration::from_secs(1),
        )
        .await;
        assert!(result.unwrap_err().contains("ownership"));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn refuses_redirects_without_following_or_echoing_body() {
        let result = fixture(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 6\r\n\r\nSECRET", std::process::id()).await;
        let error = result.unwrap_err();
        assert!(error.contains("302"));
        assert!(!error.contains("SECRET"));
    }

    #[tokio::test]
    async fn refuses_oversized_response_before_allocating_it() {
        let result = fixture(
            b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n",
            std::process::id(),
        )
        .await;
        assert!(result.unwrap_err().contains("limit"));
    }

    #[tokio::test]
    async fn dropping_request_closes_the_actual_connection() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fixture.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0; 8192];
            assert!(stream.read(&mut buf).await.unwrap() > 0);
            sent.send(()).unwrap();
            stream.read(&mut buf).await.unwrap()
        });
        let call = tokio::spawn(async move {
            let _dir = dir;
            request(
                &socket,
                std::process::id(),
                Endpoint::Tokenize,
                Some(serde_json::json!({"content":"synthetic"})),
                Duration::from_secs(60),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), received)
            .await
            .expect("request must reach fixture")
            .unwrap();
        call.abort();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}
