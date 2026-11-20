//! agile-vault-27: the human artifact store for owner-message attachments.
//!
//! An owner uploads bytes first (staged, 24h), then references them from a
//! `Create` by id and digest. Binding pins the row to the owner message, so
//! its retention is the message's retention (`ON DELETE CASCADE`).
use super::*;
use aq_supervisor_control::attachments::{
    MAX_STAGED_BYTES, MAX_STAGED_FILES, Refusal, STAGED_TTL_MS, allowed, content_valid,
    digest_valid, name_valid, serve,
};
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderMap, StatusCode, header};
use sqlx::Row as _;
pub(super) use aq_supervisor_control::attachments::{
    MAX_FILE_BYTES as MAX_SIZE, MAX_PER_MESSAGE as MAX_ATTACHMENTS,
};
const COLUMNS: &str = "attachment_id,conversation_id,uploader_id,name,media_type,size_bytes,digest,created_at_ms,message_id";

/// A pinned reference from `Create.attachments` to a staged upload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AttachmentRef {
    pub(super) id: Uuid,
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub(super) digest: String,
}
pub(super) fn refs_valid(refs: &[AttachmentRef]) -> bool {
    refs.len() <= MAX_ATTACHMENTS
        && refs.iter().enumerate().all(|(i, a)| {
            !a.id.is_nil() && digest_valid(&a.digest) && refs[..i].iter().all(|b| b.id != a.id)
        })
}
/// The served metadata of one stored file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Descriptor {
    pub(super) id: Uuid,
    pub(super) name: String,
    pub(super) media_type: String,
    pub(super) size_bytes: u64,
    pub(super) digest: String,
}
impl Descriptor {
    fn valid(&self) -> bool {
        !self.id.is_nil()
            && name_valid(&self.name)
            && allowed(&self.media_type)
            && (1..=MAX_SIZE as u64).contains(&self.size_bytes)
            && digest_valid(&self.digest)
    }
    /// One stable line for the supervisor turn text. The name is JSON-quoted
    /// so owner text cannot forge a line break or a closing bracket.
    pub(super) fn render(&self) -> String {
        format!(
            "{} name {} {} {} bytes {}",
            self.id,
            json!(self.name),
            self.media_type,
            self.size_bytes,
            self.digest
        )
    }
}
/// Decodes the stored descriptors of one message, refusing anything the
/// writer could not produce.
pub(super) fn stored(raw: Option<Value>) -> Result<Vec<Descriptor>> {
    let Some(raw) = raw else {
        return Ok(vec![]);
    };
    let items: Vec<Descriptor> = serde_json::from_value(raw).map_err(|_| unavailable())?;
    if items.is_empty() || items.len() > MAX_ATTACHMENTS || !items.iter().all(Descriptor::valid) {
        return Err(unavailable());
    }
    Ok(items)
}
/// Per-message descriptors ordered by binding ordinal, for a row aliased `m`.
pub(super) const DESCRIPTORS: &str = "(SELECT jsonb_agg(jsonb_build_object('id',a.attachment_id,'name',a.name,'media_type',a.media_type,'size_bytes',a.size_bytes,'digest',a.digest) ORDER BY a.ordinal) FROM global_supervisor_attachments a WHERE a.message_id=m.message_id)";
#[derive(Serialize)]
pub(super) struct Upload {
    #[serde(flatten)]
    descriptor: Descriptor,
    conversation_id: Uuid,
    created_at_ms: u64,
    state: &'static str,
    message_id: Option<Uuid>,
    expires_at_ms: Option<u64>,
}
struct Row {
    descriptor: Descriptor,
    conversation_id: Uuid,
    uploader_id: Uuid,
    created_at_ms: i64,
    message_id: Option<Uuid>,
}
impl Row {
    fn decode(r: sqlx::postgres::PgRow) -> Result<Self> {
        let descriptor = Descriptor {
            id: r.get("attachment_id"),
            name: r.get("name"),
            media_type: r.get("media_type"),
            size_bytes: u64::try_from(r.get::<i64, _>("size_bytes")).map_err(|_| unavailable())?,
            digest: r.get("digest"),
        };
        let created_at_ms: i64 = r.get("created_at_ms");
        if !descriptor.valid() || created_at_ms <= 0 {
            return Err(unavailable());
        }
        Ok(Self {
            descriptor,
            conversation_id: r.get("conversation_id"),
            uploader_id: r.get("uploader_id"),
            created_at_ms,
            message_id: r.get("message_id"),
        })
    }
    fn upload(self) -> Upload {
        Upload {
            conversation_id: self.conversation_id,
            created_at_ms: self.created_at_ms as u64,
            state: if self.message_id.is_some() {
                "attached"
            } else {
                "staged"
            },
            expires_at_ms: self
                .message_id
                .is_none()
                .then(|| (self.created_at_ms + STAGED_TTL_MS) as u64),
            message_id: self.message_id,
            descriptor: self.descriptor,
        }
    }
    /// Staged rows are usable by their uploader until they expire; bound rows
    /// are readable by any current owner for the message's lifetime.
    fn visible(&self, user: Uuid, conversation: Uuid, at: i64) -> bool {
        self.conversation_id == conversation
            && (self.message_id.is_some()
                || (self.uploader_id == user && self.created_at_ms > at - STAGED_TTL_MS))
    }
}
/// Owner routes answer with a typed [`Refusal`] or the shared [`AppError`].
pub(super) enum Refused {
    App(AppError),
    File(Refusal),
}
impl From<AppError> for Refused {
    fn from(e: AppError) -> Self {
        Self::App(e)
    }
}
impl From<Refusal> for Refused {
    fn from(r: Refusal) -> Self {
        Self::File(r)
    }
}
impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        match self {
            Self::App(e) => e.into_response(),
            Self::File(r) => r.into_response(),
        }
    }
}
type Reply = std::result::Result<Response, Refused>;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UploadQuery {
    #[serde(default)]
    name: String,
}
/// `PUT .../attachments/{attachment}?name=`: stage one file. Idempotent for an
/// identical retry; any other reuse of the id is a conflict.
pub(super) async fn upload(
    State(state): State<AppState>,
    Extension(service): Extension<Arc<Service>>,
    RequireOwner(user): RequireOwner,
    Path((id, attachment)): Path<(Uuid, Uuid)>,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    bytes: std::result::Result<Bytes, BytesRejection>,
) -> Reply {
    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|t| allowed(t))
        .ok_or(Refusal::MediaTypeUnsupported)?
        .to_owned();
    let bytes = bytes.map_err(|r| match r.status() {
        StatusCode::PAYLOAD_TOO_LARGE => Refused::File(Refusal::TooLarge),
        _ => Refused::App(invalid()),
    })?;
    if bytes.len() > MAX_SIZE {
        return Err(Refusal::TooLarge.into());
    }
    if id.is_nil() || attachment.is_nil() {
        return Err(invalid().into());
    }
    if !name_valid(&query.name) {
        return Err(Refusal::NameInvalid.into());
    }
    if !content_valid(&media_type, &bytes) {
        return Err(Refusal::Unreadable.into());
    }
    let descriptor = Descriptor {
        id: attachment,
        name: query.name,
        media_type,
        size_bytes: bytes.len() as u64,
        digest: hash(&bytes),
    };
    let at = i64::try_from(stamp(&state)?).map_err(|_| unavailable())?;
    let mut tx = service.tx().await?;
    owner(&mut tx, &user).await?;
    // The conversation lock serializes the staged quota below.
    definition(&mut tx, Some(id), true).await?;
    sqlx::query("DELETE FROM global_supervisor_attachments WHERE conversation_id=$1 AND message_id IS NULL AND created_at_ms<=$2")
        .bind(id)
        .bind(at - STAGED_TTL_MS)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let existing = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM global_supervisor_attachments WHERE attachment_id=$1"
    ))
    .bind(attachment)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;
    if let Some(row) = existing {
        let row = Row::decode(row)?;
        if row.uploader_id != user.id || row.conversation_id != id || row.descriptor != descriptor {
            return Err(Refusal::IdConflict.into());
        }
        tx.commit().await.map_err(db)?;
        return Ok((StatusCode::OK, Json(row.upload())).into_response());
    }
    let (count, total): (i64, i64) = sqlx::query_as("SELECT count(*),COALESCE(sum(size_bytes),0)::bigint FROM global_supervisor_attachments WHERE conversation_id=$1 AND message_id IS NULL")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    if count >= MAX_STAGED_FILES || total + descriptor.size_bytes as i64 > MAX_STAGED_BYTES {
        return Err(Refusal::CapacityExhausted.into());
    }
    sqlx::query("INSERT INTO global_supervisor_attachments(attachment_id,conversation_id,uploader_id,name,media_type,size_bytes,digest,content,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(attachment)
        .bind(id)
        .bind(user.id)
        .bind(&descriptor.name)
        .bind(&descriptor.media_type)
        .bind(descriptor.size_bytes as i64)
        .bind(&descriptor.digest)
        .bind(bytes.as_ref())
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    let row = Row {
        descriptor,
        conversation_id: id,
        uploader_id: user.id,
        created_at_ms: at,
        message_id: None,
    };
    Ok((StatusCode::CREATED, Json(row.upload())).into_response())
}
/// Bytes are served as an inert download whatever their declared type.
fn content(row: Row, bytes: Vec<u8>) -> Result<Response> {
    let d = row.descriptor;
    let ok = hash(&bytes) == d.digest;
    ok.then(|| serve(&d.name, &d.media_type, &d.digest, bytes))
        .flatten()
        .ok_or_else(unavailable)
}
/// One stored file; `bytes` false reads the metadata only (empty content).
async fn fetch(
    tx: &mut Transaction<'_, Postgres>,
    attachment: Uuid,
    bytes: bool,
) -> Result<Option<(Row, Vec<u8>)>> {
    let content = ["''::bytea AS content", "content"][bytes as usize];
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS},{content} FROM global_supervisor_attachments WHERE attachment_id=$1"
    ))
    .bind(attachment)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?;
    row.map(|r| {
        let bytes: Vec<u8> = r.get("content");
        Ok((Row::decode(r)?, bytes))
    })
    .transpose()
}
/// A file the current owner may read: a staged upload of their own that has
/// not expired, or a file bound to a message of this conversation.
async fn readable(
    state: &AppState,
    service: &Service,
    user: &User,
    (id, attachment): (Uuid, Uuid),
    bytes: bool,
) -> std::result::Result<(Row, Vec<u8>), Refused> {
    if id.is_nil() || attachment.is_nil() {
        return Err(invalid().into());
    }
    let at = i64::try_from(stamp(state)?).map_err(|_| unavailable())?;
    let mut tx = service.tx().await?;
    owner(&mut tx, user).await?;
    definition(&mut tx, Some(id), false).await?;
    let file = fetch(&mut tx, attachment, bytes)
        .await?
        .filter(|(r, _)| r.visible(user.id, id, at))
        .ok_or(Refusal::NotFound)?;
    tx.commit().await.map_err(db)?;
    Ok(file)
}
/// `GET .../attachments/{attachment}`: owner download.
pub(super) async fn download(
    State(state): State<AppState>,
    Extension(service): Extension<Arc<Service>>,
    RequireOwner(user): RequireOwner,
    Path(ids): Path<(Uuid, Uuid)>,
) -> Reply {
    let (row, bytes) = readable(&state, &service, &user, ids, true).await?;
    Ok(content(row, bytes)?)
}
/// `GET .../attachments/{attachment}/metadata`: the `AttachmentUpload` state.
pub(super) async fn metadata(
    State(state): State<AppState>,
    Extension(service): Extension<Arc<Service>>,
    RequireOwner(user): RequireOwner,
    Path(ids): Path<(Uuid, Uuid)>,
) -> Reply {
    let (row, _) = readable(&state, &service, &user, ids, false).await?;
    Ok(Json(row.upload()).into_response())
}
/// `DELETE .../attachments/{attachment}`: the uploader withdraws a staged file.
/// A bound file is never deleted here; it lives as long as its message.
pub(super) async fn unstage(
    State(state): State<AppState>,
    Extension(service): Extension<Arc<Service>>,
    RequireOwner(user): RequireOwner,
    Path((id, attachment)): Path<(Uuid, Uuid)>,
) -> Reply {
    if id.is_nil() || attachment.is_nil() {
        return Err(invalid().into());
    }
    let at = i64::try_from(stamp(&state)?).map_err(|_| unavailable())?;
    let mut tx = service.tx().await?;
    owner(&mut tx, &user).await?;
    definition(&mut tx, Some(id), true).await?;
    // The row lock re-checks `message_id` against a concurrent Create's claim.
    let deleted = sqlx::query("DELETE FROM global_supervisor_attachments WHERE attachment_id=$1 AND conversation_id=$2 AND uploader_id=$3 AND message_id IS NULL AND created_at_ms>$4")
        .bind(attachment)
        .bind(id)
        .bind(user.id)
        .bind(at - STAGED_TTL_MS)
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();
    if deleted == 0 {
        let bound = fetch(&mut tx, attachment, false)
            .await?
            .is_some_and(|(r, _)| r.message_id.is_some() && r.visible(user.id, id, at));
        return Err([Refusal::NotFound, Refusal::Bound][bound as usize].into());
    }
    tx.commit().await.map_err(db)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
/// `GET /v1/supervisor/conversation/attachments/{attachment}`: the central
/// supervisor reads a bound file only when its message was dispatched to it.
pub(super) async fn supervisor_download(
    State(state): State<AppState>,
    Extension(service): Extension<Arc<Service>>,
    headers: HeaderMap,
    Path(attachment): Path<Uuid>,
) -> Result<Response> {
    let at = stamp(&state)?;
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(reply::rejected)?;
    let mut tx = service.tx().await?;
    let principal =
        reply::authenticate_in_transaction(&service.pool, &mut tx, bearer, "messaging", at).await?;
    let dispatched: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM global_supervisor_attachments a JOIN central_supervisor_mail_members b ON b.message_id=a.message_id JOIN central_supervisor_dispatches d ON d.actor_id=b.actor_id AND d.reservation_id=b.reservation_id WHERE a.attachment_id=$1 AND b.actor_id=$2)")
        .bind(attachment)
        .bind(principal.actor_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    if !dispatched {
        return Err(missing());
    }
    let (row, bytes) = fetch(&mut tx, attachment, true).await?.ok_or_else(missing)?;
    tx.commit().await.map_err(db)?;
    content(row, bytes)
}
/// Binds the referenced staged uploads to a new owner message inside the
/// enqueue transaction. `None` when any ref is not a live staged upload of
/// this owner and conversation with the pinned digest.
pub(super) async fn claim(
    tx: &mut Transaction<'_, Postgres>,
    refs: &[AttachmentRef],
    user: Uuid,
    conversation: Uuid,
    at: i64,
) -> Result<Option<i64>> {
    let mut total = 0;
    for r in refs {
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM global_supervisor_attachments WHERE attachment_id=$1 FOR UPDATE"
        ))
        .bind(r.id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
        let Some(row) = row.map(Row::decode).transpose()? else {
            return Ok(None);
        };
        if row.message_id.is_some()
            || row.uploader_id != user
            || !row.visible(user, conversation, at)
            || row.descriptor.digest != r.digest
        {
            return Ok(None);
        }
        total += row.descriptor.size_bytes as i64;
    }
    Ok(Some(total))
}
pub(super) async fn bind(
    tx: &mut Transaction<'_, Postgres>,
    refs: &[AttachmentRef],
    message: Uuid,
) -> Result<()> {
    for (ordinal, r) in refs.iter().enumerate() {
        sqlx::query("UPDATE global_supervisor_attachments SET message_id=$1,ordinal=$2 WHERE attachment_id=$3 AND message_id IS NULL")
            .bind(message)
            .bind(ordinal as i16)
            .bind(r.id)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
    }
    Ok(())
}
