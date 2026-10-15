//! PNG carver.
//!
//! Structure: the 8-byte signature `89 50 4E 47 0D 0A 1A 0A` followed by chunks
//! `<length:u32 BE> <type:4 ASCII letters> <data:length> <crc:u32 BE>`. The first
//! chunk must be `IHDR` with a 13-byte payload; the file ends right after the
//! CRC of the `IEND` chunk. Chunk lengths are limited to 2^31 - 1 by the spec,
//! which is also used to reject garbage. The CRC (the zlib CRC-32 over type and
//! data) of `IHDR` is verified when identifying; later chunk CRCs are not
//! checked while walking, to keep carving cheap.

use super::{Carved, Carver, Input, Signature};
use crate::filetype::Category;

/// PNG images.
pub struct Png;

const MAGIC: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const SIGS: &[Signature] = &[Signature::at0(MAGIC)];

/// Largest chunk payload allowed by the PNG specification.
const MAX_CHUNK: u32 = 0x7FFF_FFFF;
/// Upper bound on the number of chunks walked for one file.
const MAX_CHUNKS: u32 = 4_000_000;
/// Length of the IHDR payload.
const IHDR_LEN: u32 = 13;

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

/// CRC-32 as used by PNG (reflected, polynomial 0xEDB88320).
fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |c, &b| {
        CRC_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8)
    })
}

/// Chunk header at `pos`: payload length and type.
fn chunk_header(input: &Input, pos: u64) -> Option<(u32, [u8; 4])> {
    let h = input.array::<8>(pos)?;
    let len = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
    Some((len, [h[4], h[5], h[6], h[7]]))
}

impl Carver for Png {
    fn name(&self) -> &'static str {
        "png"
    }

    fn signatures(&self) -> &'static [Signature] {
        SIGS
    }

    fn categories(&self) -> &'static [Category] {
        &[Category::Photo]
    }

    fn identify(&self, head: &[u8]) -> Option<&'static str> {
        // signature + length + "IHDR" + 13 bytes + crc
        let ihdr = head.get(8..33)?;
        if !head.starts_with(MAGIC) || ihdr[..4] != IHDR_LEN.to_be_bytes() || &ihdr[4..8] != b"IHDR"
        {
            return None;
        }
        let stored = u32::from_be_bytes([ihdr[21], ihdr[22], ihdr[23], ihdr[24]]);
        (crc32(&ihdr[4..21]) == stored).then_some("png")
    }

    fn carve(&self, input: &Input, start: u64, limit: u64) -> Option<Carved> {
        let mut pos = start + MAGIC.len() as u64;
        let (len, typ) = chunk_header(input, pos)?;
        if len != IHDR_LEN || &typ != b"IHDR" {
            return None;
        }
        for _ in 0..MAX_CHUNKS {
            if pos + 8 > limit {
                return Some(Carved::truncated(limit - start, "png"));
            }
            let Some((len, typ)) = chunk_header(input, pos) else {
                return Some(Carved::truncated(pos - start, "png"));
            };
            if len > MAX_CHUNK || !typ.iter().all(u8::is_ascii_alphabetic) {
                // Corrupt chunk: keep what was valid so far.
                return Some(Carved::truncated(pos - start, "png"));
            }
            let end = pos + 12 + u64::from(len);
            if end > limit {
                return Some(Carved::truncated(limit - start, "png"));
            }
            if &typ == b"IEND" {
                return Some(Carved::structure(end - start, "png"));
            }
            pos = end;
        }
        Some(Carved::truncated(pos - start, "png"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carve::testutil::{carve_bytes, carve_with_limit};

    fn chunk(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = (payload.len() as u32).to_be_bytes().to_vec();
        let mut body = typ.to_vec();
        body.extend_from_slice(payload);
        v.extend_from_slice(&body);
        v.extend(crc32(&body).to_be_bytes());
        v
    }

    fn ihdr() -> Vec<u8> {
        chunk(b"IHDR", &[0, 0, 0, 4, 0, 0, 0, 4, 8, 2, 0, 0, 0])
    }

    /// A structurally valid PNG with two IDAT chunks and an ancillary chunk.
    fn sample() -> Vec<u8> {
        let mut v = MAGIC.to_vec();
        v.extend(ihdr());
        v.extend(chunk(b"tEXt", b"Comment\0hello"));
        v.extend(chunk(b"IDAT", &[0x55; 3000]));
        v.extend(chunk(b"IDAT", &[0xAA; 100]));
        v.extend(chunk(b"IEND", &[]));
        v
    }

    #[test]
    fn crc_matches_known_value() {
        // Well-known CRC-32 check value.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        // CRC of an empty IEND chunk is a fixed constant in every PNG file.
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }

    #[test]
    fn finds_iend_before_trailing_garbage() {
        let data = sample();
        let c = carve_bytes(&Png, &data, 5000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert_eq!(c.ext, "png");
        assert!(!c.truncated);
    }

    #[test]
    fn truncated_at_limit() {
        let data = sample();
        let c = carve_with_limit(&Png, &data, 1000).unwrap();
        assert_eq!(c.len, 1000);
        assert!(c.truncated);
        // Limit just before the final CRC byte.
        let c = carve_with_limit(&Png, &data, data.len() as u64 - 1).unwrap();
        assert_eq!(c.len, data.len() as u64 - 1);
        assert!(c.truncated);
    }

    #[test]
    fn corrupt_chunk_after_ihdr_keeps_valid_prefix() {
        let mut data = MAGIC.to_vec();
        data.extend(ihdr());
        let good = data.len() as u64;
        data.extend([0, 0, 0, 4, 0x01, 0x02, 0x03, 0x04, 9, 9, 9, 9, 9, 9, 9, 9]);
        let c = carve_bytes(&Png, &data, 100).unwrap();
        assert_eq!(c.len, good);
        assert!(c.truncated);
    }

    #[test]
    fn rejects_garbage() {
        // Only the signature.
        assert!(carve_bytes(&Png, MAGIC, 3000).is_none());
        // IHDR with wrong length.
        let mut data = MAGIC.to_vec();
        data.extend(chunk(b"IHDR", &[0; 12]));
        assert!(carve_bytes(&Png, &data, 3000).is_none());
        // IHDR with bad CRC.
        let mut data = sample();
        data[8 + 8 + 13 + 3] ^= 0xFF;
        assert!(carve_bytes(&Png, &data, 10).is_none());
        // Wrong chunk type first.
        let mut data = MAGIC.to_vec();
        data.extend(chunk(b"IDAT", &[0; 13]));
        assert!(carve_bytes(&Png, &data, 3000).is_none());
    }

    #[test]
    fn oversized_chunk_length_is_not_followed() {
        let mut data = MAGIC.to_vec();
        data.extend(ihdr());
        let good = data.len() as u64;
        data.extend(0x8000_0000u32.to_be_bytes());
        data.extend(b"IDAT");
        let c = carve_bytes(&Png, &data, 100).unwrap();
        assert_eq!(c.len, good);
        assert!(c.truncated);
    }
}
