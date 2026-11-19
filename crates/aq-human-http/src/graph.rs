//! Bounded responses from the configured Graph v1 service.
use crate::Error;

pub async fn read(request: reqwest::RequestBuilder, max: usize) -> Result<Vec<u8>, Error> {
    let mut response = request
        .header("X-AQ-Graph-API-Version", "1")
        .send()
        .await
        .map_err(|_| Error::Transport)?;
    if response
        .headers()
        .get("X-AQ-Graph-API-Version")
        .is_none_or(|h| h != "1")
    {
        return Err(Error::Response);
    }
    if response.status() != reqwest::StatusCode::OK {
        return Err(Error::Status(response.status().as_u16()));
    }
    if response.content_length().is_some_and(|n| n > max as u64) {
        return Err(Error::ResponseBound);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
        if bytes.len() + chunk.len() > max {
            return Err(Error::ResponseBound);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
