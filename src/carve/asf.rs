//! ASF carver (Windows Media: WMV video, WMA audio).
//!
//! Structure: a sequence of top-level objects `<GUID:16> <size:u64 LE> <payload>`
//! where `size` includes the 24-byte object header. GUIDs are stored with the
//! first three fields little-endian. A file is one Header Object (which holds
//! child objects, among them one Stream Properties Object per stream with the
//! stream type GUID at payload offset 0), one Data Object with the media
//! packets, and optionally index objects (Simple Index, Index, Media Object
//! Index, Timecode Index). The file ends after the last such object. It is a
//! WMV if any stream is of type Video Media, else a WMA.

use super::{Carved, Carver, Input, Signature};
use crate::filetype::Category;

/// Windows Media (ASF) files.
pub struct Asf;

type Guid = [u8; 16];

/// Builds the on-disk byte form of a GUID written as `d1-d2-d3-d4`.
const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Guid {
    let a = d1.to_le_bytes();
    let b = d2.to_le_bytes();
    let c = d3.to_le_bytes();
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4], d4[5],
        d4[6], d4[7],
    ]
}

const HEADER: Guid = guid(
    0x75B2_2630,
    0x668E,
    0x11CF,
    [0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C],
);
const DATA: Guid = guid(
    0x75B2_2636,
    0x668E,
    0x11CF,
    [0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C],
);
const SIMPLE_INDEX: Guid = guid(
    0x3300_0890,
    0xE5B1,
    0x11CF,
    [0x89, 0xF4, 0x00, 0xA0, 0xC9, 0x03, 0x49, 0xCB],
);
const INDEX: Guid = guid(
    0xD6E2_29D3,
    0x35DA,
    0x11D1,
    [0x90, 0x34, 0x00, 0xA0, 0xC9, 0x03, 0x49, 0xBE],
);
const MEDIA_OBJECT_INDEX: Guid = guid(
    0xFEB1_03F8,
    0x12AD,
    0x4C64,
    [0x84, 0x0F, 0x2A, 0x1D, 0x2F, 0x7A, 0xD4, 0x8C],
);
const TIMECODE_INDEX: Guid = guid(
    0x3CB7_3FD0,
    0x0C4A,
    0x4803,
    [0x95, 0x3D, 0xED, 0xF7, 0xB6, 0x22, 0x8F, 0x0C],
);
const STREAM_PROPERTIES: Guid = guid(
    0xB7DC_0791,
    0xA9B7,
    0x11CF,
    [0x8E, 0xE6, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65],
);
const VIDEO_MEDIA: Guid = guid(
    0xBC19_EFC0,
    0x5B4D,
    0x11CF,
    [0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B],
);

const SIGS: &[Signature] = &[Signature::at0(&HEADER)];

/// Object header: GUID + size.
const OBJ_HDR: u64 = 24;
/// Header Object: object header + child count (u32) + two reserved bytes.
const HEADER_FIXED: u64 = 30;
/// Largest Header Object we believe (they hold metadata and sometimes cover art).
const MAX_HEADER: u64 = 64 << 20;
/// Safety cap on the objects walked in the Header Object / after it.
const MAX_OBJECTS: u32 = 100_000;

fn is_index(g: &Guid) -> bool {
    *g == SIMPLE_INDEX || *g == INDEX || *g == MEDIA_OBJECT_INDEX || *g == TIMECODE_INDEX
}

/// Looks through the child objects of a Header Object of `size` bytes for
/// Stream Properties Objects. `read(off, buf)` reads at an offset relative to
/// the start of the Header Object. Returns `Some(true)` for a video stream,
/// `Some(false)` if streams were found but none is video, `None` if no stream
/// could be inspected.
fn has_video_stream(size: u64, read: impl Fn(u64, &mut [u8]) -> bool) -> Option<bool> {
    let mut pos = HEADER_FIXED;
    let mut found = false;
    for _ in 0..MAX_OBJECTS {
        let mut hdr = [0u8; 24];
        if !read(pos, &mut hdr) {
            break;
        }
        let obj_size = u64::from_le_bytes([
            hdr[16], hdr[17], hdr[18], hdr[19], hdr[20], hdr[21], hdr[22], hdr[23],
        ]);
        let Some(end) = pos.checked_add(obj_size) else {
            break;
        };
        if obj_size < OBJ_HDR || end > size {
            break;
        }
        if hdr[..16] == STREAM_PROPERTIES {
            let mut stream_type = [0u8; 16];
            if obj_size >= OBJ_HDR + 16 && read(pos + OBJ_HDR, &mut stream_type) {
                if stream_type == VIDEO_MEDIA {
                    return Some(true);
                }
                found = true;
            }
        }
        pos = end;
    }
    found.then_some(false)
}

fn ext_for(video: Option<bool>) -> &'static str {
    if video == Some(true) { "wmv" } else { "wma" }
}

impl Carver for Asf {
    fn name(&self) -> &'static str {
        "asf"
    }

    fn signatures(&self) -> &'static [Signature] {
        SIGS
    }

    fn categories(&self) -> &'static [Category] {
        &[Category::Video, Category::Audio]
    }

    fn identify(&self, head: &[u8]) -> Option<&'static str> {
        if head.len() < HEADER_FIXED as usize || head[..16] != HEADER {
            return None;
        }
        let size = u64::from_le_bytes(head[16..24].try_into().ok()?);
        if !(HEADER_FIXED..=MAX_HEADER).contains(&size) {
            return None;
        }
        // The Data Object follows the Header Object; check it when it is in view.
        if let Some(next) = usize::try_from(size)
            .ok()
            .and_then(|s| head.get(s..s.checked_add(16)?))
            && next != DATA
        {
            return None;
        }
        let video = has_video_stream(size, |off, buf| {
            usize::try_from(off)
                .ok()
                .and_then(|o| head.get(o..o.checked_add(buf.len())?))
                .map(|src| buf.copy_from_slice(src))
                .is_some()
        });
        Some(ext_for(video))
    }

    fn carve(&self, input: &Input, start: u64, limit: u64) -> Option<Carved> {
        let mut pos = start;
        let mut ext = "wma";
        let mut seen_header = false;
        let mut seen_data = false;
        for _ in 0..MAX_OBJECTS {
            let Some(hdr) = input.array::<24>(pos) else {
                break;
            };
            let g: Guid = [
                hdr[0], hdr[1], hdr[2], hdr[3], hdr[4], hdr[5], hdr[6], hdr[7], hdr[8], hdr[9],
                hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15],
            ];
            let size = u64::from_le_bytes([
                hdr[16], hdr[17], hdr[18], hdr[19], hdr[20], hdr[21], hdr[22], hdr[23],
            ]);
            let is_header = g == HEADER && !seen_header;
            let is_data = g == DATA && seen_header && !seen_data;
            let is_idx = is_index(&g) && seen_data;
            if !(is_header || is_data || is_idx) || size < OBJ_HDR {
                break;
            }
            if is_header {
                if !(HEADER_FIXED..=MAX_HEADER).contains(&size) {
                    return None;
                }
                seen_header = true;
                ext = ext_for(has_video_stream(size, |off, buf| {
                    input.read_exact(pos + off, buf)
                }));
            }
            let Some(end) = pos.checked_add(size) else {
                break;
            };
            if is_data {
                seen_data = true;
            }
            if end > limit {
                return seen_data.then(|| Carved::truncated(limit.saturating_sub(start), ext));
            }
            pos = end;
        }
        seen_data.then(|| Carved::structure(pos - start, ext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carve::testutil::{carve_bytes, carve_with_limit};

    fn object(g: &Guid, payload: &[u8]) -> Vec<u8> {
        let mut v = g.to_vec();
        v.extend((payload.len() as u64 + OBJ_HDR).to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    fn stream(kind: &Guid) -> Vec<u8> {
        let mut p = kind.to_vec();
        p.extend([0u8; 40]);
        object(&STREAM_PROPERTIES, &p)
    }

    fn header(streams: &[&Guid]) -> Vec<u8> {
        let mut children = object(&guid(1, 2, 3, [4; 8]), &[0; 80]); // e.g. File Properties
        for s in streams {
            children.extend(stream(s));
        }
        let mut p = (streams.len() as u32 + 1).to_le_bytes().to_vec();
        p.extend([1, 2]);
        p.extend(children);
        object(&HEADER, &p)
    }

    const AUDIO_MEDIA: Guid = guid(
        0xF8699E40,
        0x5B4D,
        0x11CF,
        [0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B],
    );

    fn file(streams: &[&Guid]) -> Vec<u8> {
        let mut v = header(streams);
        v.extend(object(&DATA, &[0x82; 3000]));
        v
    }

    #[test]
    fn video_stream_gives_wmv_and_audio_only_wma() {
        let wmv = file(&[&AUDIO_MEDIA, &VIDEO_MEDIA]);
        let c = carve_bytes(&Asf, &wmv, 2000).unwrap();
        assert_eq!(c.ext, "wmv");
        assert_eq!(c.len, wmv.len() as u64);
        assert_eq!(Asf.identify(&wmv[..wmv.len().min(4096)]), Some("wmv"));

        let wma = file(&[&AUDIO_MEDIA]);
        let c = carve_bytes(&Asf, &wma, 2000).unwrap();
        assert_eq!(c.ext, "wma");
        assert_eq!(c.len, wma.len() as u64);
        assert_eq!(Asf.identify(&wma[..wma.len().min(4096)]), Some("wma"));
    }

    #[test]
    fn index_objects_are_included_and_other_objects_end_the_file() {
        let mut data = file(&[&VIDEO_MEDIA]);
        data.extend(object(&SIMPLE_INDEX, &[0; 56]));
        data.extend(object(&INDEX, &[0; 120]));
        data.extend(object(&MEDIA_OBJECT_INDEX, &[0; 10]));
        data.extend(object(&TIMECODE_INDEX, &[0; 10]));
        let end = data.len() as u64;
        // A second, concatenated ASF file is not part of this one.
        data.extend(file(&[&AUDIO_MEDIA]));
        let c = carve_bytes(&Asf, &data, 3000).unwrap();
        assert_eq!(c.len, end);
        assert!(!c.truncated);
    }

    #[test]
    fn ends_before_trailing_garbage() {
        let data = file(&[&VIDEO_MEDIA]);
        let c = carve_bytes(&Asf, &data, 5000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert!(!c.truncated);
    }

    #[test]
    fn truncated_at_limit() {
        let data = file(&[&VIDEO_MEDIA]);
        let c = carve_with_limit(&Asf, &data, 1500).unwrap();
        assert_eq!(c.len, 1500);
        assert!(c.truncated);
        assert_eq!(c.ext, "wmv");
    }

    #[test]
    fn rejects_garbage_and_incomplete_files() {
        // Header Object without a Data Object.
        let only_header = header(&[&VIDEO_MEDIA]);
        assert!(carve_bytes(&Asf, &only_header, 3000).is_none());
        // Data Object missing: something else follows the header.
        let mut wrong_next = header(&[&VIDEO_MEDIA]);
        wrong_next.extend(object(&INDEX, &[0; 100]));
        assert!(carve_bytes(&Asf, &wrong_next, 100).is_none());
        // Impossible header sizes.
        let mut bad = file(&[&VIDEO_MEDIA]);
        bad[16..24].copy_from_slice(&5u64.to_le_bytes());
        assert!(carve_bytes(&Asf, &bad, 100).is_none());
        // Header GUID only.
        assert!(carve_bytes(&Asf, &HEADER, 3000).is_none());
        assert!(carve_bytes(&Asf, &[0u8; 100], 0).is_none());
    }
}
