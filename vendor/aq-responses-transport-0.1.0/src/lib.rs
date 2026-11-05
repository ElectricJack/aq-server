//! Bounded scoped Responses policy/transport only. No launch, admission,
//! credential-installation, process or aggregate cost authority is granted.
use anyhow::{Context, Result, ensure};
use aq_session_protocol::{Digest, ModelSettings, Validate};
use std::{
    net::{Ipv4Addr, TcpListener as StdTcpListener},
    os::fd::AsRawFd,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinHandle,
};
use zeroize::Zeroizing;
mod items;
mod policy;
pub use policy::{MAX_REQUEST_BYTES, ResponsesPolicy};
const MAX_HEAD: usize = 16384;
const MAX_STREAM: usize = 16 * 1024 * 1024;
const MAX_REQUESTS: usize = 64;
const MAX_CREDENTIAL_READS: usize = 8;

#[derive(Clone)]
struct CredentialReader {
    credential: Arc<dyn RetainedUpstreamCredential>,
    admission: Arc<Semaphore>,
}
fn shared_read_admission() -> Arc<Semaphore> {
    static READS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    READS
        .get_or_init(|| Arc::new(Semaphore::new(MAX_CREDENTIAL_READS)))
        .clone()
}

/// An already-authorized retained source. Implementations must perform bounded
/// regular-file/source/original-record checks. Calls run on the blocking pool;
/// a canceled async wait does not forcibly stop an already-started read.
pub trait RetainedUpstreamCredential: Send + Sync {
    fn read_current(&self) -> Result<Zeroizing<Vec<u8>>>;
}

/// Fixed production route; explicit fixture peers exist only with test support.
pub struct UpstreamEndpoint(reqwest::Url);
impl UpstreamEndpoint {
    pub fn openai() -> Self {
        Self(reqwest::Url::parse("https://api.openai.com/v1/responses").expect("fixed URL"))
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn development_loopback(origin: &str) -> Result<Self> {
        let url = reqwest::Url::parse(origin)?;
        ensure!(
            url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")
                && url.port().is_some_and(|p| p > 0)
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "development provider requires numeric IPv4 loopback origin"
        );
        Ok(Self(reqwest::Url::parse(&format!(
            "{}/v1/responses",
            url.as_str().trim_end_matches('/')
        ))?))
    }
}

struct State {
    policy: ResponsesPolicy,
    endpoint: UpstreamEndpoint,
    local_key: Zeroizing<Vec<u8>>,
    upstream_key: Zeroizing<Vec<u8>>,
    credential: CredentialReader,
    client: reqwest::Client,
    authority: String,
    denied: Arc<AtomicBool>,
}

/// Process-local forwarding owner, not a physical stopped proof or grant.
pub struct ScopedResponsesTransport {
    origin: String,
    owner_scope: Digest,
    scope_digest: Digest,
    deadline_ns: u64,
    stop: watch::Sender<bool>,
    denied: Arc<AtomicBool>,
    task: Option<JoinHandle<Result<()>>>,
    capability: Zeroizing<Vec<u8>>,
}
impl ScopedResponsesTransport {
    /// Consumes the exact listening descriptor; never binds a replacement.
    /// Initial credential wait is bounded by the original deadline as well.
    pub async fn start_on_listener(
        listener: StdTcpListener,
        expected_origin: &str,
        policy: ResponsesPolicy,
        endpoint: UpstreamEndpoint,
        credential: Arc<dyn RetainedUpstreamCredential>,
    ) -> Result<Self> {
        Self::start_with_reader(
            listener,
            expected_origin,
            policy,
            endpoint,
            CredentialReader {
                credential,
                admission: shared_read_admission(),
            },
        )
        .await
    }
    async fn start_with_reader(
        listener: StdTcpListener,
        expected_origin: &str,
        policy: ResponsesPolicy,
        endpoint: UpstreamEndpoint,
        credential: CredentialReader,
    ) -> Result<Self> {
        let listener = checked_listener(listener, expected_origin)?;
        let denied = Arc::new(AtomicBool::new(false));
        let upstream_key = read_current(&policy, &credential, &denied).await?;
        let capability = Zeroizing::new(
            format!(
                "aq-responses-{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            )
            .into_bytes(),
        );
        Self::start_inner(
            listener,
            expected_origin,
            policy,
            endpoint,
            capability,
            upstream_key,
            credential,
            denied,
        )
    }
    #[cfg(any(test, feature = "test-support"))]
    pub async fn start_on_listener_with_keys(
        listener: StdTcpListener,
        expected_origin: &str,
        policy: ResponsesPolicy,
        endpoint: UpstreamEndpoint,
        local_key: Zeroizing<Vec<u8>>,
        upstream_key: Zeroizing<Vec<u8>>,
    ) -> Result<Self> {
        let listener = checked_listener(listener, expected_origin)?;
        let credential = CredentialReader {
            credential: Arc::new(FixedCredential(upstream_key.clone())),
            admission: shared_read_admission(),
        };
        Self::start_inner(
            listener,
            expected_origin,
            policy,
            endpoint,
            local_key,
            upstream_key,
            credential,
            Arc::new(AtomicBool::new(false)),
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn start_inner(
        listener: TcpListener,
        origin: &str,
        policy: ResponsesPolicy,
        endpoint: UpstreamEndpoint,
        local_key: Zeroizing<Vec<u8>>,
        upstream_key: Zeroizing<Vec<u8>>,
        credential: CredentialReader,
        denied: Arc<AtomicBool>,
    ) -> Result<Self> {
        policy.remaining()?;
        for key in [&local_key, &upstream_key] {
            ensure!(
                !key.is_empty() && key.len() <= 4096 && key.iter().all(|b| b.is_ascii_graphic()),
                "scoped transport key format"
            );
        }
        ensure!(
            !key_matches(&local_key, &upstream_key),
            "local scoped capability must differ from upstream credential"
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(0)
            .build()?;
        let owner_scope = policy.owner_scope().clone();
        let scope_digest = policy.scope_digest().clone();
        let deadline_ns = policy.original_deadline_boot_ns();
        let capability = local_key.clone();
        let state = State {
            policy,
            endpoint,
            local_key,
            upstream_key,
            credential,
            client,
            authority: origin
                .strip_prefix("http://")
                .expect("checked origin")
                .into(),
            denied: denied.clone(),
        };
        let (stop, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            let serving = async {
                for _ in 0..MAX_REQUESTS {
                    let remaining = check_live(&state.policy, &state.denied)?;
                    let (mut stream, peer) =
                        tokio::time::timeout(remaining, listener.accept()).await??;
                    ensure!(peer.ip().is_loopback(), "nonlocal scoped peer");
                    let work = serve(&state, &mut stream);
                    let _result = tokio::time::timeout(
                        check_live(&state.policy, &state.denied)?.min(Duration::from_secs(120)),
                        work,
                    )
                    .await;
                }
                anyhow::bail!("scoped request count exhausted")
            };
            tokio::select! { biased; _=stopped.changed()=>Ok(()), result=serving=>result }
        });
        Ok(Self {
            origin: origin.into(),
            owner_scope,
            scope_digest,
            deadline_ns,
            stop,
            denied,
            task: Some(task),
            capability,
        })
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn owner_scope(&self) -> &Digest {
        &self.owner_scope
    }
    pub fn scope_digest(&self) -> &Digest {
        &self.scope_digest
    }
    pub fn original_deadline_boot_ns(&self) -> u64 {
        self.deadline_ns
    }
    pub fn local_capability_digest(&self) -> Digest {
        Digest::of(&self.capability)
    }
    /// Sensitive parent-only access for a closed derived-auth publisher.
    pub fn local_capability(&self) -> Zeroizing<Vec<u8>> {
        self.capability.clone()
    }
    /// Triggers denial/shutdown. Await stop before claiming completed closure.
    pub fn cancel(&self) {
        self.denied.store(true, Ordering::SeqCst);
        let _ = self.stop.send(true);
    }
    pub async fn stop(mut self) -> Result<()> {
        self.cancel();
        if let Some(task) = self.task.as_mut() {
            task.await??;
        }
        self.task.take();
        Ok(())
    }
    #[cfg(test)]
    async fn start_with_keys(
        policy: ResponsesPolicy,
        endpoint: UpstreamEndpoint,
        local_key: Zeroizing<Vec<u8>>,
        upstream_key: Zeroizing<Vec<u8>>,
    ) -> Result<Self> {
        let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let origin = format!("http://{}", listener.local_addr()?);
        Self::start_on_listener_with_keys(
            listener,
            &origin,
            policy,
            endpoint,
            local_key,
            upstream_key,
        )
        .await
    }
}
impl Drop for ScopedResponsesTransport {
    fn drop(&mut self) {
        self.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
#[cfg(any(test, feature = "test-support"))]
struct FixedCredential(Zeroizing<Vec<u8>>);
#[cfg(any(test, feature = "test-support"))]
impl RetainedUpstreamCredential for FixedCredential {
    fn read_current(&self) -> Result<Zeroizing<Vec<u8>>> {
        Ok(self.0.clone())
    }
}
fn checked_listener(listener: StdTcpListener, expected_origin: &str) -> Result<TcpListener> {
    let address = listener.local_addr()?;
    ensure!(
        address.ip() == std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
            && address.port() > 0
            && expected_origin == format!("http://{address}"),
        "scoped listener exact numeric IPv4 origin mismatch"
    );
    ensure!(
        socket_option(&listener, libc::SO_TYPE)? == libc::SOCK_STREAM
            && socket_option(&listener, libc::SO_PROTOCOL)? == libc::IPPROTO_TCP,
        "scoped descriptor is not a TCP stream socket"
    );
    ensure!(
        socket_option(&listener, libc::SO_ACCEPTCONN)? == 1,
        "scoped descriptor is not a listening TCP socket"
    );
    listener.set_nonblocking(true)?;
    Ok(TcpListener::from_std(listener)?)
}
fn socket_option(listener: &StdTcpListener, option: libc::c_int) -> Result<libc::c_int> {
    let mut accepting: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&accepting) as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                listener.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&mut accepting as *mut libc::c_int).cast(),
                &mut length,
            )
        } == 0
            && length == std::mem::size_of_val(&accepting) as libc::socklen_t,
        "scoped socket option unavailable"
    );
    Ok(accepting)
}
fn check_live(policy: &ResponsesPolicy, denied: &AtomicBool) -> Result<Duration> {
    ensure!(
        !denied.load(Ordering::SeqCst),
        "scoped forwarding cancelled"
    );
    policy.remaining()
}
async fn read_current(
    policy: &ResponsesPolicy,
    credential: &CredentialReader,
    denied: &Arc<AtomicBool>,
) -> Result<Zeroizing<Vec<u8>>> {
    let remaining = check_live(policy, denied)?;
    // Nonwaiting admission happens before enqueueing any blocking work. The
    // closure owns the permit until actual physical completion, even after an
    // async timeout/cancel drops its JoinHandle and result receiver.
    let permit = credential
        .admission
        .clone()
        .try_acquire_owned()
        .context("scoped credential read capacity unavailable")?;
    let credential = credential.credential.clone();
    let read_denied = denied.clone();
    let deadline = policy.original_deadline_boot_ns();
    let current = tokio::time::timeout(
        remaining,
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            ensure!(
                !read_denied.load(Ordering::SeqCst) && boot_ns()? < deadline,
                "scoped authority lost before credential read"
            );
            credential.read_current()
        }),
    )
    .await???;
    check_live(policy, denied)?;
    ensure!(
        !current.is_empty()
            && current.len() <= 4096
            && current.iter().all(|b| b.is_ascii_graphic()),
        "scoped transport key format"
    );
    Ok(current)
}
fn boot_ns() -> Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    ensure!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } == 0,
        "boot clock unavailable"
    );
    u64::try_from(time.tv_sec)?
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(u64::try_from(time.tv_nsec).ok()?))
        .ok_or_else(|| anyhow::anyhow!("boot clock overflow"))
}

fn key_matches(expected: &[u8], actual: &[u8]) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    expected
        .iter()
        .zip(actual)
        .fold(0u8, |different, (a, b)| different | (a ^ b))
        == 0
}
async fn serve(state: &State, stream: &mut TcpStream) -> Result<()> {
    let mut head = Zeroizing::new(Vec::new());
    loop {
        let byte = tokio::time::timeout(Duration::from_secs(5), stream.read_u8()).await??;
        head.push(byte);
        ensure!(head.len() <= MAX_HEAD, "scoped HTTP header bound");
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&head)?;
    let mut lines = text.split("\r\n");
    ensure!(
        lines.next() == Some("POST /v1/responses HTTP/1.1"),
        "unsupported scoped route/method"
    );
    let mut names = std::collections::BTreeSet::new();
    let mut length = None;
    let mut authorized = false;
    let mut content_type = false;
    let mut host = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').context("invalid scoped HTTP header")?;
        ensure!(
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "scoped header name"
        );
        ensure!(
            value
                .bytes()
                .all(|b| b.is_ascii_graphic() || b == b' ' || b == b'\t'),
            "scoped header value"
        );
        let name = name.to_ascii_lowercase();
        let value = value.trim();
        ensure!(names.insert(name.clone()), "duplicate scoped HTTP header");
        ensure!(
            !matches!(
                name.as_str(),
                "transfer-encoding" | "content-encoding" | "upgrade" | "expect"
            ),
            "unsupported scoped HTTP framing"
        );
        match name.as_str() {
            "content-length" => {
                ensure!(
                    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
                    "scoped content length"
                );
                length = Some(value.parse::<usize>()?);
            }
            "authorization" => {
                authorized = value
                    .strip_prefix("Bearer ")
                    .is_some_and(|key| key_matches(&state.local_key, key.as_bytes()))
            }
            "content-type" => content_type = value == "application/json",
            "host" => host = value == state.authority,
            _ => {}
        }
    }
    ensure!(
        authorized && content_type && host,
        "scoped HTTP authorization/type"
    );
    let length = length.context("scoped content length absent")?;
    ensure!(
        length > 0 && length <= MAX_REQUEST_BYTES,
        "scoped HTTP body bound"
    );
    let mut incoming = Zeroizing::new(vec![0; length]);
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut incoming)).await??;
    let body = state.policy.prepare(&incoming)?;
    {
        let current = read_current(&state.policy, &state.credential, &state.denied).await?;
        ensure!(
            key_matches(&current, &state.upstream_key),
            "scoped original provider material changed"
        );
    }
    check_live(&state.policy, &state.denied)?;
    let mut upstream = state
        .client
        .post(state.endpoint.0.clone())
        .bearer_auth(std::str::from_utf8(&state.upstream_key)?)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(body.to_vec())
        .send()
        .await?;
    ensure!(
        upstream.status() == reqwest::StatusCode::OK
            && upstream
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.split(';').next() == Some("text/event-stream")),
        "unsupported upstream Responses status/type"
    );
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n").await?;
    let mut bytes = 0usize;
    while let Some(chunk) = upstream.chunk().await? {
        check_live(&state.policy, &state.denied)?;
        bytes = bytes
            .checked_add(chunk.len())
            .context("scoped response bytes overflow")?;
        ensure!(bytes <= MAX_STREAM, "scoped Responses stream bound");
        for part in chunk.chunks(16384) {
            if part.is_empty() {
                continue;
            }
            stream
                .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                .await?;
            stream.write_all(part).await?;
            stream.write_all(b"\r\n").await?;
        }
    }
    stream.write_all(b"0\r\n\r\n").await?;
    stream.shutdown().await?;
    Ok(())
}
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod tests;
