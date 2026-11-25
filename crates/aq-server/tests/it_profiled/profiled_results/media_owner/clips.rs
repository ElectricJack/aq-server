use super::*;

fn clip_request(id: Uuid, input: &[u8], poster: Uuid) -> Vec<u8> {
    serde_json::to_vec(&media::UploadCreate {
        operation_id: media::Id::new(id).unwrap(),
        input_digest: media::Digest::of(input),
        input_bytes: input.len() as u64,
        filename: "clip.mp4".into(),
        capture: None,
        media: media::MediaKind::Video,
        poster_artifact_id: Some(media::Id::new(poster).unwrap()),
    })
    .unwrap()
}

#[tokio::test]
async fn media_clip_retains_passthrough_mp4_with_poster_preview() {
    // The owner futures are large in debug builds; keep them off the test stack.
    Box::pin(clip_flow()).await
}

async fn clip_flow() {
    let (_f, report, revision, owner, _root, actor) = setup().await;
    let poster = retained(&owner, &actor, report.report_id, &revision).await;
    let input = media::synthetic_mp4(320, 240, 2500);
    let id = Uuid::new_v4();
    let raw = clip_request(id, &input, poster);
    owner
        .reserve(&actor, report.report_id, &revision, &raw)
        .await
        .unwrap();
    owner.put(&actor, id, input.clone()).await.unwrap();
    let status = owner.finalize(&actor, id).await.unwrap();
    assert_eq!(status.state, media::UploadState::Retained, "{status:?}");
    let receipt = status.artifact.unwrap().receipt;
    let clip = receipt.clip.clone().unwrap();
    assert_eq!(clip.duration_ms, 2500);
    assert_eq!(clip.poster_artifact_id.as_uuid(), poster);
    assert_eq!(receipt.variants.render.digest, media::Digest::of(&input));
    let render = owner
        .content(&actor, report.report_id, &revision, id, false)
        .await
        .unwrap();
    assert_eq!(render.bytes(), &input[..]);
    let preview = owner
        .content(&actor, report.report_id, &revision, id, true)
        .await
        .unwrap();
    let poster_preview = owner
        .content(&actor, report.report_id, &revision, poster, true)
        .await
        .unwrap();
    assert_eq!(preview.bytes(), poster_preview.bytes());
    // A poster that is not a retained image on this revision is refused.
    let missing = clip_request(Uuid::new_v4(), &input, Uuid::new_v4());
    assert!(matches!(
        owner
            .reserve(&actor, report.report_id, &revision, &missing)
            .await,
        Err(Error::Invalid)
    ));
    let self_poster = clip_request(Uuid::new_v4(), &input, id);
    assert!(matches!(
        owner
            .reserve(&actor, report.report_id, &revision, &self_poster)
            .await,
        Err(Error::Invalid)
    ));
    // A malformed container rejects as invalid_video.
    let bad = b"\0\0\0\x08moof".to_vec();
    let id = Uuid::new_v4();
    let raw = clip_request(id, &bad, poster);
    owner
        .reserve(&actor, report.report_id, &revision, &raw)
        .await
        .unwrap();
    owner.put(&actor, id, bad).await.unwrap();
    let status = owner.finalize(&actor, id).await.unwrap();
    assert_eq!(status.rejection, Some(media::Rejection::InvalidVideo));
}
