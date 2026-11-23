use super::*;

/// The central grant database and its Git smart-HTTP socket.
pub struct SmartHttp<'a> {
    pub pool: &'a PgPool,
    pub socket: &'a Path,
}
impl SmartHttp<'_> {
    /// One `/v1/git/<repository>/<path>` request of the authenticated `node` for
    /// the attempt its `AQ-Attempt` header names, forwarded to aq-git's git
    /// socket under the attempt's recorded grant.
    pub async fn serve<B>(
        &self,
        node: Uuid,
        (repository, path): (Uuid, &str),
        request: Request<B>,
    ) -> Result<Response<Incoming>, Refusal>
    where
        B: Body<Data = Bytes> + Send + Unpin + 'static,
        B::Error: Into<BoxError>,
    {
        let push = route(request.method(), path, request.uri().query())?;
        let attempt = (request.headers().get(ATTEMPT_HEADER))
            .and_then(|v| Uuid::try_parse(v.to_str().ok()?).ok())
            .ok_or(Refusal::BadRequest(
                "the AQ-Attempt header must name the attempt",
            ))?;
        let grant = live(self.pool, attempt, node)
            .await
            .map_err(|_| Refusal::Unavailable)?;
        let grant = (grant.filter(|g| g.repository_id == repository))
            .ok_or(Refusal::Forbidden("no live git grant for this request"))?;
        let (limits, _) = aq_role_settings::delivery_limits(self.pool)
            .await
            .map_err(|_| Refusal::Unavailable)?;
        let deadline = Instant::now() + Duration::from_millis(limits.git_transfer_timeout_ms);
        let transfer = Transfer {
            grant: &grant,
            deadline,
            max_bytes: limits.max_push_bytes,
        };
        let (parts, body) = request.into_parts();
        let body = transfer.body(body, &parts.headers, push).await?;
        let upstream = transfer.request(parts, path, body)?;
        let mut response = exchange(self.socket, upstream, deadline).await?;
        filter_headers(&mut response);
        Ok(response)
    }
}
struct Transfer<'a> {
    grant: &'a GitGrant,
    deadline: Instant,
    max_bytes: u64,
}
impl Transfer<'_> {
    async fn body<B>(
        &self,
        body: B,
        headers: &http::HeaderMap,
        push: bool,
    ) -> Result<Counted<B>, Refusal>
    where
        B: Body<Data = Bytes> + Unpin,
        B::Error: Into<BoxError>,
    {
        let declared = (headers.get(header::CONTENT_LENGTH))
            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
        if declared.is_some_and(|n| n > self.max_bytes) {
            return Err(Refusal::TooLarge);
        }
        let mut body = Counted {
            prefix: None,
            inner: body,
            left: self.max_bytes,
        };
        if push {
            if headers.contains_key(header::CONTENT_ENCODING) {
                return Err(Refusal::BadRequest(
                    "an encoded push body cannot be checked",
                ));
            }
            let read =
                tokio::time::timeout_at(self.deadline, commands(&mut body, self.grant)).await;
            body.prefix = Some(read.map_err(|_| Refusal::Unavailable)??);
        }
        Ok(body)
    }
    fn request<B>(
        &self,
        parts: http::request::Parts,
        path: &str,
        body: B,
    ) -> Result<Request<B>, Refusal> {
        let mut uri = format!("/{}/{path}", self.grant.repository_id);
        if let Some(query) = parts.uri.query() {
            uri = format!("{uri}?{query}");
        }
        let mut out = Request::builder().method(parts.method).uri(uri);
        for name in FORWARDED {
            for value in parts.headers.get_all(&name) {
                out = out.header(&name, value);
            }
        }
        out = (out.header(header::HOST, "aq-git"))
            .header("aq-repository", self.grant.repository_id.to_string())
            .header("aq-base", &self.grant.base)
            .header("aq-task-ref", &self.grant.task_ref)
            .header("aq-max-push-bytes", self.max_bytes.to_string())
            .header("aq-operation", Uuid::new_v4().to_string());
        out.body(body)
            .map_err(|_| Refusal::BadRequest("invalid git request"))
    }
}
fn filter_headers(response: &mut Response<Incoming>) {
    let headers = std::mem::take(response.headers_mut());
    for name in RETURNED {
        for value in headers.get_all(&name) {
            response.headers_mut().append(&name, value.clone());
        }
    }
}
