//! Signature-based file carving.
//!
//! Each format lives in its own module and implements [`Carver`]. The
//! [`engine`] finds candidate headers at 512-byte-aligned offsets (pass 1)
//! and then asks the matching carver where each file ends (pass 2).

pub mod engine;
pub mod reader;

mod asf;
mod gif;
mod isobmff;
mod jpeg;
mod mp3;
mod ogg;
mod png;
mod riff;

use std::fmt;

use crate::filetype::Category;
pub use reader::Input;

/// How many leading bytes [`Carver::identify`] receives (fewer at the end of the source).
pub const HEAD_LEN: usize = 4096;

/// A header pattern: `bytes` found `offset` bytes after the start of the file.
#[derive(Debug, Clone, Copy)]
pub struct Signature {
    /// Distance from the start of the file to the pattern.
    pub offset: usize,
    /// The pattern itself.
    pub bytes: &'static [u8],
}

impl Signature {
    /// Pattern at the very start of the file.
    pub const fn at0(bytes: &'static [u8]) -> Signature {
        Signature { offset: 0, bytes }
    }

    /// Pattern `offset` bytes into the file.
    pub const fn at(offset: usize, bytes: &'static [u8]) -> Signature {
        Signature { offset, bytes }
    }
}

/// How the end of a carved file was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndMethod {
    /// By walking the format's own structure (segments, chunks, boxes, frames...).
    Structure,
    /// The format has no reliable end: the file ends at the next candidate
    /// header (or at the size cap / end of the image).
    NextHeader,
}

impl fmt::Display for EndMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EndMethod::Structure => "structure",
            EndMethod::NextHeader => "next-header",
        })
    }
}

/// Result of carving one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Carved {
    /// Length in bytes from the start offset.
    pub len: u64,
    /// File extension to use (lower case, no dot).
    pub ext: &'static str,
    /// How the end was found.
    pub end: EndMethod,
    /// The structure walk hit the size cap or the end of the image before the
    /// file's natural end, so the file is probably incomplete.
    pub truncated: bool,
    /// Extra remark for the report (e.g. "no moov box: needs repair with untrunc").
    pub note: Option<String>,
}

impl Carved {
    /// A file whose end was found by walking its structure.
    pub fn structure(len: u64, ext: &'static str) -> Carved {
        Carved {
            len,
            ext,
            end: EndMethod::Structure,
            truncated: false,
            note: None,
        }
    }

    /// A file cut at `limit` before its structure ended.
    pub fn truncated(len: u64, ext: &'static str) -> Carved {
        Carved {
            truncated: true,
            ..Carved::structure(len, ext)
        }
    }

    /// A file with no reliable end; `len` is provisional (up to the limit) and
    /// the engine shortens it to the next candidate header.
    pub fn next_header(len: u64, ext: &'static str) -> Carved {
        Carved {
            end: EndMethod::NextHeader,
            ..Carved::structure(len, ext)
        }
    }

    /// Adds a note for the report.
    pub fn with_note(mut self, note: impl Into<String>) -> Carved {
        self.note = Some(note.into());
        self
    }
}

/// One file format that can be carved.
///
/// Implementations must never panic on garbage: every length and offset read
/// from the data has to be bounds-checked, and `None` returned when the data
/// does not look like the format.
pub trait Carver: Send + Sync {
    /// Short format name, e.g. `"jpeg"`.
    fn name(&self) -> &'static str;

    /// Header patterns that start a candidate.
    fn signatures(&self) -> &'static [Signature];

    /// Categories this carver can produce (used to skip carvers excluded by `--type`).
    fn categories(&self) -> &'static [Category];

    /// Pass 1: quick check of the first bytes (`head` is up to [`HEAD_LEN`]
    /// bytes). Returns the extension the file would get, or `None` to reject.
    fn identify(&self, head: &[u8]) -> Option<&'static str>;

    /// Pass 2: determines where the file starting at absolute offset `start`
    /// ends. The file must not extend past `limit` (the size cap or the end of
    /// the image); when its structure continues beyond `limit`, return a
    /// [`Carved::truncated`] of `limit - start` bytes. `None` rejects the candidate.
    fn carve(&self, input: &Input, start: u64, limit: u64) -> Option<Carved>;
}

/// Every built-in carver.
pub fn all_carvers() -> Vec<Box<dyn Carver>> {
    vec![
        Box::new(jpeg::Jpeg),
        Box::new(png::Png),
        Box::new(gif::Gif),
        Box::new(isobmff::IsoBmff),
        Box::new(riff::Riff),
        Box::new(mp3::Mp3),
        Box::new(ogg::Ogg),
        Box::new(asf::Asf),
    ]
}

/// Test helpers shared by the format modules.
#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use crate::source::MemSource;

    /// Runs `identify` + `carve` on `data` placed at offset 0, followed by
    /// `trailing` bytes of garbage. `limit` defaults to the end of the data.
    pub fn carve_bytes(c: &dyn Carver, data: &[u8], trailing: usize) -> Option<Carved> {
        let mut all = data.to_vec();
        all.extend((0..trailing).map(|i| (i * 7 + 13) as u8));
        let src = MemSource(all);
        let head = &src.0[..src.0.len().min(HEAD_LEN)];
        c.identify(head)?;
        let inp = Input::new(&src);
        c.carve(&inp, 0, src.0.len() as u64)
    }

    /// Like [`carve_bytes`] with an explicit limit.
    pub fn carve_with_limit(c: &dyn Carver, data: &[u8], limit: u64) -> Option<Carved> {
        let src = MemSource(data.to_vec());
        let head = &src.0[..src.0.len().min(HEAD_LEN)];
        c.identify(head)?;
        let inp = Input::new(&src);
        c.carve(&inp, 0, limit)
    }
}
