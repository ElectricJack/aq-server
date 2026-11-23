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
        let grant = (
            self.grant.base.as_str(),
            self.grant.task_ref.as_str(),
            self.max_bytes,
        );
        upstream(parts, (self.grant.repository_id, path), body, grant)
    }
}
const READ_GRANT: (&str, &str, u64) = (READ_BASE, READ_TASK_REF, 1);

/// One `/v1/supervisor/git/<repository>/<path>` fetch, which the caller has
/// authorized for `repository`, forwarded to aq-git's git socket under
/// [`READ_GRANT`] within Settings `git_transfer_timeout_ms`. Anything but the
/// two fetch routes is refused; the body passes through unread and unlimited.
pub async fn serve_read<B>(
    pool: &PgPool,
    socket: &Path,
    repository: Uuid,
    path: &str,
    request: Request<B>,
) -> Result<Response<Incoming>, Refusal>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<BoxError>,
{
    let fetch = matches!(
        (request.method().as_str(), path, request.uri().query()),
        ("GET", "info/refs", Some("service=git-upload-pack")) | ("POST", "git-upload-pack", None)
    );
    if !fetch {
        return Err(Refusal::Forbidden("Supervisors can only fetch"));
    }
    let (limits, _) = aq_role_settings::delivery_limits(pool)
        .await
        .map_err(|_| Refusal::Unavailable)?;
    let deadline = Instant::now() + Duration::from_millis(limits.git_transfer_timeout_ms);
    let (parts, body) = request.into_parts();
    let upstream = upstream(parts, (repository, path), body, READ_GRANT)?;
    let mut response = exchange(socket, upstream, deadline).await?;
    filter_headers(&mut response);
    Ok(response)
}
/// The aq-git request at `/<repository>/<path>` with only the [`FORWARDED`]
/// client headers and the grant headers of `(base, task_ref, max_push_bytes)`.
fn upstream<B>(
    parts: http::request::Parts,
    (repository, path): (Uuid, &str),
    body: B,
    (base, task_ref, max_push_bytes): (&str, &str, u64),
) -> Result<Request<B>, Refusal> {
    let mut uri = format!("/{repository}/{path}");
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
        .header("aq-repository", repository.to_string())
        .header("aq-base", base)
        .header("aq-task-ref", task_ref)
        .header("aq-max-push-bytes", max_push_bytes.to_string())
        .header("aq-operation", Uuid::new_v4().to_string());
    out.body(body)
        .map_err(|_| Refusal::BadRequest("invalid git request"))
}
fn filter_headers(response: &mut Response<Incoming>) {
    let headers = std::mem::take(response.headers_mut());
    for name in RETURNED {
        for value in headers.get_all(&name) {
            response.headers_mut().append(&name, value.clone());
        }
    }
}
