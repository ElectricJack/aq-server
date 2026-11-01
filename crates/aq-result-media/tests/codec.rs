mod support;
use aq_result_media::*;
use support::*;

#[test]
fn rgba_pixels_alpha_and_separate_content_identities_are_preserved() {
    let pixels = [
        0, 20, 255, 0, 200, 0, 1, 128, 10, 30, 50, 255, 70, 80, 90, 42,
    ];
    let input = png(2, 2, png::ColorType::Rgba, &pixels);
    let prepared = prepare_png(&input).unwrap();
    assert_eq!(prepared.input_digest(), &Digest::of(&input));
    assert_eq!(prepared.input_bytes(), input.len());
    assert_eq!(decode(prepared.render().bytes()), (2, 2, pixels.to_vec()));
    assert_eq!(
        prepared.render().descriptor().digest,
        Digest::of(prepared.render().bytes())
    );
    assert_eq!(
        prepared.preview().descriptor().digest,
        Digest::of(prepared.preview().bytes())
    );
    assert_ne!(
        prepared.input_digest(),
        &prepared.render().descriptor().digest
    );
    // Logical variant roles remain distinct even when small images need no scale.
    assert_eq!(prepared.render().bytes(), prepared.preview().bytes());
    assert_eq!(
        chunks(prepared.render().bytes())
            .iter()
            .map(|c| c.0)
            .collect::<Vec<_>>(),
        [*b"IHDR", *b"sRGB", *b"IDAT", *b"IEND"]
    );
}
#[test]
fn rgb_expansion_is_in_place_and_opaque() {
    let input = png(3, 1, png::ColorType::Rgb, &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
    let p = prepare_png(&input).unwrap();
    assert_eq!(
        decode(p.render().bytes()).2,
        [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255]
    );
}
#[test]
fn rendering_is_repeatable_and_has_a_frozen_golden_digest() {
    let input = png(3, 1, png::ColorType::Rgb, &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
    let first = prepare_png(&input).unwrap();
    let second = prepare_png(&input).unwrap();
    assert_eq!(first.render().bytes(), second.render().bytes());
    assert_eq!(
        first.render().descriptor().digest.as_str(),
        "sha256:94dcc170d8c4497fb8f06973b34d767a86345456952cdcba26b27585bef286bc"
    );
    // Tiny inputs can take the pinned encoder's stored/no-filter fallback.
    // This larger vector exercises actual fixed-Sub filtering/compression.
    let flat = png(32, 32, png::ColorType::Rgba, &vec![17; 4096]);
    let normalized = prepare_png(&flat).unwrap();
    assert_eq!(
        normalized.render().descriptor().digest.as_str(),
        "sha256:5d1264bfe819346b03b74b25d9a9194aa439a099d416c2dfb225892fe9b608c8"
    );
}
#[test]
fn preview_geometry_and_sample_locations_cover_asymmetric_images() {
    for (width, height) in [
        (769, 5),
        (5, 769),
        (400, 400),
        (384, 17),
        (1, 4096),
        (4096, 1),
        (13, 7),
    ] {
        let pixels: Vec<u8> = (0..u64::from(width) * u64::from(height))
            .flat_map(|n| {
                [
                    (n % 251) as u8,
                    (n % 199) as u8,
                    (n % 127) as u8,
                    (n % 253) as u8,
                ]
            })
            .collect();
        let input = png(width, height, png::ColorType::Rgba, &pixels);
        let p = prepare_png(&input).unwrap();
        let (pw, ph, preview) = decode(p.preview().bytes());
        assert_eq!((pw, ph), preview_dimensions(width, height).unwrap());
        assert!(pw.max(ph) <= PREVIEW_EDGE);
        assert!(p.preview().bytes().len() <= MAX_PREVIEW_BYTES);
        for y in 0..ph {
            for x in 0..pw {
                let sx = u64::from(x) * u64::from(width) / u64::from(pw);
                let sy = u64::from(y) * u64::from(height) / u64::from(ph);
                let src = ((sy * u64::from(width) + sx) * 4) as usize;
                let dst = ((u64::from(y) * u64::from(pw) + u64::from(x)) * 4) as usize;
                assert_eq!(&preview[dst..dst + 4], &pixels[src..src + 4]);
            }
        }
    }
}
#[test]
fn maximum_full_raster_and_checked_geometry_boundaries() {
    assert_eq!(rgba_len(2000, 1000).unwrap(), MAX_RGBA_BYTES);
    for (w, h) in [
        (0, 1),
        (1, 0),
        (4097, 1),
        (1, u32::MAX),
        (u32::MAX, u32::MAX),
        (2001, 1000),
    ] {
        assert!(rgba_len(w, h).is_err());
        let mut bad = b"\x89PNG\r\n\x1a\n".to_vec();
        bad.extend(header(w, h, 6));
        bad.extend(chunk(b"IDAT", &[1]));
        bad.extend(chunk(b"IEND", &[]));
        assert!(prepare_png(&bad).is_err());
    }
    let pixels = vec![17; MAX_RGBA_BYTES];
    let input = png(2000, 1000, png::ColorType::Rgba, &pixels);
    assert!(input.len() < MAX_INPUT_BYTES);
    drop(pixels);
    let p = prepare_png(&input).unwrap();
    assert_eq!(
        (
            p.render().descriptor().width,
            p.render().descriptor().height
        ),
        (2000, 1000)
    );
    assert!(p.render().bytes().len() <= MAX_RENDER_BYTES);
}
#[test]
fn signature_truncation_crc_trailing_and_chunk_order_are_not_images() {
    let good = png(1, 1, png::ColorType::Rgba, &[0, 0, 0, 255]);
    for length in 0..good.len() {
        assert!(prepare_png(&good[..length]).is_err(), "prefix {length}");
    }
    let mut bad = good.clone();
    bad[20] ^= 1;
    assert_eq!(preflight_png(&bad), Err(Invalid::MalformedPng));
    let mut bad = good.clone();
    bad.push(0);
    assert!(preflight_png(&bad).is_err());
    let mut list = chunks(&good);
    list.insert(1, list[0].clone());
    assert!(preflight_png(&rebuild(&list)).is_err());
    let mut list = chunks(&good);
    list.swap(0, 1);
    assert!(preflight_png(&rebuild(&list)).is_err());
    assert_eq!(
        prepare_png(&vec![0; MAX_INPUT_BYTES + 1]).unwrap_err(),
        Invalid::Limit("input bytes")
    );
}
#[test]
fn apng_profiles_text_palettes_interlace_and_chunk_flood_are_rejected_before_decode() {
    let good = png(1, 1, png::ColorType::Rgba, &[0, 0, 0, 255]);
    for kind in [
        *b"acTL", *b"fcTL", *b"fdAT", *b"PLTE", *b"iCCP", *b"gAMA", *b"tEXt", *b"zTXt", *b"iTXt",
        *b"cICP",
    ] {
        let mut list = chunks(&good);
        list.insert(1, (kind, vec![0; 8]));
        assert_eq!(preflight_png(&rebuild(&list)), Err(Invalid::UnsupportedPng));
    }
    let mut list = chunks(&good);
    list[0].1[12] = 1;
    assert_eq!(preflight_png(&rebuild(&list)), Err(Invalid::UnsupportedPng));
    let mut list = chunks(&good);
    for _ in 0..MAX_PNG_CHUNKS {
        list.insert(1, (*b"IDAT", vec![]));
    }
    assert_eq!(
        preflight_png(&rebuild(&list)),
        Err(Invalid::Limit("PNG chunks"))
    );
}
#[test]
fn malformed_deflate_and_adler_fail_after_structural_preflight() {
    let good = png(1, 1, png::ColorType::Rgba, &[0, 0, 0, 255]);
    for checksum in [false, true] {
        let mut list = chunks(&good);
        let data = &mut list.iter_mut().find(|c| c.0 == *b"IDAT").unwrap().1;
        if checksum {
            *data.last_mut().unwrap() ^= 1;
        } else {
            data[0] = 0;
        }
        let bad = rebuild(&list);
        assert!(preflight_png(&bad).is_ok());
        assert_eq!(prepare_png(&bad).unwrap_err(), Invalid::MalformedPng);
    }
}
#[test]
fn surplus_decompressed_image_data_is_rejected() {
    let good = png(2, 2, png::ColorType::Rgba, &[17; 16]);
    let mut list = chunks(&good);
    list[0].1[..8].copy_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1]);
    let bad = rebuild(&list);
    assert!(preflight_png(&bad).is_ok());
    assert!(prepare_png(&bad).is_err());
}
#[test]
fn concatenated_or_trailing_compressed_streams_are_rejected() {
    let good = png(1, 1, png::ColorType::Rgba, &[0, 0, 0, 255]);
    let stream = chunks(&good)
        .into_iter()
        .find(|c| c.0 == *b"IDAT")
        .unwrap()
        .1;
    for suffix in [stream, vec![0]] {
        let mut list = chunks(&good);
        list.iter_mut()
            .find(|c| c.0 == *b"IDAT")
            .unwrap()
            .1
            .extend(suffix);
        let bad = rebuild(&list);
        assert!(preflight_png(&bad).is_ok());
        assert_eq!(prepare_png(&bad).unwrap_err(), Invalid::MalformedPng);
    }
}
#[test]
fn consecutive_one_byte_idat_chunks_keep_pixels_and_variant_identity() {
    let good = png(1, 1, png::ColorType::Rgba, &[1, 2, 3, 255]);
    let original = prepare_png(&good).unwrap();
    let mut parts = Vec::new();
    for (kind, data) in chunks(&good) {
        if kind == *b"IDAT" {
            for byte in data {
                parts.push((kind, vec![byte]));
                parts.push((kind, vec![]));
            }
        } else {
            parts.push((kind, data));
        }
    }
    let split = rebuild(&parts);
    let normalized = prepare_png(&split).unwrap();
    assert_ne!(Digest::of(&split), Digest::of(&good));
    assert_eq!(normalized.render().bytes(), original.render().bytes());
}
#[test]
fn patched_direct_decoder_rejects_upstream_malformed_palette_without_panic() {
    // https://github.com/image-rs/image-png/issues/721; fix8330c2c6fc91c56fb8c4bb0cd6ea65994027f15e.
    let mut bad = b"\x89PNG\r\n\x1a\n".to_vec();
    bad.extend(header(1, 1, 3));
    bad.extend(chunk(b"PLTE", &[255, 0, 0, 7]));
    bad.extend(chunk(
        b"IDAT",
        &[0x78, 0x01, 0x01, 0x02, 0, 0xfd, 0xff, 0, 0, 0, 2, 0, 1],
    ));
    bad.extend(chunk(b"IEND", &[]));
    assert_eq!(preflight_png(&bad), Err(Invalid::UnsupportedPng));
    let decoded = std::panic::catch_unwind(|| {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(&bad));
        decoder.set_transformations(png::Transformations::EXPAND);
        decoder.read_info()
    });
    assert!(decoded.is_ok());
    assert!(decoded.unwrap().is_err());
}
#[test]
fn bounded_structural_mutation_corpus_never_panics() {
    let input = png(3, 2, png::ColorType::Rgba, &[127; 24]);
    for offset in 0..input.len() {
        for bit in [1, 128] {
            let mut candidate = input.clone();
            candidate[offset] ^= bit;
            assert!(std::panic::catch_unwind(|| prepare_png(&candidate)).is_ok());
        }
    }
}
