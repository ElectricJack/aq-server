use super::*;
pub(super) struct ReadRequest {
    pub(super) path: String,
    pub(super) query: Vec<(String, String)>,
    pub(super) max_bytes: usize,
}
impl ReadRequest {
    pub(super) async fn get(
        self,
        runtime: &Runtime,
        clock: &Clock<'_>,
    ) -> Result<(Value, GraphReadStamp, Vec<u8>)> {
        let requested = clock()?;
        let response = runtime
            .client
            .get(
                runtime
                    .url
                    .join(&self.path)
                    .map_err(|_| SchedulingReadError::InvalidRequest)?,
            )
            .bearer_auth(&runtime.token)
            .header("X-AQ-Graph-API-Version", "1")
            .query(&self.query)
            .send()
            .await
            .map_err(|_| SchedulingReadError::Unavailable)?;
        check_headers(&response, self.max_bytes)?;
        let bytes = read_bytes(response, self.max_bytes).await?;
        observation(requested, clock()?, bytes)
    }
}
fn check_headers(response: &reqwest::Response, max_bytes: usize) -> Result<()> {
    if response
        .headers()
        .get("X-AQ-Graph-API-Version")
        .is_none_or(|v| v != "1")
    {
        return Err(SchedulingReadError::InvalidResponse);
    }
    match response.status().as_u16() {
        200 => {}
        401 | 403 => return Err(SchedulingReadError::Forbidden),
        404 => return Err(SchedulingReadError::NotFound),
        _ => return Err(SchedulingReadError::Unavailable),
    }
    if response
        .content_length()
        .is_some_and(|n| n > max_bytes as u64)
    {
        return Err(SchedulingReadError::InvalidResponse);
    }
    Ok(())
}
async fn read_bytes(mut response: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| SchedulingReadError::Unavailable)?
    {
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|n| n > max_bytes)
        {
            return Err(SchedulingReadError::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
fn observation(
    requested: u64,
    received: u64,
    bytes: Vec<u8>,
) -> Result<(Value, GraphReadStamp, Vec<u8>)> {
    let expires = requested
        .checked_add(30_000)
        .ok_or(SchedulingReadError::Unavailable)?;
    if received < requested || received >= expires {
        return Err(SchedulingReadError::Unavailable);
    }
    let value = parse::parse(&bytes).map_err(|_| SchedulingReadError::InvalidResponse)?;
    use sha2::Digest as _;
    let response_digest = hex::encode(sha2::Sha256::digest(&bytes));
    Ok((
        value,
        GraphReadStamp {
            requested_at_ms: requested,
            received_at_ms: received,
            expires_at_ms: expires,
            response_digest,
        },
        bytes,
    ))
}
