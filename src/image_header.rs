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

/// Container format recognised from an image's magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageFormat {
    Png,
    Gif,
    Jpeg,
    Webp,
}

impl ImageFormat {
    /// Whether the format can hold several frames. Like gdk-pixbuf, an
    /// animated PNG counts as a still PNG.
    pub(crate) fn may_be_animated(self) -> bool {
        matches!(self, Self::Gif | Self::Webp)
    }
}

/// Width and height declared by the PNG, GIF, JPEG or WebP at `path`. `None`
/// for other formats and for files too short or malformed to say.
pub(crate) fn declared_dimensions(path: &Path) -> Option<(u32, u32)> {
    declared_header(path).map(|(_, width, height)| (width, height))
}

/// Format and declared canvas size of the image at `path`. Reads a few
/// header bytes, so unlike gdk-pixbuf's out-of-process glycin probe it is
/// cheap enough to call on the GTK thread for every bound row.
pub(crate) fn declared_header(path: &Path) -> Option<(ImageFormat, u32, u32)> {
    let mut reader = BufReader::new(File::open(path).ok()?);
    header(&mut reader)
}

fn header(reader: &mut (impl Read + Seek)) -> Option<(ImageFormat, u32, u32)> {
    let mut magic = [0u8; 12];
    reader.read_exact(&mut magic).ok()?;
    let (format, (width, height)) = match magic {
        [0x89, b'P', b'N', b'G', ..] => (ImageFormat::Png, png(reader)?),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', w0, w1, h0, h1, ..] => (
            ImageFormat::Gif,
            (
                u32::from(u16::from_le_bytes([w0, w1])),
                u32::from(u16::from_le_bytes([h0, h1])),
            ),
        ),
        [0xff, 0xd8, ..] => {
            reader.seek(std::io::SeekFrom::Start(2)).ok()?;
            (ImageFormat::Jpeg, jpeg(reader)?)
        }
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P'] => {
            (ImageFormat::Webp, webp(reader)?)
        }
        _ => return None,
    };
    Some((format, width, height))
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

/// Frames declared by the GIF or WebP at `path`, read from its block and
/// chunk headers alone: 1 means a still, so callers can skip the animation
/// decoder and decode once. A GIF stops counting at 2. `None` for other
/// formats and for files too short or malformed to say; callers should then
/// assume the file may be animated.
pub(crate) fn declared_frame_count(path: &Path) -> Option<u32> {
    let mut reader = BufReader::new(File::open(path).ok()?);
    let (format, _, _) = header(&mut reader)?;
    reader.rewind().ok()?;
    match format {
        ImageFormat::Gif => gif_frame_count(&mut reader),
        ImageFormat::Webp => {
            webp_frame_count(&mut reader).map(|frames| u32::try_from(frames).unwrap_or(u32::MAX))
        }
        ImageFormat::Png | ImageFormat::Jpeg => Some(1),
    }
}

/// Most blocks and sub-blocks [`gif_frame_count`] walks before giving up:
/// twice what an 8 MiB file of 255-byte sub-blocks needs.
const MAX_GIF_BLOCKS: u32 = 1 << 16;

/// Number of image descriptors in a GIF, counting up to 2. Skips extension
/// and image data sub-blocks without decoding them. `None` when the stream
/// is not a GIF, is truncated before its trailer, or has no image.
fn gif_frame_count(reader: &mut (impl Read + Seek)) -> Option<u32> {
    let mut screen = [0u8; 13];
    reader.read_exact(&mut screen).ok()?;
    if &screen[..3] != b"GIF" {
        return None;
    }
    skip_gif_color_table(reader, screen[10])?;
    let mut frames = 0;
    for _ in 0..MAX_GIF_BLOCKS {
        let mut block = [0u8; 1];
        reader.read_exact(&mut block).ok()?;
        match block[0] {
            // Extension: a label byte, then data sub-blocks.
            0x21 => {
                reader.seek_relative(1).ok()?;
                skip_gif_sub_blocks(reader)?;
            }
            // Image descriptor, optional local color table, LZW code size,
            // then data sub-blocks.
            0x2c => {
                frames += 1;
                if frames == 2 {
                    return Some(frames);
                }
                let mut descriptor = [0u8; 9];
                reader.read_exact(&mut descriptor).ok()?;
                skip_gif_color_table(reader, descriptor[8])?;
                reader.seek_relative(1).ok()?;
                skip_gif_sub_blocks(reader)?;
            }
            0x3b => return (frames > 0).then_some(frames),
            _ => return None,
        }
    }
    None
}

/// Skips the color table that `packed` (the flags byte of a logical screen
/// or image descriptor) announces: bit 7 is its presence, bits 0-2 its size.
fn skip_gif_color_table(reader: &mut impl Seek, packed: u8) -> Option<()> {
    if packed & 0x80 != 0 {
        reader.seek_relative(3 << ((packed & 0x07) + 1)).ok()?;
    }
    Some(())
}

/// Skips length-prefixed sub-blocks up to and including the zero terminator.
fn skip_gif_sub_blocks(reader: &mut (impl Read + Seek)) -> Option<()> {
    for _ in 0..MAX_GIF_BLOCKS {
        let mut length = [0u8; 1];
        reader.read_exact(&mut length).ok()?;
        if length[0] == 0 {
            return Some(());
        }
        reader.seek_relative(i64::from(length[0])).ok()?;
    }
    None
}

/// Number of frames in a RIFF WebP: its `ANMF` chunks, or 1 for a still.
/// Walks chunk headers only. `None` when the stream is not a WebP.
pub(crate) fn webp_frame_count(mut reader: impl std::io::Read + std::io::Seek) -> Option<u64> {
    let mut header = [0u8; 12];
    reader.read_exact(&mut header).ok()?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WEBP" {
        return None;
    }
    let mut frames = 0u64;
    let mut chunk = [0u8; 8];
    while reader.read_exact(&mut chunk).is_ok() {
        if &chunk[..4] == b"ANMF" {
            frames += 1;
        }
        let size = i64::from(u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]));
        // Chunk payloads are padded to an even length.
        reader.seek_relative(size + (size & 1)).ok()?;
    }
    Some(frames.max(1))
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
        header(&mut std::io::Cursor::new(bytes)).map(|(_, width, height)| (width, height))
    }

    fn format(bytes: &[u8]) -> Option<ImageFormat> {
        header(&mut std::io::Cursor::new(bytes)).map(|(format, _, _)| format)
    }

    #[test]
    fn names_the_format_and_whether_it_may_animate() {
        assert_eq!(format(&encoded("png", 2, 2)), Some(ImageFormat::Png));
        assert_eq!(format(&encoded("jpeg", 2, 2)), Some(ImageFormat::Jpeg));
        assert_eq!(
            format(b"GIF89a\x02\x00\x02\x00\x80\x00\x00"),
            Some(ImageFormat::Gif)
        );
        assert!(ImageFormat::Gif.may_be_animated());
        assert!(ImageFormat::Webp.may_be_animated());
        assert!(!ImageFormat::Png.may_be_animated());
        assert!(!ImageFormat::Jpeg.may_be_animated());
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

    /// 1x1 GIF89a with a global color table and one frame.
    const STILL_GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;";

    /// The same GIF with a second frame, which has a local color table.
    const TWO_FRAME_GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff\
        !\xf9\x04\x00\x0a\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00\
        !\xfe\x03a\x3b\x3b\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\x02\x02L\x01\x00;";

    fn gif_frames(bytes: &[u8]) -> Option<u32> {
        gif_frame_count(&mut std::io::Cursor::new(bytes))
    }

    #[test]
    fn counts_gif_frames_from_block_headers() {
        assert_eq!(gif_frames(STILL_GIF), Some(1));
        assert_eq!(gif_frames(TWO_FRAME_GIF), Some(2));
    }

    #[test]
    fn gif_data_bytes_that_look_like_blocks_are_skipped() {
        // The comment extension and the LZW data hold 0x2c and 0x3b bytes.
        let mut bytes = TWO_FRAME_GIF.to_vec();
        let at = bytes.windows(2).position(|pair| pair == b"D\x01").unwrap();
        bytes[at] = 0x2c;
        assert_eq!(gif_frames(&bytes), Some(2));
    }

    #[test]
    fn malformed_gifs_have_no_frame_count() {
        // Truncated before the trailer, so a later frame cannot be ruled out.
        assert_eq!(gif_frames(&STILL_GIF[..STILL_GIF.len() - 1]), None);
        assert_eq!(gif_frames(&STILL_GIF[..30]), None);
        // No image at all, and an unknown block type.
        assert_eq!(gif_frames(b"GIF89a\x01\x00\x01\x00\x00\x00\x00;"), None);
        assert_eq!(gif_frames(b"GIF89a\x01\x00\x01\x00\x00\x00\x00\x99;"), None);
        assert_eq!(gif_frames(b"not a gif at all"), None);
        // A sub-block chain that never ends is cut off, not followed forever.
        let mut endless = b"GIF89a\x01\x00\x01\x00\x00\x00\x00!\xf9".to_vec();
        endless.extend(std::iter::repeat_n(1u8, 3 * MAX_GIF_BLOCKS as usize));
        assert_eq!(gif_frames(&endless), None);
    }

    fn frame_count_of(name: &str, bytes: &[u8]) -> Option<u32> {
        let path = std::env::temp_dir().join(format!("conduit-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).expect("write temp image");
        let count = declared_frame_count(&path);
        let _ = std::fs::remove_file(path);
        count
    }

    #[test]
    fn declared_frame_count_covers_every_format() {
        assert_eq!(frame_count_of("one.gif", STILL_GIF), Some(1));
        assert_eq!(frame_count_of("two.gif", TWO_FRAME_GIF), Some(2));
        let still = riff_webp(&[(b"VP8L", &[0x2f, 0, 0, 0, 0, 0, 0, 0, 0, 0])]);
        assert_eq!(frame_count_of("one.webp", &still), Some(1));
        let anim = riff_webp(&[
            (b"VP8X", &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            (b"ANMF", &[0; 3]),
            (b"ANMF", &[0; 16]),
        ]);
        assert_eq!(frame_count_of("two.webp", &anim), Some(2));
        assert_eq!(frame_count_of("one.png", &encoded("png", 2, 2)), Some(1));
        assert_eq!(frame_count_of("junk.bin", b"not an image at all"), None);
    }

    fn riff_webp(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut body = b"WEBP".to_vec();
        for (tag, payload) in chunks {
            body.extend_from_slice(*tag);
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                body.push(0);
            }
        }
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes
    }

    #[test]
    fn webp_frame_count_counts_anmf_chunks() {
        let count = |bytes: Vec<u8>| webp_frame_count(std::io::Cursor::new(bytes));
        let anim = riff_webp(&[
            (b"VP8X", &[0; 10]),
            (b"ANIM", &[0; 6]),
            (b"ANMF", &[0; 3]),
            (b"ANMF", &[0; 16]),
            (b"ANMF", &[]),
        ]);
        assert_eq!(count(anim), Some(3));
        assert_eq!(count(riff_webp(&[(b"VP8 ", &[0; 5])])), Some(1));
        assert_eq!(count(STILL_GIF.to_vec()), None);
    }
}
