//! JPEG carver.
//!
//! Structure: `FFD8` (SOI) followed by marker segments `FF xx <len:u16 BE> <len-2 bytes>`.
//! APPn segments (including EXIF, which may embed a complete thumbnail JPEG
//! with its own `FFD8…FFD9`) are skipped using their length fields. After an
//! SOS segment comes entropy-coded data in which `FF00` (stuffed byte) and
//! `FFD0`–`FFD7` (restart markers) do not terminate; any other marker ends the
//! scan (progressive JPEGs have several scans), and `FFD9` (EOI) ends the file.

use super::{Carved, Carver, Input, Signature};
use crate::filetype::Category;

/// JPEG / JFIF / EXIF images.
pub struct Jpeg;

const SIGS: &[Signature] = &[Signature::at0(&[0xFF, 0xD8, 0xFF])];

/// What follows the entropy-coded data of a scan.
enum ScanEnd {
    /// EOI found; the file ends at this absolute offset.
    Eoi(u64),
    /// Another marker starts at this offset (e.g. the next progressive scan).
    Marker(u64),
    /// Reached the limit without an end.
    Limit,
}

/// Scans entropy-coded data from `pos` for the next real marker.
fn scan_entropy(input: &Input, pos: u64, limit: u64) -> ScanEnd {
    let mut carry_ff: Option<u64> = None;
    let found = input.scan(pos, limit, |base, chunk| {
        let mut i = 0usize;
        if let Some(ff_at) = carry_ff.take() {
            // The previous chunk ended with 0xFF.
            match chunk[0] {
                0x00 | 0xD0..=0xD7 => i = 1,
                0xFF => carry_ff = None,
                0xD9 => return Some(ScanEnd::Eoi(ff_at + 2)),
                _ => return Some(ScanEnd::Marker(ff_at)),
            }
        }
        while i < chunk.len() {
            let p = memchr::memchr(0xFF, &chunk[i..])?;
            let at = i + p;
            let Some(&next) = chunk.get(at + 1) else {
                carry_ff = Some(base + at as u64);
                return None;
            };
            match next {
                0x00 | 0xD0..=0xD7 => i = at + 2,
                0xFF => i = at + 1, // fill byte; the marker comes later
                0xD9 => return Some(ScanEnd::Eoi(base + at as u64 + 2)),
                _ => return Some(ScanEnd::Marker(base + at as u64)),
            }
        }
        None
    });
    found.unwrap_or(ScanEnd::Limit)
}

impl Carver for Jpeg {
    fn name(&self) -> &'static str {
        "jpeg"
    }

    fn signatures(&self) -> &'static [Signature] {
        SIGS
    }

    fn categories(&self) -> &'static [Category] {
        &[Category::Photo]
    }

    fn identify(&self, head: &[u8]) -> Option<&'static str> {
        if head.len() < 6 || !head.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return None;
        }
        let m = head[3];
        // First marker after SOI: APPn, DQT, DHT, SOFn, COM, DRI...
        let plausible = matches!(m, 0xC0..=0xCF | 0xDB..=0xDD | 0xE0..=0xEF | 0xFE);
        let len = u16::from_be_bytes([head[4], head[5]]);
        (plausible && len >= 2).then_some("jpg")
    }

    fn carve(&self, input: &Input, start: u64, limit: u64) -> Option<Carved> {
        let mut pos = start + 2;
        let mut seen_sos = false;
        let mut segments = 0u32;
        loop {
            if pos + 2 > limit {
                return seen_sos.then(|| Carved::truncated(limit - start, "jpg"));
            }
            if input.u8(pos)? != 0xFF {
                // Corrupt structure. Before any image data this is not a JPEG.
                return seen_sos.then(|| Carved::truncated(pos - start, "jpg"));
            }
            let marker = input.u8(pos + 1)?;
            match marker {
                0xFF => {
                    pos += 1; // fill byte
                    continue;
                }
                0xD9 => return seen_sos.then(|| Carved::structure(pos + 2 - start, "jpg")),
                0xD8 | 0x00 => {
                    return seen_sos.then(|| Carved::truncated(pos - start, "jpg"));
                }
                0x01 | 0xD0..=0xD7 => {
                    pos += 2;
                    continue;
                }
                _ => {}
            }
            let len = u64::from(input.be16(pos + 2)?);
            if len < 2 {
                return seen_sos.then(|| Carved::truncated(pos - start, "jpg"));
            }
            segments += 1;
            if segments > 100_000 {
                return None;
            }
            let seg_end = pos + 2 + len;
            if seg_end > limit {
                return seen_sos.then(|| Carved::truncated(limit - start, "jpg"));
            }
            if marker == 0xDA {
                seen_sos = true;
                match scan_entropy(input, seg_end, limit) {
                    ScanEnd::Eoi(end) => return Some(Carved::structure(end - start, "jpg")),
                    ScanEnd::Marker(m) => pos = m,
                    ScanEnd::Limit => return Some(Carved::truncated(limit - start, "jpg")),
                }
            } else {
                pos = seg_end;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carve::testutil::{carve_bytes, carve_with_limit};

    fn seg(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, marker];
        v.extend(((payload.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    /// A structurally valid baseline JPEG with an EXIF thumbnail in APP1.
    fn sample(entropy: &[u8]) -> Vec<u8> {
        let thumb = [
            0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x02, 0xFF, 0xDA, 0x00, 0x02, 0x11, 0xFF, 0xD9,
        ];
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&thumb);
        let mut v = vec![0xFF, 0xD8];
        v.extend(seg(0xE1, &exif));
        v.extend(seg(0xDB, &[0u8; 65]));
        v.extend(seg(0xC0, &[8, 0, 16, 0, 16, 1, 1, 0x11, 0]));
        v.extend(seg(0xDA, &[1, 1, 0, 0, 63, 0]));
        v.extend_from_slice(entropy);
        v.extend([0xFF, 0xD9]);
        v
    }

    #[test]
    fn finds_eoi_skipping_thumbnail_and_stuffing() {
        let data = sample(&[
            0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD3, 0x56, 0xFF, 0xFF, 0xD0, 0x99,
        ]);
        let c = carve_bytes(&Jpeg, &data, 5000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert_eq!(c.ext, "jpg");
        assert!(!c.truncated);
    }

    #[test]
    fn progressive_scans_continue() {
        let mut data = sample(&[0x11, 0x22]);
        data.truncate(data.len() - 2); // drop EOI
        data.extend(seg(0xC4, &[0u8; 20])); // DHT between scans
        data.extend(seg(0xDA, &[1, 1, 0, 0, 63, 0]));
        data.extend([0x33, 0xFF, 0x00, 0xFF, 0xD9]);
        let c = carve_bytes(&Jpeg, &data, 100).unwrap();
        assert_eq!(c.len, data.len() as u64);
    }

    #[test]
    fn truncated_at_limit() {
        let data = sample(&vec![0x42; 10_000]);
        let c = carve_with_limit(&Jpeg, &data, 2000).unwrap();
        assert_eq!(c.len, 2000);
        assert!(c.truncated);
    }

    #[test]
    fn rejects_garbage() {
        assert!(carve_bytes(&Jpeg, &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 1, 2, 3], 3000).is_none());
        assert!(carve_bytes(&Jpeg, &[0xFF, 0xD8, 0xFF, 0x00, 0x00], 10).is_none());
        assert!(carve_bytes(&Jpeg, &[0xFF, 0xD8, 0xFF], 0).is_none());
    }
}
