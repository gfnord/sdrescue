//! File categories (photo / video / audio) and magic-number checks by extension.

use clap::ValueEnum;
use serde::Serialize;

/// File categories used by `--type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    /// jpg, png, heic, raw formats, ...
    Photo,
    /// mp4, mov, avi, mts, ...
    Video,
    /// mp3, wav, m4a, flac, ...
    Audio,
}

const PHOTO: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "heic", "heif", "webp", "tif", "tiff", "dng", "cr2", "cr3",
    "nef", "arw", "orf", "rw2", "raf",
];
const VIDEO: &[&str] = &[
    "mp4", "mov", "m4v", "3gp", "avi", "mts", "m2ts", "mpg", "mpeg", "mod", "wmv", "mkv",
];
const AUDIO: &[&str] = &[
    "mp3", "wav", "m4a", "aac", "ogg", "opus", "flac", "amr", "wma",
];

/// Lower-case extension of a file name (without the dot), if any.
pub fn extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// Category of an extension (case-insensitive).
pub fn category_of_ext(ext: &str) -> Option<Category> {
    let e = ext.to_ascii_lowercase();
    let e = e.as_str();
    if PHOTO.contains(&e) {
        Some(Category::Photo)
    } else if VIDEO.contains(&e) {
        Some(Category::Video)
    } else if AUDIO.contains(&e) {
        Some(Category::Audio)
    } else {
        None
    }
}

/// Category of a file name, from its extension.
pub fn category_of_name(name: &str) -> Option<Category> {
    extension(name).and_then(|e| category_of_ext(&e))
}

/// Result of comparing a file's first bytes with the magic number expected
/// for its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SignatureCheck {
    /// The data starts with the expected magic number.
    Ok,
    /// The data does not look like this kind of file.
    Mismatch,
    /// No known signature for this extension (or nothing to check).
    #[serde(rename = "n/a")]
    NotApplicable,
}

impl SignatureCheck {
    /// Text used in tables and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            SignatureCheck::Ok => "ok",
            SignatureCheck::Mismatch => "mismatch",
            SignatureCheck::NotApplicable => "n/a",
        }
    }
}

/// Number of leading bytes [`check_signature`] wants to see.
pub const SIGNATURE_PEEK: usize = 512;

fn is_iso_bmff(h: &[u8]) -> bool {
    h.get(4..8).is_some_and(|t| {
        matches!(
            t,
            b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free" | b"skip" | b"pnot"
        )
    })
}

fn is_riff(h: &[u8], form: &[u8; 4]) -> bool {
    h.starts_with(b"RIFF") && h.get(8..12) == Some(&form[..])
}

fn is_tiff(h: &[u8]) -> bool {
    h.starts_with(b"II*\0") || h.starts_with(b"MM\0*")
}

fn is_mpeg_audio_sync(h: &[u8]) -> bool {
    h.len() >= 2 && h[0] == 0xFF && h[1] & 0xE0 == 0xE0 && h[1] & 0x06 != 0
}

/// Compares `head` (the first bytes of the file data) with the magic number
/// expected for extension `ext`.
pub fn check_signature(ext: &str, head: &[u8]) -> SignatureCheck {
    let e = ext.to_ascii_lowercase();
    let ok = match e.as_str() {
        "jpg" | "jpeg" => head.starts_with(&[0xFF, 0xD8, 0xFF]),
        "png" => head.starts_with(b"\x89PNG\r\n\x1a\n"),
        "gif" => head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a"),
        "bmp" => head.starts_with(b"BM"),
        "webp" => is_riff(head, b"WEBP"),
        "avi" => is_riff(head, b"AVI "),
        "wav" => is_riff(head, b"WAVE"),
        "heic" | "heif" | "cr3" | "mp4" | "mov" | "m4v" | "3gp" | "m4a" => is_iso_bmff(head),
        "tif" | "tiff" | "dng" | "cr2" | "nef" | "arw" => is_tiff(head),
        "orf" => is_tiff(head) || head.starts_with(b"IIRO") || head.starts_with(b"IIRS"),
        "rw2" => head.starts_with(b"IIU\0") || is_tiff(head),
        "raf" => head.starts_with(b"FUJIFILMCCD-RAW"),
        "mts" | "m2ts" => head.get(4) == Some(&0x47) && head.get(196).is_none_or(|&b| b == 0x47),
        "mpg" | "mpeg" | "mod" => {
            head.starts_with(&[0, 0, 1, 0xBA]) || head.starts_with(&[0, 0, 1, 0xB3])
        }
        "wmv" | "wma" => head.starts_with(&[
            0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62,
            0xCE, 0x6C,
        ]),
        "mkv" => head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]),
        "mp3" => head.starts_with(b"ID3") || is_mpeg_audio_sync(head),
        "aac" => {
            head.starts_with(b"ID3")
                || (head.len() >= 2 && head[0] == 0xFF && head[1] & 0xF6 == 0xF0)
        }
        "ogg" | "opus" => head.starts_with(b"OggS"),
        "flac" => head.starts_with(b"fLaC") || head.starts_with(b"ID3"),
        "amr" => head.starts_with(b"#!AMR"),
        _ => return SignatureCheck::NotApplicable,
    };
    if ok {
        SignatureCheck::Ok
    } else {
        SignatureCheck::Mismatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_are_case_insensitive() {
        assert_eq!(category_of_name("IMG_0001.JPG"), Some(Category::Photo));
        assert_eq!(category_of_name("clip.MtS"), Some(Category::Video));
        assert_eq!(category_of_name("song.flac"), Some(Category::Audio));
        assert_eq!(category_of_name("notes.txt"), None);
        assert_eq!(category_of_name("noext"), None);
        assert_eq!(category_of_name(".jpg"), None);
    }

    #[test]
    fn signatures() {
        assert_eq!(
            check_signature("JPG", &[0xFF, 0xD8, 0xFF, 0xE1]),
            SignatureCheck::Ok
        );
        assert_eq!(
            check_signature("jpg", b"\0\0\0\0"),
            SignatureCheck::Mismatch
        );
        assert_eq!(
            check_signature("mp4", b"\0\0\0\x18ftypisom"),
            SignatureCheck::Ok
        );
        assert_eq!(
            check_signature("wav", b"RIFF\x10\0\0\0WAVEfmt "),
            SignatureCheck::Ok
        );
        assert_eq!(
            check_signature("mp3", &[0xFF, 0xFB, 0x90, 0x00]),
            SignatureCheck::Ok
        );
        assert_eq!(
            check_signature("txt", b"hello"),
            SignatureCheck::NotApplicable
        );
    }
}
