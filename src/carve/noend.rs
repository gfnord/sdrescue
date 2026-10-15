//! Carvers for formats whose end cannot be found reliably: FLAC, AMR and TIFF
//! (including Canon CR2).
//!
//! Why there is no reliable end:
//! - FLAC is a stream of self-delimiting audio frames with no trailer; the
//!   STREAMINFO block records the sample count, but not the byte length, and the
//!   count may be 0 (unknown) for streamed encodes.
//! - AMR storage files are a magic line followed by frames with no length field
//!   covering the whole file and no end marker.
//! - TIFF (and CR2) consists of IFDs and data strips that may be placed
//!   anywhere, in any order, so the largest offset seen in the headers says
//!   nothing about where the file stops.
//!
//! The carvers here only validate the header (so random sectors are rejected)
//! and return [`Carved::next_header`] with the provisional length up to the
//! limit. The engine then cuts the file at the next candidate header (or at the
//! size cap / end of the image), and the report marks these files `next-header`:
//! they may contain trailing bytes of whatever followed on the card.

use super::mp3::id3v2_len;
use super::{Carved, Carver, Input, Signature};
use crate::filetype::Category;

/// Longest chain of FLAC metadata blocks that is followed before giving up.
const MAX_FLAC_BLOCKS: usize = 1024;

/// Reads a 24-bit big-endian value.
fn be24(b: &[u8]) -> Option<u32> {
    let b: &[u8; 3] = b.get(..3)?.try_into().ok()?;
    Some(u32::from_be_bytes([0, b[0], b[1], b[2]]))
}

/// FLAC audio, optionally preceded by an ID3v2 tag.
///
/// Structure: `fLaC`, then metadata blocks (`last:1 type:7 length:24`) with
/// STREAMINFO (type 0, 34 bytes) first, then audio frames starting with the
/// 14-bit sync `FFF8`/`FFF9`.
pub struct Flac;

const FLAC_SIGS: &[Signature] = &[Signature::at0(b"fLaC"), Signature::at0(b"ID3")];

/// Checks the 34-byte STREAMINFO body.
fn streaminfo_ok(b: &[u8]) -> bool {
    let (Some(min_block), Some(max_block)) = (
        b.get(0..2)
            .and_then(|s| s.try_into().ok())
            .map(u16::from_be_bytes),
        b.get(2..4)
            .and_then(|s| s.try_into().ok())
            .map(u16::from_be_bytes),
    ) else {
        return false;
    };
    let (Some(&b10), Some(&b11), Some(&b12), Some(&b13)) =
        (b.get(10), b.get(11), b.get(12), b.get(13))
    else {
        return false;
    };
    let sample_rate = (u32::from(b10) << 12) | (u32::from(b11) << 4) | u32::from(b12 >> 4);
    let bits_per_sample = ((u32::from(b12 & 1) << 4) | u32::from(b13 >> 4)) + 1;
    max_block >= 16 && min_block <= max_block && sample_rate != 0 && bits_per_sample >= 4
}

/// Checks that `buf` starts with a STREAMINFO block header (and body, if present).
fn first_block_ok(buf: &[u8]) -> bool {
    let Some(h) = buf.get(..4) else { return false };
    h[0] & 0x7F == 0 && be24(&h[1..]) == Some(34) && buf.get(4..38).is_none_or(streaminfo_ok)
}

impl Carver for Flac {
    fn name(&self) -> &'static str {
        "flac"
    }

    fn signatures(&self) -> &'static [Signature] {
        FLAC_SIGS
    }

    fn categories(&self) -> &'static [Category] {
        &[Category::Audio]
    }

    fn identify(&self, head: &[u8]) -> Option<&'static str> {
        let at = if head.starts_with(b"ID3") {
            id3v2_len(head)?
        } else {
            0
        };
        let magic_end = at.checked_add(4)?;
        if head.get(at..magic_end)? != b"fLaC" {
            return None;
        }
        first_block_ok(head.get(magic_end..)?).then_some("flac")
    }

    fn carve(&self, input: &Input, start: u64, limit: u64) -> Option<Carved> {
        let mut pos = start;
        if let Some(h) = input.array::<10>(start).filter(|h| h.starts_with(b"ID3")) {
            pos = pos.checked_add(u64::try_from(id3v2_len(&h)?).ok()?)?;
        }
        if pos.checked_add(4)? > limit || input.array::<4>(pos)? != *b"fLaC" {
            return None;
        }
        pos += 4;
        let mut first = true;
        let mut blocks = 0;
        loop {
            let h = input.array::<4>(pos)?;
            let kind = h[0] & 0x7F;
            let len = u64::from(be24(&h[1..])?);
            if kind == 127 || (first && (kind != 0 || len != 34)) {
                return None;
            }
            if first {
                let info = input.bytes(pos + 4, 34)?;
                if !streaminfo_ok(&info) {
                    return None;
                }
                first = false;
            }
            pos = pos.checked_add(4 + len)?;
            if h[0] & 0x80 != 0 {
                break;
            }
            blocks += 1;
            if blocks >= MAX_FLAC_BLOCKS || pos >= limit {
                break;
            }
        }
        // The first audio frame follows the last metadata block.
        if pos.checked_add(2)? <= limit && input.be16(pos).is_some_and(|s| s & 0xFFFE != 0xFFF8) {
            return None;
        }
        Some(Carved::next_header(limit - start, "flac"))
    }
}

/// AMR / AMR-WB storage files.
///
/// Structure: magic line `#!AMR\n` (narrow band) or `#!AMR-WB\n` (wide band),
/// then frames each starting with a one-byte header `0 FT:4 Q:1 00` whose frame
/// type gives the frame size.
pub struct Amr;

const AMR_SIGS: &[Signature] = &[Signature::at0(b"#!AMR")];

/// Frame sizes in bytes (header byte included) per frame type, 0 = invalid.
const AMR_NB_SIZES: [u8; 16] = [13, 14, 16, 18, 20, 21, 27, 32, 6, 0, 0, 0, 0, 0, 0, 1];
const AMR_WB_SIZES: [u8; 16] = [18, 24, 33, 37, 41, 47, 51, 59, 61, 6, 0, 0, 0, 0, 1, 1];

/// Frames whose headers are checked.
const AMR_CHECKED_FRAMES: usize = 4;

/// Wideband flag and magic length, if `head` starts with an AMR magic line.
fn amr_magic(head: &[u8]) -> Option<(bool, usize)> {
    if head.starts_with(b"#!AMR\n") {
        Some((false, 6))
    } else if head.starts_with(b"#!AMR-WB\n") {
        Some((true, 9))
    } else {
        None
    }
}

/// Validates the first frame headers; `byte_at(i)` reads the byte `i` bytes
/// after the magic. The first header must be readable; later ones only have to
/// be valid when present.
fn amr_frames_ok(wide: bool, byte_at: impl Fn(usize) -> Option<u8>) -> bool {
    let sizes = if wide { &AMR_WB_SIZES } else { &AMR_NB_SIZES };
    let mut off = 0usize;
    for n in 0..AMR_CHECKED_FRAMES {
        let Some(b) = byte_at(off) else {
            return n > 0;
        };
        let size = sizes[usize::from((b >> 3) & 0x0F)];
        if b & 0x83 != 0 || size == 0 {
            return false;
        }
        off += usize::from(size);
    }
    true
}

impl Carver for Amr {
    fn name(&self) -> &'static str {
        "amr"
    }

    fn signatures(&self) -> &'static [Signature] {
        AMR_SIGS
    }

    fn categories(&self) -> &'static [Category] {
        &[Category::Audio]
    }

    fn identify(&self, head: &[u8]) -> Option<&'static str> {
        let (wide, magic) = amr_magic(head)?;
        amr_frames_ok(wide, |i| head.get(magic.checked_add(i)?).copied()).then_some("amr")
    }

    fn carve(&self, input: &Input, start: u64, limit: u64) -> Option<Carved> {
        let magic = input.array::<9>(start)?;
        let (wide, magic_len) = amr_magic(&magic)?;
        let first = start.checked_add(magic_len as u64)?;
        let ok = amr_frames_ok(wide, |i| {
            let at = first.checked_add(i as u64)?;
            if at >= limit { None } else { input.u8(at) }
        });
        ok.then(|| Carved::next_header(limit - start, "amr"))
    }
}

/// TIFF images and Canon CR2 raw files.
///
/// Structure: byte-order mark (`II`/`MM`), magic 42, offset of the first IFD,
/// whose first two bytes are the entry count and which holds 12-byte entries
/// (tag, type, count, value/offset). CR2 is a little-endian TIFF with `CR` and
/// a version at bytes 8..10 right after the header.
pub struct Tiff;

const TIFF_SIGS: &[Signature] = &[Signature::at0(b"II*\0"), Signature::at0(b"MM\0*")];

/// Largest first-IFD offset considered plausible.
const MAX_IFD_OFFSET: u32 = 64 << 20;
/// Largest plausible number of entries in the first IFD.
const MAX_IFD_ENTRIES: u16 = 1000;

/// Byte order and first-IFD offset from an 8-byte TIFF header.
fn tiff_header(h: &[u8]) -> Option<(bool, u32)> {
    let le = h.starts_with(b"II*\0");
    if !le && !h.starts_with(b"MM\0*") {
        return None;
    }
    let off: [u8; 4] = h.get(4..8)?.try_into().ok()?;
    let off = if le {
        u32::from_le_bytes(off)
    } else {
        u32::from_be_bytes(off)
    };
    (8..MAX_IFD_OFFSET).contains(&off).then_some((le, off))
}

/// Checks the start of an IFD (`ifd` = its first bytes: count, then entries).
fn ifd_ok(le: bool, ifd: &[u8]) -> bool {
    let Some(count) = ifd.get(..2).and_then(|c| c.try_into().ok()) else {
        return false;
    };
    let count = if le {
        u16::from_le_bytes(count)
    } else {
        u16::from_be_bytes(count)
    };
    if !(1..=MAX_IFD_ENTRIES).contains(&count) {
        return false;
    }
    // First entry's field type: BYTE..IFD (1..=13) plus the BigTIFF/extra types.
    match ifd.get(4..6).and_then(|t| t.try_into().ok()) {
        Some(t) => {
            let t = if le {
                u16::from_le_bytes(t)
            } else {
                u16::from_be_bytes(t)
            };
            (1..=18).contains(&t)
        }
        None => true,
    }
}

/// `cr2` for Canon raw files, `tif` otherwise.
fn tiff_ext(le: bool, h: &[u8]) -> &'static str {
    if le && h.get(8..10) == Some(b"CR") {
        "cr2"
    } else {
        "tif"
    }
}

impl Carver for Tiff {
    fn name(&self) -> &'static str {
        "tiff"
    }

    fn signatures(&self) -> &'static [Signature] {
        TIFF_SIGS
    }

    fn categories(&self) -> &'static [Category] {
        &[Category::Photo]
    }

    fn identify(&self, head: &[u8]) -> Option<&'static str> {
        let (le, ifd) = tiff_header(head)?;
        // The IFD is only checked when it lies inside the head.
        if let Some(rest) = head.get(usize::try_from(ifd).ok()?..)
            && !rest.is_empty()
            && !ifd_ok(le, rest.get(..14).unwrap_or(rest))
        {
            return None;
        }
        Some(tiff_ext(le, head))
    }

    fn carve(&self, input: &Input, start: u64, limit: u64) -> Option<Carved> {
        let mut h = [0u8; 16];
        let n = input.read(
            start,
            &mut h[..(limit.checked_sub(start)? as usize).min(16)],
        );
        let h = &h[..n];
        let (le, ifd) = tiff_header(h)?;
        let at = start.checked_add(u64::from(ifd))?;
        let mut buf = [0u8; 14];
        let want = (limit.checked_sub(at)? as usize).min(buf.len());
        let n = input.read(at, &mut buf[..want]);
        if !ifd_ok(le, &buf[..n]) {
            return None;
        }
        Some(Carved::next_header(limit - start, tiff_ext(le, h)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carve::EndMethod;
    use crate::carve::mp3::Mp3;
    use crate::carve::testutil::{carve_bytes, carve_with_limit};

    fn streaminfo() -> Vec<u8> {
        let mut b = vec![0u8; 34];
        b[0..2].copy_from_slice(&4096u16.to_be_bytes());
        b[2..4].copy_from_slice(&4096u16.to_be_bytes());
        // 44100 Hz (20 bits), 2 channels, 16 bits per sample.
        let rate = 44100u32;
        b[10] = (rate >> 12) as u8;
        b[11] = (rate >> 4) as u8;
        b[12] = ((rate & 0xF) << 4) as u8 | (1 << 1);
        b[13] = 15 << 4;
        b
    }

    fn flac() -> Vec<u8> {
        let mut v = b"fLaC".to_vec();
        v.extend([0x00, 0, 0, 34]);
        v.extend(streaminfo());
        v.extend([0x84, 0, 0, 8]); // last block: VORBIS_COMMENT (type 4), 8 bytes
        v.extend([0u8; 8]);
        v.extend([0xFF, 0xF8, 0xC9, 0x18]);
        v.resize(v.len() + 3000, 0x42);
        v
    }

    fn id3v2(body_len: u8) -> Vec<u8> {
        let mut v = b"ID3\x04\x00\x00\x00\x00\x00".to_vec();
        v.push(body_len);
        v.resize(10 + usize::from(body_len), 0);
        v
    }

    #[test]
    fn flac_is_next_header() {
        let data = flac();
        let c = carve_bytes(&Flac, &data, 2000).unwrap();
        assert_eq!(c.ext, "flac");
        assert_eq!(c.end, EndMethod::NextHeader);
        assert_eq!(c.len, data.len() as u64 + 2000);
    }

    #[test]
    fn flac_after_id3_is_flac_not_mp3() {
        let mut data = id3v2(40);
        data.extend(flac());
        assert_eq!(carve_bytes(&Flac, &data, 100).unwrap().ext, "flac");
        assert!(carve_bytes(&Mp3, &data, 100).is_none());
    }

    #[test]
    fn flac_truncated_limit_is_applied() {
        let data = flac();
        let c = carve_with_limit(&Flac, &data, 1000).unwrap();
        assert_eq!(c.len, 1000);
        assert_eq!(c.end, EndMethod::NextHeader);
    }

    #[test]
    fn flac_rejects_garbage() {
        // First block is not STREAMINFO.
        let mut v = b"fLaC".to_vec();
        v.extend([0x01, 0, 0, 34]);
        v.extend(streaminfo());
        v.resize(3000, 0);
        assert!(carve_bytes(&Flac, &v, 0).is_none());
        // Wrong STREAMINFO length.
        let mut v = b"fLaC".to_vec();
        v.extend([0x00, 0, 0, 33]);
        v.resize(3000, 0);
        assert!(carve_bytes(&Flac, &v, 0).is_none());
        // Zero sample rate.
        let mut v = b"fLaC".to_vec();
        v.extend([0x00, 0, 0, 34]);
        v.extend([0u8; 34]);
        v.resize(3000, 0);
        assert!(carve_bytes(&Flac, &v, 0).is_none());
        // Reserved metadata block type 127 after STREAMINFO.
        let mut v = b"fLaC".to_vec();
        v.extend([0x00, 0, 0, 34]);
        v.extend(streaminfo());
        v.extend([0x7F, 0, 0, 4, 0, 0, 0, 0]);
        v.resize(3000, 0);
        assert!(carve_bytes(&Flac, &v, 0).is_none());
        // Audio frame sync missing after the metadata.
        let mut v = flac();
        v[4 + 4 + 34 + 4 + 8] = 0x12;
        assert!(carve_bytes(&Flac, &v, 0).is_none());
        // ID3 not followed by fLaC.
        let mut v = id3v2(10);
        v.extend([0x55; 500]);
        assert!(carve_bytes(&Flac, &v, 0).is_none());
        assert!(carve_bytes(&Flac, b"fLaC", 0).is_none());
    }

    fn amr_nb() -> Vec<u8> {
        let mut v = b"#!AMR\n".to_vec();
        for _ in 0..20 {
            v.push(0x3C); // FT 7 (12.2 kbit/s), quality bit set
            v.extend([0x11; 31]);
        }
        v
    }

    #[test]
    fn amr_nb_and_wb_are_next_header() {
        let data = amr_nb();
        let c = carve_bytes(&Amr, &data, 500).unwrap();
        assert_eq!(c.ext, "amr");
        assert_eq!(c.end, EndMethod::NextHeader);
        assert_eq!(c.len, data.len() as u64 + 500);

        let mut wb = b"#!AMR-WB\n".to_vec();
        for _ in 0..10 {
            wb.push(0x44); // FT 8 (23.85 kbit/s): 61 bytes
            wb.extend([0x22; 60]);
        }
        let c = carve_with_limit(&Amr, &wb, 300).unwrap();
        assert_eq!((c.ext, c.len), ("amr", 300));
    }

    #[test]
    fn amr_rejects_garbage() {
        assert!(carve_bytes(&Amr, b"#!AMR\n", 0).is_none()); // no frame
        assert!(carve_bytes(&Amr, b"#!AMRX\n", 100).is_none());
        // Invalid frame type 9 for narrow band / padding bits set.
        let mut v = b"#!AMR\n".to_vec();
        v.push(9 << 3);
        v.resize(200, 0);
        assert!(carve_bytes(&Amr, &v, 0).is_none());
        let mut v = b"#!AMR\n".to_vec();
        v.push(0x3C | 0x01);
        v.resize(200, 0);
        assert!(carve_bytes(&Amr, &v, 0).is_none());
        // Second frame header is invalid.
        let mut v = amr_nb();
        v[6 + 32] = 0xFF;
        assert!(carve_bytes(&Amr, &v, 0).is_none());
    }

    fn tiff(le: bool, ifd: u32, count: u16, tail: &[u8]) -> Vec<u8> {
        let u16b = |x: u16| if le { x.to_le_bytes() } else { x.to_be_bytes() };
        let u32b = |x: u32| if le { x.to_le_bytes() } else { x.to_be_bytes() };
        let mut v = if le {
            b"II*\0".to_vec()
        } else {
            b"MM\0*".to_vec()
        };
        v.extend(u32b(ifd));
        v.extend_from_slice(tail);
        v.resize(ifd as usize, 0);
        v.extend(u16b(count));
        v.extend(u16b(0x0100)); // ImageWidth
        v.extend(u16b(3)); // SHORT
        v.extend(u32b(1));
        v.extend(u32b(640));
        v.resize(v.len() + 500, 0);
        v
    }

    #[test]
    fn tiff_both_byte_orders() {
        for le in [true, false] {
            let data = tiff(le, 8, 12, &[]);
            let c = carve_bytes(&Tiff, &data, 700).unwrap();
            assert_eq!(c.ext, "tif");
            assert_eq!(c.end, EndMethod::NextHeader);
            assert_eq!(c.len, data.len() as u64 + 700);
        }
    }

    #[test]
    fn cr2_is_recognised() {
        let data = tiff(true, 16, 14, b"CR\x02\x00");
        assert_eq!(carve_bytes(&Tiff, &data, 100).unwrap().ext, "cr2");
        // Big-endian TIFF with the same bytes is not CR2.
        let data = tiff(false, 16, 14, b"CR\x02\x00");
        assert_eq!(carve_bytes(&Tiff, &data, 100).unwrap().ext, "tif");
    }

    #[test]
    fn tiff_ifd_beyond_head_is_accepted_when_readable() {
        let data = tiff(true, 20_000, 5, &[]);
        assert_eq!(carve_bytes(&Tiff, &data, 0).unwrap().ext, "tif");
        // Beyond the limit: the IFD cannot be validated.
        assert!(carve_with_limit(&Tiff, &data, 10_000).is_none());
    }

    #[test]
    fn tiff_truncated_limit_is_applied() {
        let data = tiff(true, 8, 12, &[]);
        let c = carve_with_limit(&Tiff, &data, 300).unwrap();
        assert_eq!(c.len, 300);
    }

    #[test]
    fn tiff_rejects_garbage() {
        assert!(carve_bytes(&Tiff, &tiff(true, 8, 0, &[]), 0).is_none()); // no entries
        assert!(carve_bytes(&Tiff, &tiff(true, 8, 5000, &[]), 0).is_none()); // too many
        assert!(carve_bytes(&Tiff, &tiff(false, 8, 0, &[]), 0).is_none());
        // IFD offset inside the header or absurdly large.
        let mut v = tiff(true, 8, 12, &[]);
        v[4..8].copy_from_slice(&4u32.to_le_bytes());
        assert!(carve_bytes(&Tiff, &v, 0).is_none());
        v[4..8].copy_from_slice(&0x7000_0000u32.to_le_bytes());
        assert!(carve_bytes(&Tiff, &v, 0).is_none());
        // Bad field type in the first entry.
        let mut v = tiff(true, 8, 12, &[]);
        v[12] = 99;
        assert!(carve_bytes(&Tiff, &v, 0).is_none());
        // Wrong magic.
        let mut v = tiff(true, 8, 12, &[]);
        v[2] = 43;
        assert!(carve_bytes(&Tiff, &v, 0).is_none());
        assert!(carve_bytes(&Tiff, b"II*\0", 0).is_none());
    }
}
