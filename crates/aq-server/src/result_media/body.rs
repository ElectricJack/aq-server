//! Each bounded HTTP frame retains the verified Content and
//! admission permit, including after the body is dropped under backpressure.
use super::Content;
use axum::{
    body::{Body, Bytes},
    http::{Response, header},
};
use http_body::{Body as HttpBody, Frame, SizeHint};
use std::{
    convert::Infallible,
    ops::Range,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::OwnedSemaphorePermit;
const FRAME_BYTES: usize = 16_384;

struct Lease {
    content: Content,
    _admission: OwnedSemaphorePermit,
}
struct Chunk {
    lease: Arc<Lease>,
    range: Range<usize>,
}
impl AsRef<[u8]> for Chunk {
    fn as_ref(&self) -> &[u8] {
        &self.lease.content.bytes()[self.range.clone()]
    }
}
struct MediaBody {
    lease: Arc<Lease>,
    offset: usize,
    end: usize,
}
impl HttpBody for MediaBody {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        let length = this.end;
        if this.offset >= length {
            return Poll::Ready(None);
        }
        let end = (this.offset + FRAME_BYTES).min(length);
        let chunk = Chunk {
            lease: this.lease.clone(),
            range: this.offset..end,
        };
        this.offset = end;
        // No full-image copied/queued frame, and no resource release merely
        // because EOF or body cancellation occurred while a frame survives.
        Poll::Ready(Some(Ok(Frame::data(Bytes::from_owner(chunk)))))
    }
    fn is_end_stream(&self) -> bool {
        self.offset >= self.end
    }
    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact((self.end - self.offset) as u64)
    }
}
/// `range` is a validated half-open byte range of a clip render.
pub(super) fn response(
    content: Content,
    admission: OwnedSemaphorePermit,
    range: Option<Range<usize>>,
) -> Response<Body> {
    let descriptor = content.descriptor.clone();
    let receipt = content.receipt_digest.clone();
    let total = content.bytes().len();
    let span = range.clone().unwrap_or(0..total);
    let body = MediaBody {
        lease: Arc::new(Lease {
            content,
            _admission: admission,
        }),
        offset: span.start,
        end: span.end,
    };
    let mut builder = Response::builder()
        .status(if range.is_some() { 206 } else { 200 })
        .header(header::CONTENT_TYPE, descriptor.mime.as_str());
    if descriptor.mime == aq_result_media::Mime::Mp4 {
        builder = builder.header(header::ACCEPT_RANGES, "bytes");
    }
    if range.is_some() {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{total}", span.start, span.end - 1),
        );
    }
    builder
        .header(header::CONTENT_LENGTH, span.len().to_string())
        .header("aq-media-digest", descriptor.digest.as_str())
        .header("aq-media-receipt-digest", receipt.as_str())
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CONTENT_DISPOSITION, "inline")
        .header(header::CONTENT_SECURITY_POLICY, "default-src 'none'")
        .body(Body::new(body))
        .expect("static and validated media response headers")
}
