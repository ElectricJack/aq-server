//! Bounded, correlated dependency health probes. Never execution authority.
use reqwest::StatusCode;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
pub async fn graph_ready(origin: &str) -> bool {
    let probe = async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(1))
            .build()
            .ok()?;
        let mut response = client
            .get(format!("{}/health/ready", origin.trim_end_matches('/')))
            .send()
            .await
            .ok()?;
        if response.status() != StatusCode::OK {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len() + chunk.len() > 4096 {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        (body["status"] == "ready" && body["api_version"] == "1").then_some(())
    };
    tokio::time::timeout(Duration::from_secs(1), probe)
        .await
        .is_ok_and(|v| v.is_some())
}

pub async fn git_ready(socket: &std::path::Path) -> bool {
    let probe = async {
        let id = uuid::Uuid::new_v4().to_string();
        let request = serde_json::json!({"protocol_version":1,"request_id":id,"operation":"capabilities","params":{}});
        let mut stream = tokio::net::UnixStream::connect(socket).await.ok()?;
        let mut bytes = serde_json::to_vec(&request).ok()?;
        bytes.push(b'\n');
        stream.write_all(&bytes).await.ok()?;
        let mut reader = BufReader::new(stream.take(65_537));
        bytes.clear();
        reader.read_until(b'\n', &mut bytes).await.ok()?;
        if bytes.len() > 65_536 || bytes.last() != Some(&b'\n') {
            return None;
        }
        let body: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        (body["protocol_version"] == 1
            && body["request_id"] == id
            && body["ok"] == true
            && body.get("error").is_none_or(serde_json::Value::is_null)
            && body["result"]["protocol_version"] == 1
            && body["result"]["operations"]
                .as_array()
                .is_some_and(|ops| ops.iter().any(|op| op == "repo.show")))
        .then_some(())
    };
    tokio::time::timeout(Duration::from_secs(1), probe)
        .await
        .is_ok_and(|v| v.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn git_probe_rejects_uncorrelated_malformed_and_stalled_frames() {
        for frame in [Some(b"not-json\n".as_slice()), Some(b"{\"protocol_version\":1,\"request_id\":\"other\",\"ok\":true,\"result\":{\"protocol_version\":1,\"operations\":[\"repo.show\"]}}\n".as_slice()), None] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("git.sock");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let peer = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                if let Some(frame) = frame { stream.write_all(frame).await.unwrap(); }
                else { std::future::pending::<()>().await; }
            });
            assert!(!tokio::time::timeout(Duration::from_secs(2), git_ready(&path)).await.unwrap());
            peer.abort();
        }
    }

    #[tokio::test]
    async fn graph_probe_rejects_false_success_and_stalled_http() {
        for body in [
            Some("{}"),
            Some(r#"{"status":"ready","api_version":"99"}"#),
            None,
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let peer = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                if let Some(body) = body {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                } else {
                    std::future::pending::<()>().await;
                }
            });
            assert!(
                !tokio::time::timeout(Duration::from_secs(2), graph_ready(&origin))
                    .await
                    .unwrap()
            );
            peer.abort();
        }
    }
}
