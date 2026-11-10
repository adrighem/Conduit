//! GTK-thread memo of what cached image files declare in their headers:
//! format, size and whether they may animate. Row binds ask for these on
//! every bind; cache files are written once by an atomic rename under a
//! content-addressed name, so each path's header is read once and kept.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Seek};
use std::path::{Path, PathBuf};

use crate::image_header::ImageFormat;

/// Paths remembered before the memo starts over.
const MAX_ENTRIES: usize = 4096;

/// What one image file's headers declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImageMetadata {
    pub(crate) format: ImageFormat,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Frames declared, a GIF counted up to 2: `Some(1)` is a still, `None`
    /// means the headers could not say.
    pub(crate) frames: Option<u32>,
}

thread_local! {
    /// `None` for a file that opened but is no image format we parse.
    static MEMO: RefCell<HashMap<PathBuf, Option<ImageMetadata>>> =
        RefCell::new(HashMap::new());
}

#[cfg(test)]
thread_local! {
    /// Files opened to read headers on this thread.
    pub(crate) static HEADER_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The headers of `path`, read once per path. A file that cannot be opened
/// is not remembered, so it is read again once it exists.
pub(crate) fn metadata(path: &Path) -> Option<ImageMetadata> {
    if let Some(known) = MEMO.with(|memo| memo.borrow().get(path).copied()) {
        return known;
    }
    let Ok(file) = File::open(path) else {
        return None;
    };
    #[cfg(test)]
    HEADER_READS.with(|reads| reads.set(reads.get() + 1));
    let read = read_metadata(&mut BufReader::new(file));
    MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if memo.len() >= MAX_ENTRIES {
            memo.clear();
        }
        memo.insert(path.to_path_buf(), read);
    });
    read
}

fn read_metadata(reader: &mut BufReader<File>) -> Option<ImageMetadata> {
    let (format, width, height) = crate::image_header::header(reader)?;
    let frames = if format.may_be_animated() {
        reader.rewind().ok()?;
        crate::image_header::frame_count(reader)
    } else {
        Some(1)
    };
    Some(ImageMetadata {
        format,
        width,
        height,
        frames,
    })
}

/// Forgets every file, e.g. on workspace sign-out.
pub(crate) fn clear() {
    MEMO.with(|memo| memo.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::animated_image::tests::{write_temp, ANIMATED_GIF};

    fn reads_during(probe: impl FnOnce()) -> usize {
        let before = HEADER_READS.with(std::cell::Cell::get);
        probe();
        HEADER_READS.with(std::cell::Cell::get) - before
    }

    #[test]
    fn perf_guard_repeated_binds_read_an_image_header_once() {
        let path = write_temp("memo-bind.gif", ANIMATED_GIF);
        let reads = reads_during(|| {
            // What every bind of an animated image asks.
            for _ in 0..5 {
                assert!(crate::animated_image::may_animate(&path));
                assert_eq!(metadata(&path).map(|m| (m.width, m.height)), Some((1, 1)));
            }
        });
        std::fs::remove_file(path).ok();
        assert_eq!(reads, 1, "only the first bind opens the file");
    }

    #[test]
    fn remembers_unknown_formats_but_not_missing_files() {
        let junk = write_temp("memo-junk.bin", b"not an image");
        let missing = junk.with_extension("missing");
        let reads = reads_during(|| {
            assert_eq!(metadata(&junk), None);
            assert_eq!(metadata(&junk), None);
            assert_eq!(metadata(&missing), None);
        });
        assert_eq!(reads, 1);
        std::fs::write(&missing, ANIMATED_GIF).expect("write image");
        let found = metadata(&missing).expect("a file written later is read");
        assert_eq!((found.format, found.frames), (ImageFormat::Gif, Some(2)));
        std::fs::remove_file(junk).ok();
        std::fs::remove_file(missing).ok();
    }
}
