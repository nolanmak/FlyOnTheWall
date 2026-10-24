//! Retry-After is enforced by the actual transport, not only policy metadata.
use fotw_stt::{
    BackoffPolicy, DeepgramEndpoint, DeepgramStream, DeepgramStreamConfig, Source, StreamEvent,
    StreamState,
};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn a_rate_limit_waits_for_retry_after_before_the_next_handshake() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let mut attempts = Vec::new();
        for response in [
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 2\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0; 1];
                socket.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            attempts.push(Instant::now());
            socket.write_all(response.as_bytes()).await.unwrap();
        }
        attempts[1].duration_since(attempts[0])
    });
    let config = DeepgramStreamConfig::new(
        "fixture-not-a-key",
        fotw_stt::deepgram::DeepgramConfig::new("fixture", Source::System),
    )
    .with_endpoint(DeepgramEndpoint::loopback(port))
    .with_backoff(BackoffPolicy::fast());
    let (stream, mut events) = DeepgramStream::open(config);
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            if event == StreamEvent::State(StreamState::Closed) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let elapsed = server.await.unwrap();
    assert!(
        elapsed >= Duration::from_millis(1900),
        "provider was retried too early: {elapsed:?}"
    );
    stream.close().await.unwrap();
}
