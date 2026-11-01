#![allow(dead_code)]
pub fn png(width: u32, height: u32, color: png::ColorType, pixels: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Balanced);
        encoder.set_filter(png::Filter::Paeth);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(pixels).unwrap();
        writer.finish().unwrap();
    }
    bytes
}
pub fn decode(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .unwrap();
    let mut out = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut out).unwrap();
    reader.finish().unwrap();
    out.truncate(info.buffer_size());
    (info.width, info.height, out)
}
// Independent bitwise reference, compared to known PNG chunks and the codec's
// table-based preflight. Test construction does not use the production parser.
pub fn crc(bytes: &[u8]) -> u32 {
    let mut c = !0_u32;
    for &x in bytes {
        c ^= u32::from(x);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb88320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}
pub fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc(&out[4..]).to_be_bytes());
    out
}
pub fn header(width: u32, height: u32, color: u8) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&width.to_be_bytes());
    data.extend_from_slice(&height.to_be_bytes());
    data.extend_from_slice(&[8, color, 0, 0, 0]);
    chunk(b"IHDR", &data)
}
pub fn chunks(bytes: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    let mut out = Vec::new();
    let mut offset = 8;
    while offset < bytes.len() {
        let len = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        out.push((
            bytes[offset + 4..offset + 8].try_into().unwrap(),
            bytes[offset + 8..offset + 8 + len].to_vec(),
        ));
        offset += 12 + len;
    }
    out
}
pub fn rebuild(chunks: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in chunks {
        out.extend(chunk(kind, data));
    }
    out
}
