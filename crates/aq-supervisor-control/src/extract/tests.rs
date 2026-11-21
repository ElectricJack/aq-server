use super::*;
use crate::attachments::Descriptor;
use flate2::{Compression, Crc, write::DeflateEncoder, write::ZlibEncoder};
use std::io::Write;

fn describe(media_type: &str, bytes: &[u8]) -> Descriptor {
    Descriptor {
        id: uuid::Uuid::from_u128(7),
        name: "fixture".into(),
        media_type: media_type.into(),
        size_bytes: bytes.len() as u64,
        digest: format!("sha256:{}", hex::encode(Sha256::digest(bytes))),
    }
}
fn run(media_type: &str, bytes: &[u8]) -> Extraction {
    extract(&describe(media_type, bytes), bytes, &Limits::default())
}
fn text_of(media_type: &str, bytes: &[u8]) -> String {
    let e = run(media_type, bytes);
    assert_eq!(e.state, State::Extracted, "{}", e.message);
    e.text.unwrap()
}
fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

// ---- text ----
#[test]
fn text_and_markdown_decode_completely_and_keep_their_source() {
    let md = "# Plan\r\n\r\n- é ünï\r\n- 日本語\r\n";
    let e = run("text/markdown", md.as_bytes());
    assert_eq!(e.state, State::Extracted);
    assert_eq!(e.text.as_deref(), Some("# Plan\n\n- é ünï\n- 日本語\n"));
    assert_eq!(e.source, describe("text/markdown", md.as_bytes()));
    assert_eq!(text_of("text/plain", b"\xef\xbb\xbfhello"), "hello");
    let utf16: Vec<u8> = [0xff, 0xfe]
        .into_iter()
        .chain("hi é".encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    assert_eq!(text_of("text/plain", &utf16), "hi é");
}
#[test]
fn text_decoding_failures_are_explicit() {
    let bad = run("text/plain", b"ok \xff\xfe bad");
    assert_eq!(bad.state, State::Unreadable);
    assert!(bad.message.contains("UTF-8") && bad.message.contains("offset 3"));
    assert!(bad.text.is_none());
    for (bytes, state) in [
        (&b"a\0b"[..], State::Unreadable),
        (&b"\xff\xfeA"[..], State::Unreadable),
        (&b"  \n\t "[..], State::NoExtractableText),
        (&b""[..], State::NoExtractableText),
    ] {
        assert_eq!(run("text/plain", bytes).state, state, "{bytes:?}");
    }
}

// ---- limits and source integrity ----
#[test]
fn oversized_text_is_refused_not_truncated() {
    let body = "word ".repeat(100);
    let tight = Limits {
        max_text_bytes: 100,
        ..Limits::default()
    };
    let e = extract(
        &describe("text/plain", body.as_bytes()),
        body.as_bytes(),
        &tight,
    );
    assert_eq!(e.state, State::LimitExceeded);
    assert!(e.text.is_none() && e.message.contains("500") && e.message.contains("100"));
    let exact = Limits {
        max_text_bytes: body.len(),
        ..Limits::default()
    };
    let e = extract(
        &describe("text/plain", body.as_bytes()),
        body.as_bytes(),
        &exact,
    );
    assert_eq!(e.text.as_deref(), Some(body.as_str()));
}
#[test]
fn bytes_must_match_their_descriptor() {
    let d = describe("text/plain", b"hello");
    for other in [&b"hellp"[..], b"hello!"] {
        let e = extract(&d, other, &Limits::default());
        assert_eq!(e.state, State::ExtractionFailed);
        assert!(e.text.is_none());
        assert_eq!(e.source, d);
    }
}
#[test]
fn images_and_unknown_types_have_their_own_states() {
    let png = b"\x89PNG\r\n\x1a\nxx";
    assert_eq!(run("image/png", png).state, State::NoExtractableText);
    assert_eq!(run("text/html", b"<p>x</p>").state, State::ExtractionFailed);
}
#[test]
fn states_serialize_as_stable_names() {
    let e = serde_json::to_value(run("image/gif", b"GIF89a")).unwrap();
    assert_eq!(e["state"], "no_extractable_text");
    assert_eq!(e["text"], serde_json::Value::Null);
    assert_eq!(e["source"]["name"], "fixture");
}

// ---- DOCX ----
struct Part<'a>(&'a str, &'a [u8], bool);
/// A ZIP with real checksums; `Part.2` deflates the entry.
fn zip(parts: &[Part], flags: u16) -> Vec<u8> {
    let (mut out, mut dir) = (Vec::new(), Vec::new());
    for Part(name, data, deflate) in parts {
        let packed = if *deflate {
            let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
            e.write_all(data).unwrap();
            e.finish().unwrap()
        } else {
            data.to_vec()
        };
        let method: u16 = if *deflate { 8 } else { 0 };
        let mut crc = Crc::new();
        crc.update(data);
        let at = out.len() as u32;
        out.extend(b"PK\x03\x04\x14\0");
        out.extend(flags.to_le_bytes());
        out.extend(method.to_le_bytes());
        out.extend([0; 4]);
        out.extend(crc.sum().to_le_bytes());
        out.extend((packed.len() as u32).to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend([0, 0]);
        out.extend(name.as_bytes());
        out.extend(&packed);
        dir.extend(b"PK\x01\x02\x14\0\x14\0");
        dir.extend(flags.to_le_bytes());
        dir.extend(method.to_le_bytes());
        dir.extend([0; 4]);
        dir.extend(crc.sum().to_le_bytes());
        dir.extend((packed.len() as u32).to_le_bytes());
        dir.extend((data.len() as u32).to_le_bytes());
        dir.extend((name.len() as u16).to_le_bytes());
        dir.extend([0; 12]);
        dir.extend(at.to_le_bytes());
        dir.extend(name.as_bytes());
    }
    let start = out.len() as u32;
    out.extend(&dir);
    out.extend(b"PK\x05\x06\0\0\0\0");
    out.extend((parts.len() as u16).to_le_bytes());
    out.extend((parts.len() as u16).to_le_bytes());
    out.extend((dir.len() as u32).to_le_bytes());
    out.extend(start.to_le_bytes());
    out.extend([0, 0]);
    out
}
fn docx(body: &str, deflate: bool) -> Vec<u8> {
    let xml = format!(
        "<?xml version=\"1.0\"?><w:document xmlns:w=\"x\"><w:body>{body}</w:body></w:document>"
    );
    zip(
        &[
            Part("[Content_Types].xml", b"<Types/>", deflate),
            Part("word/document.xml", xml.as_bytes(), deflate),
        ],
        0,
    )
}
const WORD: &str = crate::attachments::DOCX;

#[test]
fn docx_text_is_read_in_order_with_structure() {
    let body = "<w:p><w:r><w:t>Quarterly </w:t></w:r><w:r><w:t xml:space=\"preserve\">plan &amp; budget</w:t></w:r></w:p>\
        <w:p><w:r><w:t>a</w:t><w:tab/><w:t>b</w:t><w:br/><w:t>caf&#xE9; &#233;</w:t></w:r></w:p>\
        <w:p><w:del><w:r><w:delText>removed</w:delText></w:r></w:del><w:r><w:instrText> PAGE </w:instrText></w:r></w:p>\
        <w:p><w:r><w:t/></w:r><w:r><w:t>last</w:t></w:r></w:p>";
    for deflate in [false, true] {
        let got = text_of(WORD, &docx(body, deflate));
        assert_eq!(
            got, "Quarterly plan & budget\na\tb\ncafé é\n\nlast",
            "deflate={deflate}"
        );
    }
}
#[test]
fn docx_without_text_or_not_a_package_is_distinguished() {
    let empty = docx("<w:p><w:r><w:drawing/></w:r></w:p>", true);
    assert_eq!(run(WORD, &empty).state, State::NoExtractableText);
    let missing = zip(&[Part("[Content_Types].xml", b"<Types/>", false)], 0);
    let mut cut = docx("<w:p><w:r><w:t>x</w:t></w:r></w:p>", true);
    cut.truncate(cut.len() / 2);
    let mut flipped = docx("<w:p><w:r><w:t>hello</w:t></w:r></w:p>", false);
    let at = flipped.windows(5).position(|w| w == b"hello").unwrap();
    flipped[at] = b'j';
    let encrypted = zip(
        &[
            Part("[Content_Types].xml", b"<Types/>", false),
            Part("word/document.xml", b"<w:t>x</w:t>", false),
        ],
        1,
    );
    let bad_xml = docx("<w:p><w:t>a &bogus; b</w:t></w:p>", false);
    let ole = [b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1".as_slice(), &[0; 64]].concat();
    for (name, bytes) in [
        ("missing document", missing),
        ("truncated", cut),
        ("bad checksum", flipped),
        ("encrypted entry", encrypted),
        ("bad entity", bad_xml),
        ("ole container", ole),
    ] {
        let e = run(WORD, &bytes);
        assert_eq!(e.state, State::Unreadable, "{name}: {}", e.message);
        assert!(e.text.is_none() && !e.message.is_empty());
    }
}
#[test]
fn docx_decompression_is_bounded() {
    let body = format!("<w:p><w:t>{}</w:t></w:p>", "a".repeat(200_000));
    let bytes = docx(&body, true);
    assert!(bytes.len() < 2_000, "the fixture compresses well");
    assert_eq!(text_of(WORD, &bytes).len(), 200_000);
    let tight = Limits {
        max_inflated_bytes: 50_000,
        ..Limits::default()
    };
    let e = extract(&describe(WORD, &bytes), &bytes, &tight);
    assert_eq!(e.state, State::LimitExceeded);
    assert!(e.text.is_none() && e.message.contains("decompression"));
}

// ---- PDF ----
fn object(n: u32, dict: &str, stream: Option<&[u8]>) -> Vec<u8> {
    let mut out = format!("{n} 0 obj\n").into_bytes();
    match stream {
        Some(data) => {
            out.extend(format!("<< {dict} /Length {} >>\nstream\n", data.len()).bytes());
            out.extend(data);
            out.extend(b"\nendstream");
        }
        None => out.extend(format!("<< {dict} >>").bytes()),
    }
    out.extend(b"\nendobj\n");
    out
}
fn pdf(objects: &[Vec<u8>], trailer: &str) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    objects.iter().for_each(|o| out.extend(o));
    out.extend(format!("trailer\n<< /Root 1 0 R {trailer} >>\nstartxref\n0\n%%EOF\n").bytes());
    out
}
/// Pages (content, optional filter), all with font /F1 = object 3.
fn simple_pdf(contents: &[(&[u8], bool)], font: &str) -> Vec<u8> {
    let kids: String = (0..contents.len())
        .map(|i| format!("{} 0 R ", 10 + 2 * i))
        .collect();
    let mut objs = vec![
        object(1, "/Type /Catalog /Pages 2 0 R", None),
        object(
            2,
            &format!(
                "/Type /Pages /Count {} /Kids [{kids}] /Resources << /Font << /F1 3 0 R >> >>",
                contents.len()
            ),
            None,
        ),
        object(3, font, None),
    ];
    for (i, (data, packed)) in contents.iter().enumerate() {
        let n = 10 + 2 * i as u32;
        objs.push(object(
            n,
            &format!("/Type /Page /Parent 2 0 R /Contents {} 0 R", n + 1),
            None,
        ));
        if *packed {
            objs.push(object(n + 1, "/Filter /FlateDecode", Some(&zlib(data))));
        } else {
            objs.push(object(n + 1, "", Some(data)));
        }
    }
    pdf(&objs, "")
}
const HELVETICA: &str = "/Type /Font /Subtype /Type1 /BaseFont /Helvetica";
const PDF: &str = "application/pdf";

#[test]
fn pdf_text_follows_page_order_and_line_structure() {
    let first = b"BT /F1 12 Tf 72 700 Td (Hello \\(PDF\\) world) Tj 0 -14 Td [(Kern) -300 (ed) 10 (text)] TJ ET";
    let second = b"BT /F1 12 Tf 1 0 0 1 72 700 Tm <48656C6C6F> Tj 1 0 0 1 72 680 Tm (Page two \\101) Tj T* (next) ' ET";
    for packed in [false, true] {
        let bytes = simple_pdf(&[(first, packed), (second, packed)], HELVETICA);
        assert_eq!(
            text_of(PDF, &bytes),
            "Hello (PDF) world\nKern edtext\n\nHello\nPage two A\nnext",
            "packed={packed}"
        );
    }
}
#[test]
fn pdf_pages_follow_the_tree_not_file_order() {
    let a = simple_pdf(
        &[
            (b"BT /F1 9 Tf (one) Tj ET", false),
            (b"BT /F1 9 Tf (two) Tj ET", false),
        ],
        HELVETICA,
    );
    // Swap the kids: the second page object now comes first in the tree.
    let swapped = String::from_utf8_lossy(&a).replace("[10 0 R 12 0 R ]", "[12 0 R 10 0 R ]");
    assert_eq!(text_of(PDF, swapped.as_bytes()), "two\n\none");
}
#[test]
fn pdf_unicode_fonts_use_their_tounicode_map() {
    let cmap = b"/CIDInit begincmap 1 begincodespacerange <0000> <FFFF> endcodespacerange \
        2 beginbfchar <0001> <0048> <0002> <00E9> endbfchar \
        1 beginbfrange <0003> <0005> <0061> endbfrange \
        1 beginbfrange <0010> <0011> [<D83DDE00> <0058>] endbfrange endcmap";
    let objs = vec![
        object(1, "/Type /Catalog /Pages 2 0 R", None),
        object(2, "/Type /Pages /Count 1 /Kids [4 0 R]", None),
        object(
            3,
            "/Type /Font /Subtype /Type0 /Encoding /Identity-H /ToUnicode 5 0 R",
            None,
        ),
        object(
            4,
            "/Type /Page /Parent 2 0 R /Contents 6 0 R /Resources << /Font << /F1 3 0 R >> >>",
            None,
        ),
        object(5, "/Filter /FlateDecode", Some(&zlib(cmap))),
        object(
            6,
            "",
            Some(b"BT /F1 12 Tf <0001000200030004000500100011> Tj ET"),
        ),
    ];
    assert_eq!(text_of(PDF, &pdf(&objs, "")), "Héabc😀X");
}
#[test]
fn pdf_objects_inside_object_streams_are_read() {
    let (p1, p2) = (
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Count 1 /Kids [4 0 R] /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 >> >> >> >>",
    );
    let p4 = "<< /Type /Page /Parent 2 0 R /Contents 6 0 R >>";
    let header = format!("1 0 2 {} 4 {} ", p1.len() + 1, p1.len() + p2.len() + 2);
    let body = format!("{header}{p1} {p2} {p4}");
    let objs = vec![
        object(
            9,
            &format!(
                "/Type /ObjStm /N 3 /First {} /Filter /FlateDecode",
                header.len()
            ),
            Some(&zlib(body.as_bytes())),
        ),
        object(6, "", Some(b"BT /F1 12 Tf (packed) Tj ET")),
    ];
    assert_eq!(text_of(PDF, &pdf(&objs, "")), "packed");
}
#[test]
fn image_only_pdf_has_no_extractable_text() {
    let page = b"q 612 0 0 792 0 0 cm /Im0 Do Q BI /W 1 /H 1 /CS /G /BPC 8 ID \x00(x) Tj\nEI";
    let e = run(PDF, &simple_pdf(&[(page, true)], HELVETICA));
    assert_eq!(e.state, State::NoExtractableText, "{}", e.message);
    assert!(e.message.contains("OCR") && e.text.is_none());
    let blank = run(
        PDF,
        &simple_pdf(&[(b"BT /F1 12 Tf ( ) Tj ET", false)], HELVETICA),
    );
    assert_eq!(blank.state, State::NoExtractableText);
}
#[test]
fn unmappable_fonts_are_reported_never_dropped() {
    let custom = "/Type /Font /Subtype /Type1 /Encoding << /Differences [1 /a] >>";
    let e = run(
        PDF,
        &simple_pdf(&[(b"BT /F1 12 Tf (\x01\x01) Tj ET", false)], custom),
    );
    assert_eq!(e.state, State::ExtractionFailed, "{}", e.message);
    assert!(e.message.contains("2 text characters") && e.text.is_none());
    // Mixed: readable text stays, the unmappable part is marked and counted.
    let two = [(&b"BT /F1 12 Tf (visible) Tj ET"[..], false)];
    let mut bytes = simple_pdf(&two, HELVETICA);
    let extra = object(
        20,
        "/Type /Page /Parent 2 0 R /Contents 21 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type3 >> >> >>",
        None,
    );
    let content = object(21, "", Some(b"BT /F1 12 Tf (zz) Tj ET"));
    let at = bytes.windows(7).position(|w| w == b"trailer").unwrap();
    bytes.splice(at..at, extra.into_iter().chain(content));
    let text = String::from_utf8_lossy(&bytes).replace("[10 0 R ]", "[10 0 R 20 0 R]");
    let e = run(PDF, text.as_bytes());
    assert_eq!(e.state, State::Extracted, "{}", e.message);
    assert_eq!(e.text.as_deref(), Some("visible\n\n\u{FFFD}\u{FFFD}"));
    assert!(e.message.contains("2 characters") && e.message.contains("U+FFFD"));
}
#[test]
fn damaged_encrypted_or_unsupported_pdfs_are_explicit() {
    let good = simple_pdf(&[(b"BT /F1 12 Tf (x) Tj ET", true)], HELVETICA);
    let truncated = good[..good.len() - 40].to_vec();
    let mut corrupt = good.clone();
    let at = corrupt.windows(6).position(|w| w == b"stream").unwrap() + 12;
    corrupt[at..at + 8].fill(0xAB);
    let no_pages = pdf(&[object(1, "/Type /Catalog", None)], "");
    let encrypted = pdf(
        &[object(1, "/Type /Catalog /Pages 2 0 R", None)],
        "/Encrypt 9 0 R",
    );
    for (name, bytes, state) in [
        ("truncated", truncated, State::Unreadable),
        ("corrupt stream", corrupt, State::Unreadable),
        ("no pages", no_pages, State::Unreadable),
        ("encrypted", encrypted, State::Unreadable),
        ("not a pdf", b"PK\x03\x04 %%EOF".to_vec(), State::Unreadable),
    ] {
        let e = run(PDF, &bytes);
        assert_eq!(e.state, state, "{name}: {}", e.message);
        assert!(e.text.is_none() && !e.message.is_empty(), "{name}");
    }
    let lzw = simple_pdf(&[(b"BT (x) Tj ET", false)], HELVETICA);
    let lzw =
        String::from_utf8_lossy(&lzw).replace("/Length 12 >>", "/Filter /LZWDecode /Length 12 >>");
    let e = run(PDF, lzw.as_bytes());
    assert_eq!(e.state, State::ExtractionFailed, "{}", e.message);
    assert!(e.message.contains("LZWDecode"));
}
#[test]
fn pdf_decompression_and_text_are_bounded() {
    let content = format!("BT /F1 12 Tf ({}) Tj ET", "a".repeat(300_000));
    let bytes = simple_pdf(&[(content.as_bytes(), true)], HELVETICA);
    let tight = Limits {
        max_inflated_bytes: 100_000,
        ..Limits::default()
    };
    assert_eq!(
        extract(&describe(PDF, &bytes), &bytes, &tight).state,
        State::LimitExceeded
    );
    let small = Limits {
        max_text_bytes: 1000,
        ..Limits::default()
    };
    let e = extract(&describe(PDF, &bytes), &bytes, &small);
    assert_eq!(e.state, State::LimitExceeded);
    assert!(e.text.is_none());
    assert_eq!(text_of(PDF, &bytes).len(), 300_000);
}
#[test]
fn hostile_pdf_structure_terminates() {
    // Cyclic page tree and a deeply nested array do not hang or overflow.
    let objs = vec![
        object(1, "/Type /Catalog /Pages 2 0 R", None),
        object(2, "/Type /Pages /Kids [2 0 R 3 0 R]", None),
        object(3, "/Type /Pages /Kids [2 0 R]", None),
    ];
    assert_eq!(run(PDF, &pdf(&objs, "")).state, State::Unreadable);
    let nested = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    let objs = vec![
        object(1, "/Type /Catalog /Pages 2 0 R", None),
        object(2, &format!("/Type /Pages /Kids [4 0 R] /X {nested}"), None),
        object(4, "/Type /Page /Parent 2 0 R /Contents 6 0 R", None),
        object(6, "", Some(format!("{nested} BT (deep) Tj ET").as_bytes())),
    ];
    let e = run(PDF, &pdf(&objs, ""));
    assert!(matches!(
        e.state,
        State::Extracted | State::Unreadable | State::NoExtractableText
    ));
}
