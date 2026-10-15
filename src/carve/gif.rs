//! GIF carver.
//!
//! Structure: header `GIF87a`/`GIF89a` (6 bytes), logical screen descriptor
//! (7 bytes; bit 7 of the packed byte at offset 4 announces a global color
//! table of `3 * 2^((packed & 7) + 1)` bytes), then a sequence of blocks:
//!
//! - `0x21 <label> <sub-blocks>`: extension (graphic control, comment,
//!   application...);
//! - `0x2C <9-byte image descriptor> [local color table] <LZW min code size>
//!   <sub-blocks>`: one image frame; bit 7 of the descriptor's packed byte
//!   announces the local color table, sized like the global one;
//! - `0x3B`: trailer, the end of the file.
//!
//! Sub-blocks are `<length:u8> <length bytes>` sequences ended by a zero
//! length. Any other block introducer is corruption.

use super::{Carved, Carver, Input, Signature};
use crate::filetype::Category;

/// GIF images and animations.
pub struct Gif;

const SIGS: &[Signature] = &[Signature::at0(b"GIF87a"), Signature::at0(b"GIF89a")];

/// Upper bound on the number of blocks walked for one file.
const MAX_BLOCKS: u32 = 4_000_000;
/// Upper bound on the number of sub-blocks in one sequence.
const MAX_SUB_BLOCKS: u32 = 1 << 22;

/// Size in bytes of the color table announced by `packed` (0 if absent).
fn color_table_len(packed: u8) -> u64 {
    if packed & 0x80 != 0 {
        3 << ((packed & 7) + 1)
    } else {
        0
    }
}

/// Result of skipping a sub-block sequence.
enum Sub {
    /// The sequence ended; the next block starts at this offset.
    Done(u64),
    /// Ran into the limit.
    Limit,
    /// Unreadable data or absurd length.
    Bad,
}

/// Skips the sub-blocks starting at `pos`, including the zero terminator.
fn skip_sub_blocks(input: &Input, mut pos: u64, limit: u64) -> Sub {
    for _ in 0..MAX_SUB_BLOCKS {
        if pos >= limit {
            return Sub::Limit;
        }
        let Some(n) = input.u8(pos) else {
            return Sub::Bad;
        };
        pos += 1 + u64::from(n);
        if pos > limit {
            return Sub::Limit;
        }
        if n == 0 {
            return Sub::Done(pos);
        }
    }
    Sub::Bad
}

impl Carver for Gif {
    fn name(&self) -> &'static str {
        "gif"
    }

    fn signatures(&self) -> &'static [Signature] {
        SIGS
    }

    fn categories(&self) -> &'static [Category] {
        &[Category::Photo]
    }

    fn identify(&self, head: &[u8]) -> Option<&'static str> {
        if !(head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a")) {
            return None;
        }
        let width = u16::from_le_bytes([*head.get(6)?, *head.get(7)?]);
        let height = u16::from_le_bytes([*head.get(8)?, *head.get(9)?]);
        let packed = *head.get(10)?;
        if width == 0 || height == 0 {
            return None;
        }
        // The first block must be an extension or an image.
        let first = usize::try_from(13 + color_table_len(packed)).ok()?;
        matches!(head.get(first)?, 0x21 | 0x2C).then_some("gif")
    }

    fn carve(&self, input: &Input, start: u64, limit: u64) -> Option<Carved> {
        let packed = input.array::<7>(start + 6)?[4];
        let mut pos = start + 13 + color_table_len(packed);
        let mut images = 0u32;
        // Corrupt structure: only a GIF if at least one frame was seen.
        let bad =
            |pos: u64, images: u32| (images > 0).then(|| Carved::truncated(pos - start, "gif"));
        for _ in 0..MAX_BLOCKS {
            if pos >= limit {
                return Some(Carved::truncated(limit - start, "gif"));
            }
            match input.u8(pos)? {
                0x3B => return Some(Carved::structure(pos + 1 - start, "gif")),
                0x21 => {
                    if input.u8(pos + 1).is_none() {
                        return bad(pos, images);
                    }
                    match skip_sub_blocks(input, pos + 2, limit) {
                        Sub::Done(end) => pos = end,
                        Sub::Limit => return Some(Carved::truncated(limit - start, "gif")),
                        Sub::Bad => return bad(pos, images),
                    }
                }
                0x2C => {
                    let Some(desc) = input.array::<9>(pos + 1) else {
                        return bad(pos, images);
                    };
                    let after_table = pos + 10 + color_table_len(desc[8]);
                    match input.u8(after_table) {
                        Some(1..=8) => {}
                        _ => return bad(pos, images),
                    }
                    match skip_sub_blocks(input, after_table + 1, limit) {
                        Sub::Done(end) => {
                            images += 1;
                            pos = end;
                        }
                        Sub::Limit => return Some(Carved::truncated(limit - start, "gif")),
                        Sub::Bad => return bad(pos, images),
                    }
                }
                _ => return bad(pos, images),
            }
        }
        bad(pos, images)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carve::testutil::{carve_bytes, carve_with_limit};

    fn sub_blocks(data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        for c in data.chunks(255) {
            v.push(c.len() as u8);
            v.extend_from_slice(c);
        }
        v.push(0);
        v
    }

    fn header(global_table_bits: Option<u8>) -> Vec<u8> {
        let mut v = b"GIF89a".to_vec();
        v.extend([4, 0, 4, 0]);
        match global_table_bits {
            Some(n) => {
                v.extend([0x80 | n, 0, 0]);
                v.extend(vec![0x33; 3 << (n + 1)]);
            }
            None => v.extend([0, 0, 0]),
        }
        v
    }

    fn image(local_table_bits: Option<u8>, lzw: &[u8]) -> Vec<u8> {
        let mut v = vec![0x2C, 0, 0, 0, 0, 4, 0, 4, 0];
        match local_table_bits {
            Some(n) => {
                v.push(0x80 | n);
                v.extend(vec![0x77; 3 << (n + 1)]);
            }
            None => v.push(0),
        }
        v.push(2); // LZW minimum code size
        v.extend(sub_blocks(lzw));
        v
    }

    /// Animated GIF: global table, NETSCAPE + graphic control + comment
    /// extensions, two frames (one with a local table), trailer.
    fn sample() -> Vec<u8> {
        let mut v = header(Some(2));
        v.extend([0x21, 0xFF, 11]);
        v.extend(b"NETSCAPE2.0");
        v.extend(sub_blocks(&[1, 0, 0]));
        v.extend([0x21, 0xF9]);
        v.extend(sub_blocks(&[4, 10, 0, 0]));
        v.extend(image(None, &[0x3B; 700])); // 0x3B inside data must not end the file
        v.extend([0x21, 0xFE]);
        v.extend(sub_blocks(b"a comment"));
        v.extend(image(Some(1), &[0x2C; 20]));
        v.push(0x3B);
        v
    }

    #[test]
    fn finds_trailer_before_trailing_garbage() {
        let data = sample();
        let c = carve_bytes(&Gif, &data, 5000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert_eq!(c.ext, "gif");
        assert!(!c.truncated);
    }

    #[test]
    fn gif87a_without_global_table() {
        let mut data = header(None);
        data[..6].copy_from_slice(b"GIF87a");
        data.extend(image(None, &[1, 2, 3]));
        data.push(0x3B);
        let c = carve_bytes(&Gif, &data, 100).unwrap();
        assert_eq!(c.len, data.len() as u64);
    }

    #[test]
    fn truncated_at_limit() {
        let data = sample();
        let c = carve_with_limit(&Gif, &data, 400).unwrap();
        assert_eq!(c.len, 400);
        assert!(c.truncated);
    }

    #[test]
    fn garbage_after_a_frame_keeps_the_frames() {
        let mut data = header(None);
        data.extend(image(None, &[9; 10]));
        let good = data.len() as u64;
        data.extend([0x99, 1, 2, 3]);
        let c = carve_bytes(&Gif, &data, 50).unwrap();
        assert_eq!(c.len, good);
        assert!(c.truncated);
    }

    #[test]
    fn rejects_garbage() {
        // Garbage right after the header: no frame seen.
        let mut data = header(None);
        data.push(0x99);
        assert!(carve_bytes(&Gif, &data, 3000).is_none());
        // Header only.
        assert!(carve_bytes(&Gif, &header(Some(3)), 0).is_none());
        // Zero-sized screen.
        let mut data = header(None);
        data[6] = 0;
        data[7] = 0;
        data.extend(image(None, &[1]));
        assert!(carve_bytes(&Gif, &data, 10).is_none());
        // Bad LZW minimum code size in the only frame.
        let mut data = header(None);
        let mut frame = image(None, &[1, 2]);
        frame[10] = 200;
        data.extend(frame);
        assert!(carve_bytes(&Gif, &data, 10).is_none());
    }
}
