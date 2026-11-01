//! Diagnostic process observation, not a hard allocator/RSS enforcement claim.
//! Run the built example in a fresh process; no database/service/provider needed.
use aq_result_media::*;
fn main() {
    let input = {
        let mut pixels = vec![17; MAX_RGBA_BYTES];
        // Near-input-limit entropy plus a maximum decoded raster. This exercises
        // the separately accounted compressed concatenation as well as pixels.
        let mut random = 0x1234_5678_u32;
        for byte in pixels.iter_mut().take(800_000) {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            *byte = random as u8;
        }
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2000, 1000);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Balanced);
            encoder.set_filter(png::Filter::Paeth);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&pixels).unwrap();
            writer.finish().unwrap();
        }
        bytes
    };
    assert!(input.len() <= MAX_INPUT_BYTES);
    let mut compressed_idat_bytes = 0;
    let mut offset = 8;
    while offset < input.len() {
        let length = u32::from_be_bytes(input[offset..offset + 4].try_into().unwrap()) as usize;
        if &input[offset + 4..offset + 8] == b"IDAT" {
            compressed_idat_bytes += length;
        }
        offset += 12 + length;
    }
    let started = std::time::Instant::now();
    let mut observed = None;
    for _ in 0..5 {
        let p = prepare_png(&input).unwrap();
        observed = Some((
            p.render().descriptor().clone(),
            p.preview().descriptor().clone(),
        ));
    }
    let (render, preview) = observed.unwrap();
    let peak_kib = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|line| line.starts_with("VmHWM:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u64>().ok())
        });
    println!(
        "{}",
        serde_json::json!({"iterations":5,"input_bytes":input.len(),"compressed_idat_bytes":compressed_idat_bytes,"max_compressed_copy_bytes":MAX_COMPRESSED_BYTES,"zlib_scratch_bytes":ZLIB_SCRATCH_BYTES,"pixels":2_000_000,"max_full_rgba_bytes":MAX_RGBA_BYTES,"max_preview_rgba_bytes":MAX_PREVIEW_RGBA_BYTES,"decoder_scratch_hint":DECODER_SCRATCH_HINT,"render":render,"preview":preview,"elapsed_ms":started.elapsed().as_millis(),"process_peak_rss_kib":peak_kib,"qualification":"observed process high-water mark including compressed copy, not a hard RSS guarantee"})
    );
}
