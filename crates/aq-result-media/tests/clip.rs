use aq_result_media::*;

fn replace(mut bytes: Vec<u8>, from: &[u8], to: &[u8]) -> Vec<u8> {
    let at = bytes.windows(from.len()).position(|w| w == from).unwrap();
    bytes[at..at + to.len()].copy_from_slice(to);
    bytes
}

#[test]
fn synthetic_clip_preflights_with_cited_header() {
    let clip = synthetic_mp4(320, 240, 1500);
    assert_eq!(
        preflight_mp4(&clip).unwrap(),
        ClipHeader {
            width: 320,
            height: 240,
            duration_ms: 1500
        }
    );
}

#[test]
fn malformed_unsupported_and_oversized_clips_are_refused() {
    let clip = synthetic_mp4(320, 240, 1500);
    // Truncation and trailing garbage are structural failures.
    assert!(matches!(
        preflight_mp4(&clip[..clip.len() - 1]),
        Err(Invalid::MalformedVideo)
    ));
    assert!(matches!(
        preflight_mp4(&[clip.clone(), vec![0, 0, 0]].concat()),
        Err(Invalid::MalformedVideo)
    ));
    // A PNG is not a clip.
    assert!(preflight_mp4(b"\x89PNG\r\n\x1a\n").is_err());
    // Unknown brand, fragmented layout and non-visual track are unsupported.
    assert!(matches!(
        preflight_mp4(&replace(
            clip.clone(),
            b"isom\0\0\x02\0isommp41",
            b"qt  \0\0\x02\0qt  qt  "
        )),
        Err(Invalid::UnsupportedVideo)
    ));
    assert!(matches!(
        preflight_mp4(&replace(clip.clone(), b"mdat", b"moof")),
        Err(Invalid::UnsupportedVideo)
    ));
    assert!(matches!(
        preflight_mp4(&replace(clip.clone(), b"vide", b"soun")),
        Err(Invalid::MalformedVideo)
    ));
    // Duration and dimension bounds.
    assert!(matches!(
        preflight_mp4(&synthetic_mp4(320, 240, 60_001)),
        Err(Invalid::Limit(_))
    ));
    assert!(matches!(
        preflight_mp4(&synthetic_mp4(320, 240, 0)),
        Err(Invalid::MalformedVideo)
    ));
    assert!(preflight_mp4(&synthetic_mp4(9000, 240, 1000)).is_err());
    assert!(matches!(
        preflight_mp4(&vec![0; MAX_CLIP_BYTES + 1]),
        Err(Invalid::Limit(_))
    ));
}
