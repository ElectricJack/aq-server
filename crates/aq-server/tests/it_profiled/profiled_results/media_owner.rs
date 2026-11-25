//! Actual SQL result owner + managed files + real pinned PNG codec. Synthetic
//! launch fixture only; no provider, worker upload, HTTP or capture-success claim.
#[path = "media_owner/clips.rs"]
mod clips;
#[path = "media_owner/concurrency.rs"]
mod concurrency;
#[path = "media_owner/recovery.rs"]
mod recovery;
#[path = "media_owner/retention.rs"]
mod retention;

use super::*;
use aq_result_media as media;
use aq_server::{
    result_media::{Error, Owner},
    store::User,
};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Condvar, Mutex, atomic::AtomicBool};
pub(super) fn png() -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc = !0u32;
        for b in &out[start..] {
            crc ^= u32::from(*b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    0xedb88320 ^ (crc >> 1)
                } else {
                    crc >> 1
                };
            }
        }
        out.extend_from_slice(&(!crc).to_be_bytes());
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0]);
    let plain = [0u8, 10, 20, 30];
    let mut data = vec![0x78, 0x01, 1, 4, 0, 0xfb, 0xff];
    data.extend_from_slice(&plain);
    let (mut a, mut b) = (1u32, 0u32);
    for n in plain {
        a = (a + u32::from(n)) % 65521;
        b = (b + a) % 65521;
    }
    data.extend_from_slice(&((b << 16) | a).to_be_bytes());
    chunk(&mut out, b"IDAT", &data);
    chunk(&mut out, b"IEND", &[]);
    out
}

pub(super) async fn setup() -> (
    Fixture,
    ResultReport,
    String,
    Owner,
    tempfile::TempDir,
    User,
) {
    let (f, _, report) = prepare(true, true).await;
    let auth = context(&f).await;
    let recorded = results::record(&f.state, &auth, &report).await.unwrap();
    let actor = f
        .state
        .store
        .find_user_by_binding("dev", "owner@localhost")
        .await
        .unwrap()
        .unwrap();
    let root = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let owner = Owner::open(f.state.clone(), root.path(), Uuid::new_v4())
        .await
        .unwrap();
    (
        f,
        report,
        recorded.report_digest.as_str().into(),
        owner,
        root,
        actor,
    )
}

pub(super) fn request(id: Uuid) -> Vec<u8> {
    let bytes = png();
    serde_json::to_vec(&media::UploadCreate {
        operation_id: media::Id::new(id).unwrap(),
        input_digest: media::Digest::of(&bytes),
        input_bytes: bytes.len() as u64,
        filename: "human-supplied.png".into(),
        capture: None,
        media: Default::default(),
        poster_artifact_id: None,
    })
    .unwrap()
}

pub(super) async fn retained(owner: &Owner, actor: &User, report: Uuid, revision: &str) -> Uuid {
    let id = Uuid::new_v4();
    owner
        .reserve(actor, report, revision, &request(id))
        .await
        .unwrap();
    owner.put(actor, id, png()).await.unwrap();
    let status = owner.finalize(actor, id).await.unwrap();
    assert_eq!(status.state, media::UploadState::Retained);
    id
}

pub(super) struct Gate {
    open: Mutex<bool>,
    cv: Condvar,
    pub(super) entered: tokio::sync::Notify,
    once: AtomicBool,
}

impl Gate {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            open: Mutex::new(false),
            cv: Condvar::new(),
            entered: tokio::sync::Notify::new(),
            once: AtomicBool::new(false),
        })
    }
    pub(super) fn hook(self: &Arc<Self>, point: &'static str) -> aq_server::result_media::TestHook {
        let gate = self.clone();
        Arc::new(move |name| {
            if name == point && !gate.once.swap(true, Ordering::SeqCst) {
                gate.entered.notify_one();
                let mut open = gate.open.lock().unwrap();
                while !*open {
                    open = gate.cv.wait(open).unwrap();
                }
            }
            Ok(())
        })
    }
    pub(super) fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }
}
