//! The two-pass carving engine.
//!
//! Pass 1 reads the scan ranges in large chunks and, at every 512-byte-aligned
//! offset, runs an anchored multi-pattern (Aho-Corasick) search for the header
//! signatures of all carvers, confirming hits with [`Carver::identify`].
//! Pass 2 visits the candidates in offset order, skips those inside a file
//! already carved, and asks the carver where each file ends.

use aho_corasick::{AhoCorasick, Anchored, Input as AcInput, MatchKind, StartKind};
use indicatif::ProgressBar;

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

/// Pass 1: finds candidate headers inside the given absolute `[start, end)` ranges.
pub fn find_candidates(
    src: &dyn Source,
    ranges: &[(u64, u64)],
    carvers: &[Box<dyn Carver>],
    filter: &TypeFilter,
    progress: &ProgressBar,
) -> Vec<Candidate> {
    let enabled: Vec<bool> = carvers
        .iter()
        .map(|c| c.categories().iter().any(|&cat| filter.accepts(Some(cat))))
        .collect();
    let matcher = Matcher::new(carvers, &enabled);
    let extra = HEAD_LEN.max(matcher.max_extent);
    let mut buf = vec![0u8; CHUNK + extra];
    let mut found = Vec::new();
    let mut hits = Vec::new();
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
                        found.push(Candidate {
                            offset: pos + i as u64,
                            carver: ci,
                            ext,
                        });
                        break;
                    }
                }
                i += ALIGN as usize;
            }
            progress.inc(scan_len as u64);
            pos += scan_len as u64;
        }
    }
    found.sort_by_key(|c| c.offset);
    found.dedup_by_key(|c| c.offset);
    found
}

/// Pass 2: determines the extent of every candidate and drops those that
/// start inside an already-carved file.
pub fn resolve(
    input: &Input,
    candidates: &[Candidate],
    carvers: &[Box<dyn Carver>],
    caps: &SizeCaps,
    progress: &ProgressBar,
) -> Vec<CarvedFile> {
    let mut out = Vec::new();
    let mut carved_until = 0u64;
    for (i, cand) in candidates.iter().enumerate() {
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
            let next = candidates[i + 1..]
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
        out.push(CarvedFile {
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
        let cands = find_candidates(&src, &[(0, src.size())], &carvers, &filter, &pb);
        let offs: Vec<u64> = cands.iter().map(|c| c.offset).collect();
        assert_eq!(offs, vec![1024, 16384, 16384 + 4096]);
        let caps = SizeCaps {
            photo: 1 << 20,
            video: 1 << 20,
            audio: 1 << 20,
        };
        let files = resolve(&Input::new(&src), &cands, &carvers, &caps, &pb);
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
            &pb,
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].offset, 8192);
        let video_only = TypeFilter {
            types: vec![Category::Video],
        };
        assert!(find_candidates(&src, &[(0, 16384)], &carvers, &video_only, &pb).is_empty());
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
}
