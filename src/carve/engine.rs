//! The two-pass carving engine.
//!
//! Pass 1 reads the scan ranges in large chunks and, at every 512-byte-aligned
//! offset, runs an anchored multi-pattern (Aho-Corasick) search for the header
//! signatures of all carvers, confirming hits with [`Carver::identify`].
//! Pass 2 visits the candidates in offset order, skips those inside a file
//! already carved, and asks the carver where each file ends.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use aho_corasick::{AhoCorasick, Anchored, Input as AcInput, MatchKind, StartKind};
use indicatif::ProgressBar;

use super::avistream::{self, OrphanStream};
use super::{Carved, Carver, EndMethod, HEAD_LEN, Input};
use crate::cli::TypeFilter;
use crate::filetype::{Category, category_of_ext};
use crate::source::Source;

/// Headers are only looked for at multiples of this (files on FAT start on a
/// cluster boundary, which is always sector aligned).
pub const ALIGN: u64 = 512;
const CHUNK: usize = 8 << 20;

/// A confirmed header found in pass 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// Absolute offset of the start of the file.
    pub offset: u64,
    /// Index into the carver list.
    pub carver: usize,
    /// Extension reported by `identify`.
    pub ext: &'static str,
}

/// A file decided in pass 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarvedFile {
    /// Absolute offset of the file in the source.
    pub offset: u64,
    /// Carver result (with the final length).
    pub carved: Carved,
    /// Name of the carver that produced it.
    pub format: &'static str,
}

/// Maximum file size per category.
#[derive(Debug, Clone, Copy)]
pub struct SizeCaps {
    /// Photos.
    pub photo: u64,
    /// Videos.
    pub video: u64,
    /// Audio.
    pub audio: u64,
}

impl SizeCaps {
    /// Cap for a file with extension `ext`.
    pub fn for_ext(&self, ext: &str) -> u64 {
        match category_of_ext(ext) {
            Some(Category::Photo) => self.photo,
            Some(Category::Video) => self.video,
            Some(Category::Audio) => self.audio,
            None => self.photo,
        }
    }
}

/// Header matcher built from the carvers' signatures: one anchored automaton
/// per distinct signature offset. Identical patterns declared by several
/// carvers (e.g. `ID3` for MP3 and FLAC) are merged and map to all owners.
struct Matcher {
    groups: Vec<(usize, AhoCorasick, Vec<Vec<usize>>)>,
    max_extent: usize,
}

impl Matcher {
    fn new(carvers: &[Box<dyn Carver>], enabled: &[bool]) -> Matcher {
        let mut offsets: Vec<usize> = Vec::new();
        for (i, c) in carvers.iter().enumerate() {
            if enabled[i] {
                offsets.extend(c.signatures().iter().map(|s| s.offset));
            }
        }
        offsets.sort_unstable();
        offsets.dedup();
        let mut groups = Vec::new();
        let mut max_extent = 0;
        for off in offsets {
            let mut pats: Vec<&[u8]> = Vec::new();
            let mut owners: Vec<Vec<usize>> = Vec::new();
            for (i, c) in carvers.iter().enumerate() {
                if !enabled[i] {
                    continue;
                }
                for s in c.signatures().iter().filter(|s| s.offset == off) {
                    max_extent = max_extent.max(off + s.bytes.len());
                    match pats.iter().position(|p| *p == s.bytes) {
                        Some(k) => owners[k].push(i),
                        None => {
                            pats.push(s.bytes);
                            owners.push(vec![i]);
                        }
                    }
                }
            }
            let ac = AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostFirst)
                .start_kind(StartKind::Anchored)
                .build(&pats)
                .expect("static signatures form a valid automaton");
            groups.push((off, ac, owners));
        }
        Matcher { groups, max_extent }
    }

    /// Carver indices whose signatures match a file starting at `buf[pos..]`,
    /// in carver order. Signatures at the same offset must not be prefixes of
    /// one another, so leftmost-first finds the only pattern that can match.
    fn matches_at(&self, buf: &[u8], pos: usize, out: &mut Vec<usize>) {
        out.clear();
        for (off, ac, owners) in &self.groups {
            let s = pos + off;
            if s >= buf.len() {
                continue;
            }
            let input = AcInput::new(buf)
                .range(s..buf.len().min(s + self.max_extent))
                .anchored(Anchored::Yes);
            if let Some(m) = ac.find(input) {
                out.extend_from_slice(&owners[m.pattern().as_usize()]);
            }
        }
        out.sort_unstable();
        out.dedup();
    }
}

/// Result of pass 1.
#[derive(Debug, Default)]
pub struct Pass1 {
    /// Confirmed headers, sorted by offset.
    pub candidates: Vec<Candidate>,
    /// Unaligned positions where headerless AVI `movi` chunks were seen
    /// (at most one per [`avistream::HINT_SPACING`]), sorted.
    pub stream_hints: Vec<u64>,
}

/// Pass 1: finds candidate headers inside the given absolute `[start, end)`
/// ranges, and (when `stream_hints` is set) hints of headerless AVI streams.
pub fn find_candidates(
    src: &dyn Source,
    ranges: &[(u64, u64)],
    carvers: &[Box<dyn Carver>],
    filter: &TypeFilter,
    stream_hints: bool,
    progress: &ProgressBar,
) -> Pass1 {
    let enabled: Vec<bool> = carvers
        .iter()
        .map(|c| c.categories().iter().any(|&cat| filter.accepts(Some(cat))))
        .collect();
    let matcher = Matcher::new(carvers, &enabled);
    let stream_ac = AhoCorasick::new(avistream::hint_patterns())
        .expect("static patterns form a valid automaton");
    let extra = HEAD_LEN.max(matcher.max_extent);
    let mut buf = vec![0u8; CHUNK + extra];
    let mut out = Pass1::default();
    let mut hits = Vec::new();
    let mut next_hint = 0u64;
    for &(rs, re) in ranges {
        let mut pos = rs.div_ceil(ALIGN) * ALIGN;
        while pos < re {
            let scan_len = ((re - pos) as usize).min(CHUNK);
            let n = src
                .read_at(pos, &mut buf[..scan_len + extra])
                .unwrap_or_else(|e| {
                    eprintln!("warning: read error at offset {pos}: {e}; skipping this chunk");
                    0
                });
            let scan_end = scan_len.min(n);
            let mut i = 0usize;
            while i < scan_end {
                matcher.matches_at(&buf[..n], i, &mut hits);
                for &ci in &hits {
                    let head = &buf[i..n.min(i + HEAD_LEN)];
                    if let Some(ext) = carvers[ci].identify(head)
                        && filter.accepts(category_of_ext(ext))
                    {
                        out.candidates.push(Candidate {
                            offset: pos + i as u64,
                            carver: ci,
                            ext,
                        });
                        break;
                    }
                }
                i += ALIGN as usize;
            }
            if stream_hints {
                for m in stream_ac.find_iter(&buf[..scan_end]) {
                    let abs = pos + m.start() as u64;
                    if abs >= next_hint && avistream::is_hint(&buf[..n], m.start()) {
                        out.stream_hints.push(abs);
                        next_hint = (abs / avistream::HINT_SPACING + 1) * avistream::HINT_SPACING;
                    }
                }
            }
            progress.inc(scan_len as u64);
            pos += scan_len as u64;
        }
    }
    out.candidates.sort_by_key(|c| c.offset);
    out.candidates.dedup_by_key(|c| c.offset);
    out
}

/// Result of pass 2.
#[derive(Debug, Default)]
pub struct Resolved {
    /// Files carved from a recognised header.
    pub files: Vec<CarvedFile>,
    /// Headerless AVI streams to rebuild.
    pub streams: Vec<OrphanStream>,
}

/// Walks the stream at hint `h` (within the scan range holding it) and cuts
/// it before the first header in `later` that is not the payload of one of
/// its own chunks (a JPEG frame of an MJPEG stream is not a separate file).
fn stream_at(
    input: &Input,
    h: u64,
    ranges: &[(u64, u64)],
    later: &[Candidate],
) -> Option<(OrphanStream, u64)> {
    let &(_, range_end) = ranges.iter().find(|(s, e)| h >= *s && h < *e)?;
    let mut s = avistream::walk(input, h, range_end)?;
    for c in later.iter().take_while(|c| c.offset < s.end) {
        let embedded = c.offset >= 8
            && s.chunks
                .binary_search_by_key(&(c.offset - 8), |k| k.offset)
                .is_ok();
        if !embedded {
            s.chunks
                .retain(|k| k.offset + 8 + u64::from(k.size) <= c.offset);
            s.end = s.chunks.last().map_or(h, |k| {
                k.offset + 8 + u64::from(k.size) + u64::from(k.size & 1)
            });
            break;
        }
    }
    (s.video_frames() >= avistream::MIN_VIDEO_FRAMES).then_some((s, range_end))
}

/// Pass 2: visits the candidates and the stream hints in offset order,
/// determines the extent of each file, and drops anything that starts inside
/// a file or stream already found.
pub fn resolve(
    input: &Input,
    candidates: &[Candidate],
    stream_hints: &[u64],
    ranges: &[(u64, u64)],
    carvers: &[Box<dyn Carver>],
    caps: &SizeCaps,
    progress: &ProgressBar,
) -> Resolved {
    let mut out = Resolved::default();
    let mut hints: BinaryHeap<Reverse<u64>> = stream_hints.iter().map(|&h| Reverse(h)).collect();
    let mut carved_until = 0u64;
    let mut i = 0usize;
    loop {
        let next_hint = hints.peek().map(|r| r.0);
        let take_hint = match (candidates.get(i), next_hint) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(c), Some(h)) => h < c.offset,
        };
        if take_hint {
            let Some(Reverse(h)) = hints.pop() else { break };
            if h < carved_until {
                continue;
            }
            if let Some((s, range_end)) = stream_at(input, h, ranges, &candidates[i..]) {
                carved_until = s.end;
                // Another stream may start in the same MiB, which pass 1 does not report.
                if let Some(n) = avistream::find_hint(input, s.end, range_end) {
                    hints.push(Reverse(n));
                }
                out.streams.push(s);
            }
            continue;
        }
        let Some(cand) = candidates.get(i) else { break };
        i += 1;
        progress.inc(1);
        if cand.offset < carved_until {
            continue;
        }
        let cap = caps.for_ext(cand.ext).max(1);
        let limit = cand.offset.saturating_add(cap).min(input.len());
        if limit <= cand.offset {
            continue;
        }
        let Some(mut c) = carvers[cand.carver].carve(input, cand.offset, limit) else {
            continue;
        };
        c.len = c.len.min(limit - cand.offset);
        if c.end == EndMethod::NextHeader {
            let next = candidates[i..]
                .iter()
                .find(|n| n.offset > cand.offset)
                .map(|n| n.offset);
            match next {
                Some(n) if n - cand.offset <= c.len => {
                    c.len = n - cand.offset;
                    c.truncated = false;
                }
                _ => c.truncated = true,
            }
        }
        if c.len == 0 {
            continue;
        }
        carved_until = cand.offset + c.len;
        out.files.push(CarvedFile {
            offset: cand.offset,
            carved: c,
            format: carvers[cand.carver].name(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carve::all_carvers;
    use crate::source::MemSource;

    fn tiny_jpeg() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x04, 0, 0];
        v.extend([0xFF, 0xDA, 0x00, 0x02, 1, 2, 3, 0xFF, 0x00, 4, 0xFF, 0xD9]);
        v
    }

    #[test]
    fn aligned_headers_only_and_overlap_skipped() {
        let mut img = vec![0u8; 64 * 1024];
        let j = tiny_jpeg();
        img[1024..1024 + j.len()].copy_from_slice(&j);
        img[3000..3000 + j.len()].copy_from_slice(&j); // not aligned: ignored
        // A thumbnail header at an aligned offset inside a big carved file
        // (embedded in an APP1 segment, as EXIF thumbnails are).
        let mut app1 = vec![0u8; 8000];
        app1[4090..4090 + j.len()].copy_from_slice(&j);
        let mut big = vec![0xFF, 0xD8, 0xFF, 0xE1];
        big.extend(((app1.len() + 2) as u16).to_be_bytes());
        big.extend(&app1);
        big.extend([0xFF, 0xDA, 0x00, 0x02, 0x11, 0x22, 0xFF, 0xD9]);
        img[16384..16384 + big.len()].copy_from_slice(&big);
        let src = MemSource(img);
        let carvers = all_carvers();
        let pb = ProgressBar::hidden();
        let filter = TypeFilter::default();
        let cands =
            find_candidates(&src, &[(0, src.size())], &carvers, &filter, false, &pb).candidates;
        let offs: Vec<u64> = cands.iter().map(|c| c.offset).collect();
        assert_eq!(offs, vec![1024, 16384, 16384 + 4096]);
        let caps = SizeCaps {
            photo: 1 << 20,
            video: 1 << 20,
            audio: 1 << 20,
        };
        let files = resolve(&Input::new(&src), &cands, &[], &[], &carvers, &caps, &pb).files;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].offset, 1024);
        assert_eq!(files[0].carved.len, j.len() as u64);
        assert_eq!(files[1].offset, 16384);
        assert_eq!(files[1].carved.len, big.len() as u64);
    }

    #[test]
    fn respects_scan_ranges_and_type_filter() {
        let mut img = vec![0u8; 16 * 1024];
        let j = tiny_jpeg();
        img[512..512 + j.len()].copy_from_slice(&j);
        img[8192..8192 + j.len()].copy_from_slice(&j);
        let src = MemSource(img);
        let carvers = all_carvers();
        let pb = ProgressBar::hidden();
        let c = find_candidates(
            &src,
            &[(4096, 16384)],
            &carvers,
            &TypeFilter::default(),
            false,
            &pb,
        )
        .candidates;
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].offset, 8192);
        let video_only = TypeFilter {
            types: vec![Category::Video],
        };
        let p = find_candidates(&src, &[(0, 16384)], &carvers, &video_only, false, &pb);
        assert!(p.candidates.is_empty());
    }

    #[test]
    fn finds_unaligned_stream_hints_once_per_mib() {
        let mut img: Vec<u8> = (0..1234u32).map(|i| (i % 200) as u8).collect();
        for i in 0..3000u32 {
            img.extend_from_slice(if i % 4 == 3 { b"01wb" } else { b"00dc" });
            img.extend_from_slice(&992u32.to_le_bytes());
            img.extend((0..992u32).map(|k| (k * 7 + i) as u8 | 1));
        }
        let src = MemSource(img);
        let carvers = all_carvers();
        let pb = ProgressBar::hidden();
        let all = TypeFilter::default();
        let p = find_candidates(&src, &[(0, src.size())], &carvers, &all, true, &pb);
        assert_eq!(p.stream_hints.first(), Some(&1234));
        assert!(
            p.stream_hints.len() >= 2 && p.stream_hints.len() <= 4,
            "{:?}",
            p.stream_hints
        );
        assert!(p.stream_hints.windows(2).all(|w| w[1] - w[0] >= 1 << 19));
        let off = find_candidates(&src, &[(0, src.size())], &carvers, &all, false, &pb);
        assert!(off.stream_hints.is_empty());
    }

    /// A frame that happens to start on a 512-byte boundary belongs to the
    /// stream, but a real file whose header lies inside the stream's last
    /// chunk (the chunk ran into it) cuts the stream and is carved itself.
    #[test]
    fn streams_own_their_frames_but_not_foreign_files() {
        let j = tiny_jpeg();
        let mut img: Vec<u8> = vec![0x33; 1000];
        let mut payload_starts = Vec::new();
        for _ in 0..40 {
            img.extend_from_slice(b"00dc");
            img.extend_from_slice(&(j.len() as u32).to_le_bytes());
            payload_starts.push(img.len());
            img.extend_from_slice(&j);
        }
        // Pad so the next frame's payload is 512-aligned.
        let pad = (512 - (img.len() + 8 + 8) % 512) % 512;
        img.extend_from_slice(b"JUNK");
        img.extend_from_slice(&(pad as u32).to_le_bytes());
        img.resize(img.len() + pad, 0);
        img.extend_from_slice(b"00dc");
        img.extend_from_slice(&(j.len() as u32).to_le_bytes());
        let aligned_frame = img.len();
        assert_eq!(aligned_frame % 512, 0);
        img.extend_from_slice(&j);
        // A last chunk that claims 4 KiB, with a foreign photo inside it.
        img.extend_from_slice(b"00dc");
        img.extend_from_slice(&4096u32.to_le_bytes());
        let foreign = img.len().div_ceil(512) * 512 + 512;
        img.resize(foreign, 0x44);
        img.extend_from_slice(&j);
        img.resize(foreign + 8192, 0);
        let src = MemSource(img);
        let carvers = all_carvers();
        let pb = ProgressBar::hidden();
        let ranges = [(0, src.size())];
        let all = TypeFilter::default();
        let p = find_candidates(&src, &ranges, &carvers, &all, true, &pb);
        let offs: Vec<u64> = p.candidates.iter().map(|c| c.offset).collect();
        assert_eq!(offs, vec![aligned_frame as u64, foreign as u64]);
        let caps = SizeCaps {
            photo: 1 << 20,
            video: 1 << 20,
            audio: 1 << 20,
        };
        let r = resolve(
            &Input::new(&src),
            &p.candidates,
            &p.stream_hints,
            &ranges,
            &carvers,
            &caps,
            &pb,
        );
        assert_eq!(r.files.len(), 1, "{:?}", r.files);
        assert_eq!(r.files[0].offset, foreign as u64);
        assert_eq!(r.streams.len(), 1);
        let s = &r.streams[0];
        assert_eq!(s.video_frames(), 41);
        assert!(s.end <= foreign as u64);
        assert_eq!(s.start, 1000);
    }

    #[test]
    fn signatures_at_same_offset_are_not_prefixes() {
        let carvers = all_carvers();
        let sigs: Vec<_> = carvers
            .iter()
            .flat_map(|c| c.signatures().iter().map(move |s| (c.name(), *s)))
            .collect();
        for (na, a) in &sigs {
            for (nb, b) in &sigs {
                if a.offset != b.offset || a.bytes == b.bytes {
                    continue;
                }
                assert!(
                    !a.bytes.starts_with(b.bytes),
                    "{nb} signature {:?} is a prefix of {na} signature {:?}",
                    b.bytes,
                    a.bytes
                );
            }
        }
    }

    /// Every carver must survive garbage behind its own signatures: random
    /// bytes, all-0xFF, all-zero, and every truncation of the data.
    #[test]
    fn carvers_never_panic_on_garbage() {
        let carvers = all_carvers();
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        };
        for c in &carvers {
            for sig in c.signatures() {
                for fill in 0..4u8 {
                    let mut data: Vec<u8> = (0..8192)
                        .map(|_| match fill {
                            0 => 0,
                            1 => 0xFF,
                            _ => rnd(),
                        })
                        .collect();
                    data[sig.offset..sig.offset + sig.bytes.len()].copy_from_slice(sig.bytes);
                    for cut in [0, 1, 4, 8, 12, 27, 64, 200, 513, 4096, 8192] {
                        let src = MemSource(data[..cut.min(data.len())].to_vec());
                        let head = &src.0[..src.0.len().min(HEAD_LEN)];
                        let _ = c.identify(head);
                        let inp = Input::new(&src);
                        for limit in [0, 3, 100, 5000, src.size()] {
                            let limit = limit.min(src.size());
                            if let Some(r) = c.carve(&inp, 0, limit) {
                                assert!(r.len <= limit, "{} exceeded its limit", c.name());
                            }
                        }
                    }
                }
            }
        }
    }
}
