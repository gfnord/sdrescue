//! Ogg carver (Vorbis, Opus, FLAC-in-Ogg...).
//!
//! Structure: a sequence of pages, each `OggS <version:u8 = 0> <header type:u8>
//! <granule:u64> <serial:u32> <page sequence:u32> <crc:u32> <segments:u8>
//! <segment table: one length byte per segment> <segment data>`, so a page is
//! `27 + segments + sum(segment table)` bytes long. Header type bits: `0x01`
//! continued packet, `0x02` beginning of stream (BOS), `0x04` end of stream
//! (EOS); higher bits are reserved. The first page must be a BOS page with
//! sequence number 0. The CRC is CRC-32 with polynomial 0x04C11DB7, not
//! reflected, initial value 0, computed over the page with the CRC field
//! zeroed; it is verified for the first page when identifying.
//!
//! The file ends after the last page that is followed by something that is
//! not an Ogg page, or by the BOS page of another stream after data pages
//! (chained streams and back-to-back recordings are cut apart: every logical
//! stream group stays playable on its own). A stream whose first packet starts
//! with `OpusHead` gets the `opus` extension, everything else `ogg`.

use super::{Carved, Carver, Input, Signature};
use crate::filetype::Category;

/// Ogg bitstreams.
pub struct Ogg;

const SIGS: &[Signature] = &[Signature::at0(b"OggS")];

const HEADER_LEN: usize = 27;
const FLAG_CONTINUED: u8 = 0x01;
const FLAG_BOS: u8 = 0x02;
/// Upper bound on the number of pages walked for one file.
const MAX_PAGES: u32 = 1 << 26;

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = (n as u32) << 24;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ 0x04C1_1DB7
            } else {
                c << 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

/// Ogg page CRC of `page` (the CRC field at bytes 22..26 is treated as zero).
fn page_crc(page: &[u8]) -> u32 {
    page.iter().enumerate().fold(0u32, |c, (i, &b)| {
        let b = if (22..26).contains(&i) { 0 } else { b };
        (c << 8) ^ CRC_TABLE[(((c >> 24) as u8) ^ b) as usize]
    })
}

/// Parsed fixed part of a page header.
struct Page {
    flags: u8,
    segments: u8,
}

/// Validates the 27-byte page header `h`.
fn parse_header(h: &[u8; HEADER_LEN]) -> Option<Page> {
    if &h[..4] != b"OggS" || h[4] != 0 || h[5] & !0x07 != 0 {
        return None;
    }
    Some(Page {
        flags: h[5],
        segments: h[26],
    })
}

/// Extension for a stream whose first packet starts with `packet`.
fn ext_for(packet: &[u8]) -> &'static str {
    if packet.starts_with(b"OpusHead") {
        "opus"
    } else {
        "ogg"
    }
}

impl Carver for Ogg {
    fn name(&self) -> &'static str {
        "ogg"
    }

    fn signatures(&self) -> &'static [Signature] {
        SIGS
    }

    fn categories(&self) -> &'static [Category] {
        &[Category::Audio]
    }

    fn identify(&self, head: &[u8]) -> Option<&'static str> {
        let h: &[u8; HEADER_LEN] = head.get(..HEADER_LEN)?.try_into().ok()?;
        let page = parse_header(h)?;
        let seq = u32::from_le_bytes([h[18], h[19], h[20], h[21]]);
        if page.flags & FLAG_BOS == 0 || page.flags & FLAG_CONTINUED != 0 || seq != 0 {
            return None;
        }
        let table = head.get(HEADER_LEN..HEADER_LEN + usize::from(page.segments))?;
        let data_len: usize = table.iter().map(|&b| usize::from(b)).sum();
        let data_start = HEADER_LEN + table.len();
        // A BOS page is normally tiny; check its CRC when it fits in the head.
        if let Some(whole) = head.get(..data_start + data_len) {
            let stored = u32::from_le_bytes([h[22], h[23], h[24], h[25]]);
            if page_crc(whole) != stored {
                return None;
            }
        }
        Some(ext_for(head.get(data_start..)?))
    }

    fn carve(&self, input: &Input, start: u64, limit: u64) -> Option<Carved> {
        let first = input.array::<HEADER_LEN>(start)?;
        let page = parse_header(&first)?;
        if page.flags & FLAG_BOS == 0 {
            return None;
        }
        let packet = input.array::<8>(start + HEADER_LEN as u64 + u64::from(page.segments))?;
        let ext = ext_for(&packet);

        let mut pos = start;
        let mut in_data = false; // a non-BOS page has been seen
        for _ in 0..MAX_PAGES {
            if pos >= limit {
                break;
            }
            let Some(page) = input
                .array::<HEADER_LEN>(pos)
                .as_ref()
                .and_then(parse_header)
            else {
                return Some(Carved::structure(pos - start, ext));
            };
            if page.flags & FLAG_BOS != 0 && in_data {
                return Some(Carved::structure(pos - start, ext));
            }
            in_data |= page.flags & FLAG_BOS == 0;
            let seg_start = pos + HEADER_LEN as u64;
            let Some(table) = input.bytes(seg_start, usize::from(page.segments)) else {
                return Some(Carved::structure(pos - start, ext));
            };
            let data_len: u64 = table.iter().map(|&b| u64::from(b)).sum();
            let end = seg_start + u64::from(page.segments) + data_len;
            if end > limit {
                return Some(Carved::truncated(limit - start, ext));
            }
            pos = end;
        }
        Some(Carved::truncated(pos - start, ext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carve::testutil::{carve_bytes, carve_with_limit};

    /// Builds a page with the given flags, sequence number and packet bytes
    /// (split into 255-byte segments), with a correct CRC.
    fn page(flags: u8, seq: u32, serial: u32, data: &[u8]) -> Vec<u8> {
        let mut segs: Vec<u8> = data.chunks(255).map(|c| c.len() as u8).collect();
        if data.len().is_multiple_of(255) {
            segs.push(0);
        }
        let mut v = b"OggS".to_vec();
        v.extend([0, flags]);
        v.extend(0u64.to_le_bytes());
        v.extend(serial.to_le_bytes());
        v.extend(seq.to_le_bytes());
        v.extend([0; 4]);
        v.push(segs.len() as u8);
        v.extend(segs);
        v.extend_from_slice(data);
        let crc = page_crc(&v);
        v[22..26].copy_from_slice(&crc.to_le_bytes());
        v
    }

    fn vorbis_stream(serial: u32) -> Vec<u8> {
        let mut v = page(0x02, 0, serial, b"\x01vorbis\0\0\0\0\x02");
        v.extend(page(0, 1, serial, &[0x11; 600]));
        v.extend(page(0, 2, serial, &[0x22; 300]));
        v.extend(page(0x04, 3, serial, &[0x33; 50]));
        v
    }

    #[test]
    fn crc_matches_bitwise_definition() {
        // Straight bit-by-bit implementation of the Ogg CRC as the reference.
        let mut data: Vec<u8> = (0..300).map(|i| (i * 31 + 7) as u8).collect();
        data[22..26].fill(0);
        let mut crc = 0u32;
        for &b in &data {
            crc ^= u32::from(b) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04C1_1DB7
                } else {
                    crc << 1
                };
            }
        }
        // Whatever the CRC field holds must be ignored.
        data[22..26].fill(0xEE);
        assert_eq!(page_crc(&data), crc);
    }

    #[test]
    fn exact_end_before_trailing_garbage() {
        let data = vorbis_stream(7);
        let c = carve_bytes(&Ogg, &data, 5000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert_eq!(c.ext, "ogg");
        assert!(!c.truncated);
    }

    #[test]
    fn opus_stream_gets_opus_extension() {
        let mut data = page(0x02, 0, 1, b"OpusHead\x01\x02\x38\x01\x80\xbb\0\0\0\0\0");
        data.extend(page(0, 1, 1, b"OpusTags\x04\0\0\0test\0\0\0\0"));
        data.extend(page(0x04, 2, 1, &[0x44; 255])); // exactly one full segment + empty one
        let c = carve_bytes(&Ogg, &data, 3000).unwrap();
        assert_eq!(c.ext, "opus");
        assert_eq!(c.len, data.len() as u64);
    }

    #[test]
    fn muxed_streams_with_adjacent_bos_pages_stay_together() {
        let mut data = page(0x02, 0, 1, b"\x01vorbis\0\0\0\0\x02");
        data.extend(page(0x02, 0, 2, b"\x80theora\0\0\0\0\0"));
        data.extend(page(0, 1, 1, &[1; 100]));
        data.extend(page(0x04, 2, 1, &[2; 10]));
        let c = carve_bytes(&Ogg, &data, 100).unwrap();
        assert_eq!(c.len, data.len() as u64);
    }

    #[test]
    fn following_bos_page_starts_a_new_file() {
        let first = vorbis_stream(7);
        let mut data = first.clone();
        data.extend(vorbis_stream(8));
        let c = carve_bytes(&Ogg, &data, 0).unwrap();
        assert_eq!(c.len, first.len() as u64);
        assert!(!c.truncated);
    }

    #[test]
    fn truncated_at_limit() {
        let data = vorbis_stream(7);
        let c = carve_with_limit(&Ogg, &data, 500).unwrap();
        assert_eq!(c.len, 500);
        assert!(c.truncated);
    }

    #[test]
    fn partial_last_page_stops_before_it() {
        let mut data = vorbis_stream(7);
        let good = data.len() as u64;
        data.extend(b"OggS\0\0"); // not a full header
        let c = carve_bytes(&Ogg, &data, 0).unwrap();
        assert_eq!(c.len, good);
    }

    #[test]
    fn rejects_garbage() {
        // Missing BOS flag.
        assert!(carve_bytes(&Ogg, &page(0, 0, 1, &[1; 20]), 3000).is_none());
        // Continued-packet flag on the first page.
        assert!(carve_bytes(&Ogg, &page(0x03, 0, 1, &[1; 20]), 3000).is_none());
        // Non-zero sequence number.
        assert!(carve_bytes(&Ogg, &page(0x02, 5, 1, &[1; 20]), 3000).is_none());
        // Corrupted payload: CRC mismatch.
        let mut data = page(0x02, 0, 1, &[1; 20]);
        data[40] ^= 0xFF;
        assert!(carve_bytes(&Ogg, &data, 3000).is_none());
        // Bad version.
        let mut data = page(0x02, 0, 1, &[1; 20]);
        data[4] = 1;
        assert!(carve_bytes(&Ogg, &data, 3000).is_none());
        // Truncated header.
        assert!(carve_bytes(&Ogg, b"OggS\0\x02", 0).is_none());
    }
}
