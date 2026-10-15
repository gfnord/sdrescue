//! ISO base media file format carver (MP4, MOV, M4V, 3GP, M4A, HEIC, CR3).
//!
//! Structure: a flat sequence of boxes `<size:u32 BE> <type:4 bytes> <payload>`.
//! `size` includes the 8-byte header; `size == 1` means a 64-bit size follows
//! the type (`<largesize:u64 BE>` at +8) and `size == 0` means the box extends
//! to the end of the file. Files normally start with `ftyp`, whose first four
//! payload bytes are the major brand (used to pick the extension). Old
//! QuickTime files have no `ftyp` and start with `moov` or `wide`.
//!
//! The carver walks the top-level boxes while their type is a known top-level
//! type and the size is possible. The file ends after the last valid box. A
//! file is accepted only if it contains `mdat` (media data) or `moov` (index);
//! with `mdat` but no `moov` the video index is missing and the file needs a
//! repair tool such as untrunc.

use super::{Carved, Carver, Input, Signature};
use crate::filetype::Category;

/// ISO-BMFF / QuickTime containers.
pub struct IsoBmff;

const SIGS: &[Signature] = &[
    Signature::at(4, b"ftyp"),
    Signature::at(4, b"moov"),
    Signature::at(4, b"wide"),
];

/// Top-level box types that may follow each other in a file.
const TOP_LEVEL: &[&[u8; 4]] = &[
    b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"udta", b"uuid", b"meta", b"moof",
    b"mfra", b"sidx", b"ssix", b"styp", b"pnot", b"pdin", b"prft", b"emsg", b"jumb", b"junk",
];

/// Box types a file can start with (the types we have signatures for, plus `mdat`).
const FIRST: &[&[u8; 4]] = &[b"ftyp", b"moov", b"wide", b"mdat"];

/// Safety cap on the number of top-level boxes walked per file.
const MAX_BOXES: u32 = 4_000_000;

const NO_MOOV: &str = "no moov box: the video index is missing; repair it with untrunc and a reference video from the same camera";
const OPEN_ENDED: &str = "last box has size 0 (extends to end of file): the real end is unknown";

fn is_top_level(kind: &[u8; 4]) -> bool {
    TOP_LEVEL.contains(&kind)
}

/// Extension for a major brand.
fn ext_for_brand(brand: &[u8; 4]) -> &'static str {
    match brand {
        b"qt  " => "mov",
        b"M4A " | b"M4B " => "m4a",
        b"heic" | b"heix" | b"mif1" | b"msf1" | b"heim" | b"heis" | b"hevc" => "heic",
        b"M4V " | b"M4VH" | b"M4VP" => "m4v",
        b"crx " => "cr3",
        [b'3', b'g', b'p' | b'2', _] => "3gp",
        _ => "mp4",
    }
}

fn printable(b: &[u8]) -> bool {
    b.iter().all(|c| (0x20..=0x7E).contains(c))
}

/// A parsed box header.
struct Header {
    kind: [u8; 4],
    /// Header length: 8, or 16 with a 64-bit size.
    header_len: u64,
    /// Total box size including the header; `None` for "to the end of the file".
    size: Option<u64>,
}

/// Reads a box header at `pos`; `None` if unreadable, of unknown type or with an impossible size.
fn read_header(input: &Input, pos: u64) -> Option<Header> {
    let raw = input.array::<8>(pos)?;
    let size32 = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let kind = [raw[4], raw[5], raw[6], raw[7]];
    if !is_top_level(&kind) {
        return None;
    }
    match size32 {
        0 => Some(Header {
            kind,
            header_len: 8,
            size: None,
        }),
        1 => {
            let big = input.be64(pos + 8)?;
            (big >= 16).then_some(Header {
                kind,
                header_len: 16,
                size: Some(big),
            })
        }
        2..=7 => None,
        n => Some(Header {
            kind,
            header_len: 8,
            size: Some(u64::from(n)),
        }),
    }
}

impl Carver for IsoBmff {
    fn name(&self) -> &'static str {
        "isobmff"
    }

    fn signatures(&self) -> &'static [Signature] {
        SIGS
    }

    fn categories(&self) -> &'static [Category] {
        &[Category::Photo, Category::Video, Category::Audio]
    }

    fn identify(&self, head: &[u8]) -> Option<&'static str> {
        let size = u32::from_be_bytes(head.get(..4)?.try_into().ok()?);
        let kind: &[u8; 4] = head.get(4..8)?.try_into().ok()?;
        match kind {
            b"ftyp" => {
                if !(16..=4096).contains(&size) || !size.is_multiple_of(4) {
                    return None;
                }
                // Major brand, minor version, then compatible brands (all ASCII).
                let brands = head.get(8..(size as usize).min(head.len()))?;
                let (major, rest) = brands.split_first_chunk::<4>()?;
                if !printable(major) {
                    return None;
                }
                let compat = rest.get(4..).unwrap_or_default();
                if !printable(compat) {
                    return None;
                }
                Some(ext_for_brand(major))
            }
            // Old QuickTime: `moov` starting with its `mvhd` (or compressed `cmov`) child.
            b"moov" => {
                let child = head.get(12..16)?;
                (size >= 16 && (child == b"mvhd" || child == b"cmov")).then_some("mov")
            }
            // Old QuickTime: an 8-byte `wide` placeholder in front of a 64-bit-capable `mdat`.
            b"wide" => {
                let next = head.get(12..16)?;
                (size == 8 && (next == b"mdat" || next == b"moov")).then_some("mov")
            }
            _ => None,
        }
    }

    fn carve(&self, input: &Input, start: u64, limit: u64) -> Option<Carved> {
        let first = read_header(input, start)?;
        if !FIRST.contains(&&first.kind) {
            return None;
        }
        let ext = if &first.kind == b"ftyp" {
            if first.size? < 16 {
                return None;
            }
            ext_for_brand(&input.array::<4>(start + first.header_len)?)
        } else {
            "mov"
        };

        let mut pos = start;
        let mut seen_moov = false;
        let mut seen_mdat = false;
        for _ in 0..MAX_BOXES {
            let Some(h) = read_header(input, pos) else {
                break;
            };
            match &h.kind {
                b"moov" => seen_moov = true,
                b"mdat" => seen_mdat = true,
                _ => {}
            }
            let seen = seen_moov || seen_mdat;
            let Some(size) = h.size else {
                // Box runs to the end of the file.
                if !seen {
                    return None;
                }
                let mut note = OPEN_ENDED.to_string();
                if !seen_moov {
                    note = format!("{NO_MOOV}; {note}");
                }
                return Some(Carved::truncated(limit.saturating_sub(start), ext).with_note(note));
            };
            let Some(end) = pos.checked_add(size) else {
                break;
            };
            if end > limit {
                return seen.then(|| Carved::truncated(limit.saturating_sub(start), ext));
            }
            pos = end;
        }

        if !(seen_moov || seen_mdat) {
            return None;
        }
        let carved = Carved::structure(pos - start, ext);
        Some(if seen_moov {
            carved
        } else {
            carved.with_note(NO_MOOV)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carve::testutil::{carve_bytes, carve_with_limit};

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    fn ftyp(brand: &[u8; 4]) -> Vec<u8> {
        let mut p = brand.to_vec();
        p.extend([0, 0, 0, 0]);
        p.extend_from_slice(b"isom");
        p.extend_from_slice(brand);
        bx(b"ftyp", &p)
    }

    fn mp4(brand: &[u8; 4]) -> Vec<u8> {
        let mut v = ftyp(brand);
        v.extend(bx(b"mdat", &[0x55; 3000]));
        v.extend(bx(b"moov", &bx(b"mvhd", &[0; 100])));
        v
    }

    #[test]
    fn exact_end_with_trailing_garbage() {
        let mut data = mp4(b"isom");
        data.extend(bx(b"free", &[0; 20]));
        let c = carve_bytes(&IsoBmff, &data, 5000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert_eq!(c.ext, "mp4");
        assert!(!c.truncated);
        assert_eq!(c.note, None);
    }

    #[test]
    fn moof_fragments_are_followed() {
        let mut data = ftyp(b"iso5");
        data.extend(bx(b"moov", &bx(b"mvhd", &[0; 100])));
        for _ in 0..3 {
            data.extend(bx(b"moof", &[1; 50]));
            data.extend(bx(b"mdat", &[2; 500]));
        }
        let c = carve_bytes(&IsoBmff, &data, 2000).unwrap();
        assert_eq!(c.len, data.len() as u64);
    }

    #[test]
    fn handles_64_bit_box_size() {
        let mut data = ftyp(b"isom");
        let payload = [0x77u8; 4000];
        data.extend(1u32.to_be_bytes());
        data.extend_from_slice(b"mdat");
        data.extend((payload.len() as u64 + 16).to_be_bytes());
        data.extend_from_slice(&payload);
        data.extend(bx(b"moov", &bx(b"mvhd", &[0; 100])));
        let c = carve_bytes(&IsoBmff, &data, 3000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert!(!c.truncated);
    }

    #[test]
    fn rejects_impossible_64_bit_size() {
        let mut data = ftyp(b"isom");
        data.extend(1u32.to_be_bytes());
        data.extend_from_slice(b"mdat");
        data.extend(9u64.to_be_bytes());
        data.extend([0; 100]);
        assert!(carve_bytes(&IsoBmff, &data, 100).is_none());
    }

    #[test]
    fn missing_moov_gets_note() {
        let mut data = ftyp(b"mp42");
        data.extend(bx(b"mdat", &[9; 2000]));
        let c = carve_bytes(&IsoBmff, &data, 4000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert!(!c.truncated);
        assert!(c.note.unwrap().contains("no moov box"));
    }

    #[test]
    fn size_zero_mdat_runs_to_limit() {
        let mut data = ftyp(b"mp42");
        data.extend(0u32.to_be_bytes());
        data.extend_from_slice(b"mdat");
        data.extend([3; 5000]);
        let c = carve_with_limit(&IsoBmff, &data, data.len() as u64).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert!(c.truncated);
        let note = c.note.unwrap();
        assert!(note.contains("no moov box"));
        assert!(note.contains("size 0"));
    }

    #[test]
    fn brand_selects_extension() {
        let cases: &[(&[u8; 4], &str)] = &[
            (b"qt  ", "mov"),
            (b"M4A ", "m4a"),
            (b"heic", "heic"),
            (b"mif1", "heic"),
            (b"3gp4", "3gp"),
            (b"3g2a", "3gp"),
            (b"M4V ", "m4v"),
            (b"crx ", "cr3"),
            (b"isom", "mp4"),
            (b"mp42", "mp4"),
        ];
        for (brand, ext) in cases {
            let c = carve_bytes(&IsoBmff, &mp4(brand), 100).unwrap();
            assert_eq!(c.ext, *ext, "brand {:?}", String::from_utf8_lossy(*brand));
        }
    }

    #[test]
    fn quicktime_without_ftyp() {
        let mut data = bx(b"wide", &[]);
        data.extend(bx(b"mdat", &[1; 1000]));
        data.extend(bx(b"moov", &bx(b"mvhd", &[0; 100])));
        let c = carve_bytes(&IsoBmff, &data, 3000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert_eq!(c.ext, "mov");

        let moov_first = bx(b"moov", &bx(b"mvhd", &[0; 100]));
        let c = carve_bytes(&IsoBmff, &moov_first, 3000).unwrap();
        assert_eq!(c.len, moov_first.len() as u64);
        assert_eq!(c.ext, "mov");
    }

    #[test]
    fn truncated_at_limit() {
        let mut data = ftyp(b"isom");
        data.extend(bx(b"mdat", &vec![0x42; 10_000]));
        data.extend(bx(b"moov", &bx(b"mvhd", &[0; 100])));
        let c = carve_with_limit(&IsoBmff, &data, 3000).unwrap();
        assert_eq!(c.len, 3000);
        assert!(c.truncated);
        assert_eq!(c.ext, "mp4");
    }

    #[test]
    fn rejects_ftyp_only_and_garbage() {
        assert!(carve_bytes(&IsoBmff, &ftyp(b"isom"), 3000).is_none());
        let mut unknown_next = ftyp(b"isom");
        unknown_next.extend(bx(b"abcd", &[0; 100]));
        assert!(carve_bytes(&IsoBmff, &unknown_next, 100).is_none());
        assert!(carve_bytes(&IsoBmff, &[0u8; 64], 3000).is_none());
        // Bad brand characters / sizes in ftyp.
        let mut bad_brand = mp4(b"isom");
        bad_brand[8] = 0x01;
        assert!(carve_bytes(&IsoBmff, &bad_brand, 0).is_none());
        let mut bad_size = mp4(b"isom");
        bad_size[..4].copy_from_slice(&13u32.to_be_bytes());
        assert!(carve_bytes(&IsoBmff, &bad_size, 0).is_none());
        // moov that does not start with mvhd is not an old QuickTime header.
        assert!(carve_bytes(&IsoBmff, &bx(b"moov", &bx(b"abcd", &[0; 100])), 0).is_none());
    }
}
