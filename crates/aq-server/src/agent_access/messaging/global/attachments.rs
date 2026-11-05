//! agile-vault-27: the human artifact store for owner-message attachments.
//!
//! An owner uploads bytes first (staged, 24h), then references them from a
//! `Create` by id and digest. Binding pins the row to the owner message, so
//! its retention is the message's retention (`ON DELETE CASCADE`).
use super::*;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use sqlx::Row as _;
pub(super) const MAX_ATTACHMENTS: usize = 4;
pub(super) const MAX_SIZE: usize = 1 << 20;
const MAX_STAGED: i64 = 16;
const MAX_STAGED_BYTES: i64 = 16 << 20;
pub(super) const STAGED_TTL_MS: i64 = 24 * 60 * 60 * 1000;
const TEXT: [&str; 4] = [
    "text/plain",
    "text/markdown",
    "text/csv",
    "application/json",
];
const BINARY: [(&str, &[&[u8]]); 5] = [
    ("application/pdf", &[b"%PDF-"]),
    ("image/png", &[b"\x89PNG\r\n\x1a\n"]),
    ("image/jpeg", &[b"\xff\xd8\xff"]),
    ("image/gif", &[b"GIF87a", b"GIF89a"]),
    ("image/webp", &[b"RIFF"]),
];
const COLUMNS: &str = "attachment_id,conversation_id,uploader_id,name,media_type,size_bytes,digest,created_at_ms,message_id";

/// A pinned reference from `Create.attachments` to a staged upload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AttachmentRef {
    pub(super) id: Uuid,
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub(super) digest: String,
}
pub(super) fn digest_valid(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|h| {
        h.len() == 64
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
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
fn allowed(media_type: &str) -> bool {
    TEXT.contains(&media_type) || BINARY.iter().any(|(t, _)| *t == media_type)
}
fn name_valid(name: &str) -> bool {
    (1..=255).contains(&name.len())
        && name.trim() == name
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
}
fn content_valid(media_type: &str, bytes: &[u8]) -> bool {
    if TEXT.contains(&media_type) {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return false;
        };
        return !text.contains('\0')
            && (media_type != "application/json"
                || serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok());
    }
    BINARY.iter().any(|(t, magic)| {
        *t == media_type
            && magic.iter().any(|m| bytes.starts_with(m))
            && (media_type != "image/webp" || bytes.get(8..12) == Some(b"WEBP"))
    })
}
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UploadQuery {
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
    bytes: Bytes,
) -> Result<Response> {
    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|t| allowed(t))
        .ok_or_else(invalid)?
        .to_owned();
    if bytes.len() > MAX_SIZE {
        return Err(AppError::PayloadTooLarge(
            "attachment exceeds 1048576 bytes".into(),
        ));
    }
    if id.is_nil()
        || attachment.is_nil()
        || bytes.is_empty()
        || !name_valid(&query.name)
        || !content_valid(&media_type, &bytes)
    {
        return Err(invalid());
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
            return Err(conflict());
        }
        tx.commit().await.map_err(db)?;
        return Ok((StatusCode::OK, Json(row.upload())).into_response());
    }
    let (count, total): (i64, i64) = sqlx::query_as("SELECT count(*),COALESCE(sum(size_bytes),0)::bigint FROM global_supervisor_attachments WHERE conversation_id=$1 AND message_id IS NULL")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    if count >= MAX_STAGED || total + descriptor.size_bytes as i64 > MAX_STAGED_BYTES {
        return Err(AppError::Maintenance(
            "staged supervisor attachment capacity exhausted".into(),
        ));
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
    if hash(&bytes) != row.descriptor.digest {
        return Err(unavailable());
    }
    let encoded: String = row
        .descriptor
        .name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let value = |s: &str| HeaderValue::from_str(s).map_err(|_| unavailable());
    let mut response = bytes.into_response();
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, value(&row.descriptor.media_type)?);
    h.insert(
        header::CONTENT_DISPOSITION,
        value(&format!("attachment; filename*=UTF-8''{encoded}"))?,
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    h.insert("x-aq-content-digest", value(&row.descriptor.digest)?);
    Ok(response)
}
async fn fetch(
    tx: &mut Transaction<'_, Postgres>,
    attachment: Uuid,
) -> Result<Option<(Row, Vec<u8>)>> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS},content FROM global_supervisor_attachments WHERE attachment_id=$1"
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
/// `GET .../attachments/{attachment}`: owner download.
pub(super) async fn download(
    State(state): State<AppState>,
    Extension(service): Extension<Arc<Service>>,
    RequireOwner(user): RequireOwner,
    Path((id, attachment)): Path<(Uuid, Uuid)>,
) -> Result<Response> {
    if id.is_nil() || attachment.is_nil() {
        return Err(invalid());
    }
    let at = i64::try_from(stamp(&state)?).map_err(|_| unavailable())?;
    let mut tx = service.tx().await?;
    owner(&mut tx, &user).await?;
    definition(&mut tx, Some(id), false).await?;
    let (row, bytes) = fetch(&mut tx, attachment)
        .await?
        .filter(|(r, _)| r.visible(user.id, id, at))
        .ok_or_else(missing)?;
    tx.commit().await.map_err(db)?;
    content(row, bytes)
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
    let (row, bytes) = fetch(&mut tx, attachment).await?.ok_or_else(missing)?;
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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_checks_follow_the_declared_type() {
        assert!(content_valid("text/plain", b"hello"));
        assert!(!content_valid("text/plain", b"\xff\xfe"));
        assert!(!content_valid("text/plain", b"a\0b"));
        assert!(content_valid("application/json", br#"{"a":1}"#));
        assert!(!content_valid("application/json", b"{"));
        assert!(content_valid("image/png", b"\x89PNG\r\n\x1a\nrest"));
        assert!(!content_valid("image/png", b"GIF89a"));
        assert!(content_valid("image/webp", b"RIFF\0\0\0\0WEBPVP8 "));
        assert!(!content_valid("image/webp", b"RIFF\0\0\0\0WAVEfmt "));
        assert!(!allowed("text/html"));
        assert!(!allowed("text/plain; charset=utf-8"));
    }
    #[test]
    fn names_are_single_safe_path_segments() {
        assert!(name_valid("notes v2.md"));
        for bad in [
            "",
            " a",
            "a ",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a\nb",
            &"x".repeat(256),
        ] {
            assert!(!name_valid(bad), "{bad:?}");
        }
    }
}
