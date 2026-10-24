//! Canvas size declared by an image's own headers, read without gdk-pixbuf.
//!
//! gdk-pixbuf's header probe is not safe on hostile input: with the glycin
//! loaders (Fedora 44, gdk-pixbuf 2.44.4) `Pixbuf::file_info` aborts the
//! process when width x height x 4 overflows an `int`. Parsing the formats
//! Slack serves ourselves lets oversized files be rejected before gdk-pixbuf
//! ever opens them.

use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

/// Width and height declared by the PNG, GIF, JPEG or WebP at `path`. `None`
/// for other formats and for files too short or malformed to say.
pub(crate) fn declared_dimensions(path: &Path) -> Option<(u32, u32)> {
    let mut reader = BufReader::new(File::open(path).ok()?);
    dimensions(&mut reader)
}

fn dimensions(reader: &mut (impl Read + Seek)) -> Option<(u32, u32)> {
    let mut magic = [0u8; 12];
    reader.read_exact(&mut magic).ok()?;
    match magic {
        [0x89, b'P', b'N', b'G', ..] => png(reader),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', w0, w1, h0, h1, ..] => Some((
            u32::from(u16::from_le_bytes([w0, w1])),
            u32::from(u16::from_le_bytes([h0, h1])),
        )),
        [0xff, 0xd8, ..] => {
            reader.seek(std::io::SeekFrom::Start(2)).ok()?;
            jpeg(reader)
        }
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P'] => webp(reader),
        _ => None,
    }
}

/// IHDR always comes first: 4-byte length, "IHDR", then big-endian width
/// and height.
fn png(reader: &mut impl Read) -> Option<(u32, u32)> {
    let mut ihdr = [0u8; 12];
    reader.read_exact(&mut ihdr).ok()?;
    if &ihdr[..4] != b"IHDR" {
        return None;
    }
    Some((be32(&ihdr[4..8]), be32(&ihdr[8..12])))
}

/// Walks marker segments up to the first start-of-frame.
fn jpeg(reader: &mut (impl Read + Seek)) -> Option<(u32, u32)> {
    loop {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte).ok()?;
        if byte[0] != 0xff {
            return None;
        }
        // Markers may be preceded by any number of 0xff fill bytes.
        let marker = loop {
            reader.read_exact(&mut byte).ok()?;
            if byte[0] != 0xff {
                break byte[0];
            }
        };
        if matches!(marker, 0x01 | 0xd0..=0xd9) {
            continue;
        }
        let mut length = [0u8; 2];
        reader.read_exact(&mut length).ok()?;
        let length = u16::from_be_bytes(length);
        let is_start_of_frame =
            matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
        if is_start_of_frame {
            let mut frame = [0u8; 5];
            reader.read_exact(&mut frame).ok()?;
            let height = u16::from_be_bytes([frame[1], frame[2]]);
            let width = u16::from_be_bytes([frame[3], frame[4]]);
            return Some((u32::from(width), u32::from(height)));
        }
        reader
            .seek_relative(i64::from(length.checked_sub(2)?))
            .ok()?;
    }
}

/// The first chunk names the bitstream and carries the canvas size.
fn webp(reader: &mut impl Read) -> Option<(u32, u32)> {
    let mut chunk = [0u8; 18];
    reader.read_exact(&mut chunk).ok()?;
    let payload = &chunk[8..];
    match &chunk[..4] {
        b"VP8X" => Some((le24(&payload[4..7]) + 1, le24(&payload[7..10]) + 1)),
        b"VP8 " if payload[3..6] == [0x9d, 0x01, 0x2a] => Some((
            u32::from(u16::from_le_bytes([payload[6], payload[7]]) & 0x3fff),
            u32::from(u16::from_le_bytes([payload[8], payload[9]]) & 0x3fff),
        )),
        b"VP8L" if payload[0] == 0x2f => {
            let bits = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        _ => None,
    }
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn le24(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims(bytes: &[u8]) -> Option<(u32, u32)> {
        dimensions(&mut std::io::Cursor::new(bytes))
    }

    fn encoded(format: &str, width: i32, height: i32) -> Vec<u8> {
        gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, width, height)
            .expect("allocate pixbuf")
            .save_to_bufferv(format, &[])
            .expect("encode image")
    }

    #[test]
    fn reads_png_and_jpeg_written_by_gdk_pixbuf() {
        assert_eq!(dims(&encoded("png", 37, 21)), Some((37, 21)));
        assert_eq!(dims(&encoded("jpeg", 40, 9)), Some((40, 9)));
    }

    #[test]
    fn reads_gif_logical_screen() {
        assert_eq!(
            dims(b"GIF89a\x30\x75\x30\x75\x80\x00\x00"),
            Some((30000, 30000))
        );
    }

    #[test]
    fn reads_every_webp_bitstream() {
        let riff = |tag: &[u8; 4], payload: &[u8]| {
            let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
            bytes.extend_from_slice(tag);
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(payload);
            bytes
        };
        let vp8x = [0, 0, 0, 0, 0x2f, 0x75, 0, 0x2f, 0x75, 0];
        assert_eq!(dims(&riff(b"VP8X", &vp8x)), Some((30000, 30000)));
        let vp8 = [0, 0, 0, 0x9d, 0x01, 0x2a, 64, 0, 32, 0];
        assert_eq!(dims(&riff(b"VP8 ", &vp8)), Some((64, 32)));
        // 64x32: (64 - 1) | (32 - 1) << 14.
        let bits = (63u32 | 31 << 14).to_le_bytes();
        let vp8l = [0x2f, bits[0], bits[1], bits[2], bits[3], 0, 0, 0, 0, 0];
        assert_eq!(dims(&riff(b"VP8L", &vp8l)), Some((64, 32)));
    }

    #[test]
    fn unknown_or_truncated_files_have_no_size() {
        assert_eq!(dims(b"not an image at all"), None);
        assert_eq!(dims(b"GIF89a"), None);
        assert_eq!(dims(&encoded("png", 4, 4)[..14]), None);
        assert_eq!(
            dims(&[0xff, 0xd8, 0xff, 0xe0, 0, 1, 0, 0, 0, 0, 0, 0]),
            None
        );
    }
}
