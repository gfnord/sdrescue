//! RIFF carver (AVI, WAV, WebP).
//!
//! Structure: `RIFF <size:u32 LE> <form type:4>` followed by chunks, where
//! `size` counts everything after the size field, so the file occupies
//! `size + 8` bytes (plus one pad byte when `size` is odd, as RIFF chunks are
//! word aligned). The form type selects the extension: `AVI ` → `avi`,
//! `WAVE` → `wav`, `WEBP` → `webp`; other forms are not carved.
//!
//! The 32-bit size caps one RIFF chunk at 4 GiB, and AVI 1.0 indexes at 2 GiB.
//! Large AVIs written with OpenDML (and in practice most writers that split
//! near 1 GiB) continue in additional top-level `RIFF <size> AVIX` chunks that
//! directly follow the first one; those are joined into the same file.

use super::{Carved, Carver, Input, Signature};
use crate::filetype::Category;

/// RIFF containers: AVI video, WAV audio and WebP images.
pub struct Riff;

const SIGS: &[Signature] = &[Signature::at0(b"RIFF")];

/// Smallest first-segment length for which following `AVIX` chunks are
/// looked for. OpenDML writers start a new segment at about 1 GiB; the
/// threshold is lower so that segments of 1 GB or slightly less still match.
const AVIX_MIN_FIRST: u64 = 512 << 20;
/// Upper bound on the number of `AVIX` chunks joined to one file.
const MAX_AVIX: u32 = 1 << 16;

/// Extension for a RIFF form type.
fn form_ext(form: &[u8]) -> Option<&'static str> {
    match form {
        b"AVI " => Some("avi"),
        b"WAVE" => Some("wav"),
        b"WEBP" => Some("webp"),
        _ => None,
    }
}

/// Total length of the RIFF chunk whose size field is `size`, with pad byte.
fn total_len(size: u32) -> u64 {
    u64::from(size) + 8 + u64::from(size & 1)
}

/// Joins `RIFF <size> AVIX` chunks that directly follow offset `pos` (the end
/// of the previous segment). Returns the new end and whether the structure
/// ran past `limit`. Does nothing when `first_len` is below `threshold`.
fn join_avix(input: &Input, start: u64, first_len: u64, limit: u64, threshold: u64) -> (u64, bool) {
    let mut pos = start + first_len;
    if first_len < threshold {
        return (pos, false);
    }
    for _ in 0..MAX_AVIX {
        if pos >= limit {
            break;
        }
        let Some(h) = input.array::<12>(pos) else {
            break;
        };
        let size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        if &h[..4] != b"RIFF" || &h[8..] != b"AVIX" || size < 4 {
            break;
        }
        let end = pos + total_len(size);
        if end > limit {
            return (limit, true);
        }
        pos = end;
    }
    (pos, false)
}

impl Carver for Riff {
    fn name(&self) -> &'static str {
        "riff"
    }

    fn signatures(&self) -> &'static [Signature] {
        SIGS
    }

    fn categories(&self) -> &'static [Category] {
        &[Category::Photo, Category::Video, Category::Audio]
    }

    fn identify(&self, head: &[u8]) -> Option<&'static str> {
        let h = head.get(..16)?;
        let size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        // The first chunk id is four printable ASCII characters ("fmt ", "LIST", "VP8X"...).
        let chunk_id_ok = h[12..16].iter().all(|b| (0x20..0x7F).contains(b));
        if &h[..4] != b"RIFF" || size < 4 || !chunk_id_ok {
            return None;
        }
        form_ext(&h[8..12])
    }

    fn carve(&self, input: &Input, start: u64, limit: u64) -> Option<Carved> {
        let h = input.array::<12>(start)?;
        let size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        if &h[..4] != b"RIFF" || size < 4 {
            return None;
        }
        let ext = form_ext(&h[8..])?;
        let first_len = total_len(size);
        if start + first_len > limit {
            return Some(Carved::truncated(limit - start, ext));
        }
        if ext != "avi" {
            return Some(Carved::structure(first_len, ext));
        }
        let (end, over) = join_avix(input, start, first_len, limit, AVIX_MIN_FIRST);
        Some(if over {
            Carved::truncated(end - start, ext)
        } else {
            Carved::structure(end - start, ext)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carve::testutil::{carve_bytes, carve_with_limit};
    use crate::source::MemSource;

    /// `RIFF` chunk of the given form with `payload_len` bytes of sub-chunks
    /// (a single `data`-like chunk), including the pad byte for odd sizes.
    fn riff(form: &[u8; 4], first_chunk: &[u8; 4], payload_len: usize) -> Vec<u8> {
        let size = (4 + 8 + payload_len) as u32;
        let mut v = b"RIFF".to_vec();
        v.extend(size.to_le_bytes());
        v.extend(form);
        v.extend(first_chunk);
        v.extend((payload_len as u32).to_le_bytes());
        v.extend(vec![0x5A; payload_len]);
        if size & 1 == 1 {
            v.push(0);
        }
        v
    }

    #[test]
    fn wav_exact_end() {
        let data = riff(b"WAVE", b"data", 1000);
        let c = carve_bytes(&Riff, &data, 4000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert_eq!(c.ext, "wav");
        assert!(!c.truncated);
    }

    #[test]
    fn webp_with_odd_size_includes_pad_byte() {
        let data = riff(b"WEBP", b"VP8L", 1001);
        assert_eq!(data.len() % 2, 0);
        let c = carve_bytes(&Riff, &data, 1000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert_eq!(c.ext, "webp");
    }

    #[test]
    fn avi_exact_end() {
        let data = riff(b"AVI ", b"LIST", 5000);
        let c = carve_bytes(&Riff, &data, 3000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert_eq!(c.ext, "avi");
        assert!(!c.truncated);
    }

    #[test]
    fn truncated_at_limit() {
        let data = riff(b"AVI ", b"LIST", 5000);
        let c = carve_with_limit(&Riff, &data, 1024).unwrap();
        assert_eq!(c.len, 1024);
        assert!(c.truncated);
        assert_eq!(c.ext, "avi");
    }

    #[test]
    fn rejects_garbage() {
        // Unknown form type.
        assert!(carve_bytes(&Riff, &riff(b"ACON", b"anih", 100), 100).is_none());
        // Absurd size.
        let mut data = riff(b"WAVE", b"fmt ", 100);
        data[4..8].copy_from_slice(&2u32.to_le_bytes());
        assert!(carve_bytes(&Riff, &data, 100).is_none());
        // Non-text first chunk id.
        let mut data = riff(b"WAVE", b"fmt ", 100);
        data[12] = 0x01;
        assert!(carve_bytes(&Riff, &data, 100).is_none());
        // Too short / wrong magic.
        assert!(carve_bytes(&Riff, b"RIFF\x10\0\0\0WAVE", 0).is_none());
        let mut data = riff(b"WAVE", b"fmt ", 100);
        data[0] = b'X';
        assert!(carve_bytes(&Riff, &data, 100).is_none());
    }

    /// A small "first segment" followed by AVIX chunks, then garbage.
    fn avi_with_avix(avix_chunks: usize) -> (Vec<u8>, u64, u64) {
        let first = riff(b"AVI ", b"LIST", 400);
        let first_len = first.len() as u64;
        let mut data = first;
        for _ in 0..avix_chunks {
            data.extend(riff(b"AVIX", b"LIST", 301)); // odd size: padded
        }
        let end = data.len() as u64;
        data.extend((0..2000).map(|i| (i * 5 + 1) as u8));
        (data, first_len, end)
    }

    #[test]
    fn avix_chunks_are_joined() {
        let (data, first_len, end) = avi_with_avix(3);
        let src = MemSource(data);
        let inp = Input::new(&src);
        let (got, over) = join_avix(&inp, 0, first_len, src.0.len() as u64, 100);
        assert_eq!(got, end);
        assert!(!over);
    }

    #[test]
    fn avix_below_threshold_is_ignored() {
        let (data, first_len, _) = avi_with_avix(2);
        let src = MemSource(data);
        let inp = Input::new(&src);
        let (got, over) = join_avix(&inp, 0, first_len, src.0.len() as u64, first_len + 1);
        assert_eq!(got, first_len);
        assert!(!over);
    }

    #[test]
    fn avix_past_limit_is_truncated() {
        let (data, first_len, end) = avi_with_avix(3);
        let src = MemSource(data);
        let inp = Input::new(&src);
        let limit = end - 100;
        let (got, over) = join_avix(&inp, 0, first_len, limit, 100);
        assert_eq!(got, limit);
        assert!(over);
    }

    #[test]
    fn avix_without_followers_ends_at_first_segment() {
        let (data, first_len, _) = avi_with_avix(0);
        let src = MemSource(data);
        let inp = Input::new(&src);
        let (got, over) = join_avix(&inp, 0, first_len, src.0.len() as u64, 100);
        assert_eq!(got, first_len);
        assert!(!over);
    }
}
