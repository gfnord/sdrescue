//! MP3 carver.
//!
//! Structure: an optional ID3v2 tag (`ID3`, version major 2–4, revision, flags,
//! 4-byte syncsafe size; total length is 10 + size, plus 10 for a v2.4 footer),
//! then a chain of MPEG audio frames, then optionally an APEv2 tag and/or a
//! 128-byte ID3v1 `TAG` block.
//!
//! Every frame starts with a 4-byte header: 11 bits of sync, version (MPEG 1,
//! 2 or 2.5), layer, bitrate index, sample-rate index and a padding bit. Those
//! determine the frame length (`144 * bitrate / sample_rate + padding` for
//! Layer II and MPEG 1 Layer III, `72 * …` for MPEG 2/2.5 Layer III), so the
//! next frame can be located exactly. The walk continues while the headers stay
//! valid and keep the same version, layer and sample rate (the bitrate may
//! change: VBR); the file ends after the last such frame plus any trailing
//! tags.
//!
//! A frame sync is only 11 bits, so a file that does not start with an ID3v2
//! tag is accepted only when at least 8 consecutive frames follow; after an ID3v2
//! tag 2 frames are enough. An ID3v2 tag followed by `fLaC` is a FLAC file and
//! is left to the FLAC carver.

use super::{Carved, Carver, Input, Signature};
use crate::filetype::Category;

/// MP3 audio (MPEG 1/2/2.5 Layer III, with optional ID3/APE tags).
pub struct Mp3;

const SIGS: &[Signature] = &[
    Signature::at0(b"ID3"),
    Signature::at0(&[0xFF, 0xFB]),
    Signature::at0(&[0xFF, 0xFA]),
    Signature::at0(&[0xFF, 0xF3]),
    Signature::at0(&[0xFF, 0xF2]),
    Signature::at0(&[0xFF, 0xE3]),
    Signature::at0(&[0xFF, 0xE2]),
];

/// Frames required after an ID3v2 tag.
const MIN_FRAMES_TAGGED: u32 = 2;
/// Frames required when the file starts directly with a frame.
const MIN_FRAMES_BARE: u32 = 8;
/// Frames inspected by [`Mp3::identify`] when they fit into the head (a file
/// with a tag needs fewer frames, so fewer are checked there).
const IDENTIFY_FRAMES_BARE: usize = 3;
const IDENTIFY_FRAMES_TAGGED: usize = 2;
/// Size of an ID3v1 tag.
const ID3V1_LEN: u64 = 128;

/// Bitrates in kbit/s indexed by the header's bitrate index.
const KBPS_V1_L3: [u32; 15] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
];
const KBPS_V1_L2: [u32; 15] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
];
const KBPS_V2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

/// A parsed MPEG audio frame header.
#[derive(Clone, Copy)]
struct Frame {
    /// Version bits: 3 = MPEG 1, 2 = MPEG 2, 0 = MPEG 2.5.
    version: u8,
    /// 2 or 3.
    layer: u8,
    sample_rate: u32,
    /// Frame length in bytes, header included.
    len: u64,
}

impl Frame {
    /// Same version, layer and sample rate (the bitrate may differ).
    fn same_stream(&self, other: &Frame) -> bool {
        self.version == other.version
            && self.layer == other.layer
            && self.sample_rate == other.sample_rate
    }
}

/// Parses a frame header; `None` if it is not a valid Layer II/III header.
fn parse_frame(h: [u8; 4]) -> Option<Frame> {
    if h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 3;
    let layer = match (h[1] >> 1) & 3 {
        1 => 3,
        2 => 2,
        _ => return None,
    };
    let rates: [u32; 3] = match version {
        3 => [44100, 48000, 32000],
        2 => [22050, 24000, 16000],
        0 => [11025, 12000, 8000],
        _ => return None,
    };
    let sample_rate = *rates.get(usize::from((h[2] >> 2) & 3))?;
    let table = match (version, layer) {
        (3, 3) => &KBPS_V1_L3,
        (3, _) => &KBPS_V1_L2,
        _ => &KBPS_V2,
    };
    let kbps = *table.get(usize::from(h[2] >> 4))?;
    if kbps == 0 {
        return None; // index 0 = free format, 15 = invalid (not in the table)
    }
    let coef: u64 = if layer == 3 && version != 3 { 72 } else { 144 };
    let padding = u64::from((h[2] >> 1) & 1);
    let len = coef * u64::from(kbps) * 1000 / u64::from(sample_rate) + padding;
    Some(Frame {
        version,
        layer,
        sample_rate,
        len,
    })
}

/// Total length (header, body, footer) of the ID3v2 tag at the start of `h`.
///
/// `None` if `h` does not start with a plausible ID3v2 header.
pub(super) fn id3v2_len(h: &[u8]) -> Option<usize> {
    let h: &[u8; 10] = h.get(..10)?.try_into().ok()?;
    if &h[..3] != b"ID3" || !(2..=4).contains(&h[3]) || h[4] == 0xFF {
        return None;
    }
    let flags = h[5];
    // Low nibble is unused; the footer flag exists only in v2.4.
    if flags & 0x0F != 0 || (flags & 0x10 != 0 && h[3] != 4) {
        return None;
    }
    if h[6..10].iter().any(|&b| b >= 0x80) {
        return None;
    }
    let size = h[6..10]
        .iter()
        .fold(0usize, |acc, &b| (acc << 7) | usize::from(b));
    let footer = if flags & 0x10 != 0 { 10 } else { 0 };
    Some(10 + size + footer)
}

/// Why a frame walk stopped.
enum Stop {
    /// Not a valid (or consistent) frame header: the natural end.
    Invalid,
    /// Fewer than 4 bytes left before the limit.
    Capped,
    /// A valid header whose frame does not fit before the limit.
    Overrun,
}

/// Result of walking a chain of frames.
struct Walk {
    first: Frame,
    count: u32,
    /// Offset after the last complete frame.
    end: u64,
    stop: Stop,
}

/// Walks consecutive frames from `first` (absolute offset) up to `limit`.
/// `None` if there is no valid frame at `first`.
fn walk_frames(input: &Input, first: u64, limit: u64) -> Option<Walk> {
    let mut pos = first;
    let mut count = 0u32;
    let mut model: Option<Frame> = None;
    let stop = loop {
        if pos.saturating_add(4) > limit {
            break Stop::Capped;
        }
        let Some(f) = input.array::<4>(pos).and_then(parse_frame) else {
            break Stop::Invalid;
        };
        if model.as_ref().is_some_and(|m| !m.same_stream(&f)) {
            break Stop::Invalid;
        }
        model.get_or_insert(f);
        let next = pos.saturating_add(f.len);
        if next > limit {
            break Stop::Overrun;
        }
        pos = next;
        count += 1;
    };
    Some(Walk {
        first: model?,
        count,
        end: pos,
        stop,
    })
}

/// Length of an APEv2 tag (with header) starting at `pos`, if there is one.
fn ape_len(input: &Input, pos: u64, limit: u64) -> Option<u64> {
    let h = input.array::<32>(pos)?;
    if &h[..8] != b"APETAGEX" {
        return None;
    }
    let size = u64::from(u32::from_le_bytes([h[12], h[13], h[14], h[15]]));
    let flags = u32::from_le_bytes([h[20], h[21], h[22], h[23]]);
    // Must be a header (bit 31) and cover at least the footer.
    if flags & 0x8000_0000 == 0 || !(32..=(1 << 24)).contains(&size) {
        return None;
    }
    let total = 32 + size;
    (pos.checked_add(total)? <= limit).then_some(total)
}

/// End of the file after trailing APEv2 / ID3v1 tags that follow `end`.
fn skip_trailers(input: &Input, mut end: u64, limit: u64) -> u64 {
    if let Some(n) = ape_len(input, end, limit) {
        end += n;
    }
    if end + ID3V1_LEN <= limit && input.array::<3>(end) == Some(*b"TAG") {
        end += ID3V1_LEN;
    }
    end
}

impl Carver for Mp3 {
    fn name(&self) -> &'static str {
        "mp3"
    }

    fn signatures(&self) -> &'static [Signature] {
        SIGS
    }

    fn categories(&self) -> &'static [Category] {
        &[Category::Audio]
    }

    fn identify(&self, head: &[u8]) -> Option<&'static str> {
        let first = if head.starts_with(b"ID3") {
            let at = id3v2_len(head)?;
            if head.get(at..at.checked_add(4)?) == Some(b"fLaC") {
                return None;
            }
            at
        } else {
            0
        };
        // Check the frames that fit into the head (all of them must be valid).
        let mut pos = first;
        let mut model: Option<Frame> = None;
        let checked = if first > 0 {
            IDENTIFY_FRAMES_TAGGED
        } else {
            IDENTIFY_FRAMES_BARE
        };
        for _ in 0..checked {
            let Some(h) = head.get(pos..pos.saturating_add(4)) else {
                break;
            };
            let f = parse_frame([h[0], h[1], h[2], h[3]])?;
            if model.as_ref().is_some_and(|m| !m.same_stream(&f)) {
                return None;
            }
            model.get_or_insert(f);
            pos = pos.saturating_add(usize::try_from(f.len).ok()?);
        }
        match model {
            // Without a tag the first frame must be Layer III (the signatures promise it).
            Some(m) if first == 0 && m.layer != 3 => None,
            Some(_) => Some("mp3"),
            // Tag longer than the head: frames cannot be checked yet.
            None => (first > 0).then_some("mp3"),
        }
    }

    fn carve(&self, input: &Input, start: u64, limit: u64) -> Option<Carved> {
        let (first, min_frames) = match input.array::<10>(start) {
            Some(h) if h.starts_with(b"ID3") => {
                let tag = u64::try_from(id3v2_len(&h)?).ok()?;
                (start.checked_add(tag)?, MIN_FRAMES_TAGGED)
            }
            _ => (start, MIN_FRAMES_BARE),
        };
        if first >= limit || input.array::<4>(first) == Some(*b"fLaC") {
            return None;
        }
        let walk = walk_frames(input, first, limit)?;
        if walk.count < min_frames || (first == start && walk.first.layer != 3) {
            return None;
        }
        let cut_short = match walk.stop {
            Stop::Invalid => false,
            Stop::Overrun => true,
            // Fewer than 4 bytes left: a natural end only at the end of the image.
            Stop::Capped => limit < input.len(),
        };
        if cut_short {
            return Some(Carved::truncated(limit - start, "mp3"));
        }
        let end = skip_trailers(input, walk.end, limit);
        Some(Carved::structure(end - start, "mp3"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carve::EndMethod;
    use crate::carve::testutil::{carve_bytes, carve_with_limit};

    /// MPEG 1 Layer III, 128 kbit/s, 44.1 kHz: 417 bytes (+1 with padding).
    fn frame_v1(pad: bool) -> Vec<u8> {
        let mut v = vec![0xFF, 0xFB, 0x90 | (u8::from(pad) << 1), 0x00];
        v.resize(417 + usize::from(pad), 0x55);
        v
    }

    /// MPEG 2 Layer III, 64 kbit/s, 22.05 kHz: 208 bytes.
    fn frame_v2() -> Vec<u8> {
        let mut v = vec![0xFF, 0xF3, 0x80, 0x00];
        v.resize(208, 0x66);
        v
    }

    fn frames(n: usize) -> Vec<u8> {
        (0..n).flat_map(|i| frame_v1(i % 3 == 0)).collect()
    }

    fn id3v2(body_len: usize) -> Vec<u8> {
        let mut v = b"ID3\x03\x00\x00".to_vec();
        v.extend([
            ((body_len >> 21) & 0x7F) as u8,
            ((body_len >> 14) & 0x7F) as u8,
            ((body_len >> 7) & 0x7F) as u8,
            (body_len & 0x7F) as u8,
        ]);
        v.resize(10 + body_len, 0);
        v
    }

    fn id3v1() -> Vec<u8> {
        let mut v = b"TAG".to_vec();
        v.resize(128, b' ');
        v
    }

    #[test]
    fn id3v2_frames_and_id3v1_exact_end() {
        let mut data = id3v2(300);
        data.extend(frames(20));
        data.extend(id3v1());
        let c = carve_bytes(&Mp3, &data, 5000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert_eq!(c.ext, "mp3");
        assert_eq!(c.end, EndMethod::Structure);
        assert!(!c.truncated);
    }

    #[test]
    fn end_without_id3v1_and_vbr_bitrate_changes() {
        let mut data = frames(10);
        let mut other = frame_v1(false);
        other[2] = 0xA0; // 160 kbit/s: 144*160000/44100 = 522
        other.resize(522, 0x11);
        data.extend(other);
        data.extend(frames(3));
        let c = carve_bytes(&Mp3, &data, 2000).unwrap();
        assert_eq!(c.len, data.len() as u64);
    }

    #[test]
    fn id3v2_footer_and_ape_tag_are_included_in_tags() {
        // v2.4 tag with footer: 10 header + 20 body + 10 footer.
        let mut tag = b"ID3\x04\x00\x10\x00\x00\x00\x14".to_vec();
        tag.resize(30, 0);
        tag.extend(b"3DI\x04\x00\x10\x00\x00\x00\x14");
        let mut data = tag;
        data.extend(frames(5));
        // APEv2 tag: header (32) + one 8-byte item region + footer (32).
        let mut ape = b"APETAGEX".to_vec();
        ape.extend(2000u32.to_le_bytes());
        ape.extend(40u32.to_le_bytes()); // size: items + footer
        ape.extend(0u32.to_le_bytes());
        ape.extend(0x8000_0000u32.to_le_bytes());
        ape.resize(32, 0);
        ape.resize(32 + 40, 0x20);
        data.extend(ape);
        let c = carve_bytes(&Mp3, &data, 1000).unwrap();
        assert_eq!(c.len, data.len() as u64);
    }

    #[test]
    fn bare_frames_need_eight() {
        assert!(carve_bytes(&Mp3, &frames(8), 3000).is_some());
        assert!(carve_bytes(&Mp3, &frames(7), 3000).is_none());
        let c = carve_bytes(&Mp3, &frames(12), 3000).unwrap();
        assert_eq!(c.len, frames(12).len() as u64);
    }

    #[test]
    fn id3v2_needs_two_frames() {
        let mut one = id3v2(50);
        one.extend(frames(1));
        assert!(carve_bytes(&Mp3, &one, 3000).is_none());
        let mut two = id3v2(50);
        two.extend(frames(2));
        assert!(carve_bytes(&Mp3, &two, 3000).is_some());
    }

    #[test]
    fn mpeg2_layer3_frames() {
        let data: Vec<u8> = (0..10).flat_map(|_| frame_v2()).collect();
        let c = carve_bytes(&Mp3, &data, 4000).unwrap();
        assert_eq!(c.len, data.len() as u64);
    }

    #[test]
    fn walk_stops_at_sample_rate_change() {
        let mut data = frames(10);
        let good = data.len() as u64;
        data.extend(frame_v2()); // different version / sample rate
        data.extend(frame_v2());
        let c = carve_bytes(&Mp3, &data, 0).unwrap();
        assert_eq!(c.len, good);
    }

    #[test]
    fn id3_followed_by_flac_is_not_mp3() {
        let mut data = id3v2(100);
        data.extend(b"fLaC");
        data.extend([0u8; 400]);
        assert!(carve_bytes(&Mp3, &data, 0).is_none());
    }

    #[test]
    fn truncated_at_limit() {
        let mut data = id3v2(100);
        data.extend(frames(50));
        let limit = 5000;
        let c = carve_with_limit(&Mp3, &data, limit).unwrap();
        assert_eq!(c.len, limit);
        assert!(c.truncated);
    }

    #[test]
    fn data_running_out_mid_frame_is_truncated() {
        let mut data = frames(10);
        data.truncate(data.len() - 100);
        let c = carve_bytes(&Mp3, &data, 0).unwrap();
        assert!(c.truncated);
        assert_eq!(c.len, data.len() as u64);
    }

    #[test]
    fn rejects_garbage() {
        // Random-looking data and a lone sync.
        assert!(carve_bytes(&Mp3, &[0xFF, 0xFB, 0x90, 0x00], 4000).is_none());
        let junk: Vec<u8> = (0..4000).map(|i| (i * 31 + 7) as u8).collect();
        assert!(carve_bytes(&Mp3, &junk, 0).is_none());
        // Reserved version, bitrate index 15, sample rate index 3.
        for hdr in [
            [0xFF, 0xEB, 0x90, 0x00],
            [0xFF, 0xFB, 0xF0, 0x00],
            [0xFF, 0xFB, 0x9C, 0x00],
        ] {
            let mut data = Vec::new();
            for _ in 0..10 {
                data.extend(hdr);
                data.resize(data.len() + 413, 0);
            }
            assert!(carve_bytes(&Mp3, &data, 0).is_none());
        }
        // Corrupted ID3 headers: bad version, non-syncsafe size.
        let mut bad_version = id3v2(50);
        bad_version[3] = 9;
        bad_version.extend(frames(5));
        assert!(carve_bytes(&Mp3, &bad_version, 0).is_none());
        let mut bad_size = id3v2(50);
        bad_size[7] = 0x80;
        bad_size.extend(frames(5));
        assert!(carve_bytes(&Mp3, &bad_size, 0).is_none());
        // Tag followed by something that is not a frame.
        let mut no_frames = id3v2(50);
        no_frames.extend([0x13; 2000]);
        assert!(carve_bytes(&Mp3, &no_frames, 0).is_none());
    }
}
