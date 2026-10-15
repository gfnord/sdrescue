//! FAT12/16/32 volume access and the directory walk that finds deleted files.

pub mod bpb;
pub mod dir;
pub mod table;

use std::collections::HashSet;
use std::io;

use anyhow::{Context, Result};

use crate::source::Source;
use bpb::{Bpb, FatType};
use dir::{
    ATTR_DIR, DELETED, ENTRY_SIZE, FatDateTime, LfnPart, RawEntry, ShortEntry, decode_lfn,
    format_short_name, lfn_checksum, parse_entry, recover_first_char,
};
use table::FatTable;

/// FAT directories can hold at most 65536 entries (2 MiB).
const MAX_DIR_BYTES: u64 = 65536 * ENTRY_SIZE as u64;
/// Deepest directory nesting we follow.
const MAX_DEPTH: usize = 64;

/// An opened FAT volume: geometry plus an in-memory FAT.
pub struct FatVolume<'a> {
    src: &'a dyn Source,
    /// Byte offset of the volume (boot sector) inside the source.
    pub offset: u64,
    /// Parsed boot sector.
    pub bpb: Bpb,
    /// The FAT used for allocation decisions.
    pub fat: FatTable,
    /// Which FAT copy was loaded (0 = FAT #1).
    pub fat_copy: u32,
    /// Problems noticed while opening (truncated image, damaged FAT, ...).
    pub warnings: Vec<String>,
}

/// One file or directory found by the directory walk.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileRecord {
    /// Full reconstructed path, starting with `/`.
    pub path: String,
    /// Display name (long name if recovered, otherwise 8.3 name).
    pub name: String,
    /// The 8.3 name (first character recovered or replaced by `_` if deleted).
    pub short_name: String,
    /// Directory?
    pub is_dir: bool,
    /// The entry itself or one of its parent directories is deleted.
    pub deleted: bool,
    /// File size in bytes.
    pub size: u32,
    /// First cluster.
    pub start_cluster: u32,
    /// Last-modified time (local time as stored on the card).
    #[serde(skip)]
    pub modified: Option<FatDateTime>,
    /// The long file name was reconstructed.
    pub long_name: bool,
    /// The lost first character of the short name could not be recovered and was replaced by `_`.
    pub first_char_guessed: bool,
}

/// Output of [`FatVolume::walk`].
#[derive(Debug, Default)]
pub struct WalkResult {
    /// Every file and directory found.
    pub records: Vec<FileRecord>,
    /// Volume label from the root directory, if any.
    pub volume_label: Option<String>,
    /// Non-fatal problems encountered.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum DirLocation {
    /// FAT12/16 fixed root directory region.
    FixedRoot,
    /// Follow the FAT chain from this cluster.
    Chain(u32),
    /// Deleted directory: read consecutive clusters while they look like a directory.
    Contiguous(u32),
}

impl<'a> FatVolume<'a> {
    /// Opens the FAT volume whose boot sector is at byte `offset` of `src`.
    pub fn open(src: &'a dyn Source, offset: u64) -> Result<FatVolume<'a>> {
        let sector = src
            .read_vec(offset, 512)
            .with_context(|| format!("cannot read the boot sector at offset {offset}"))?;
        let bpb = Bpb::parse(&sector)?;
        let mut warnings = Vec::new();
        let end = offset.saturating_add(bpb.volume_bytes());
        if end > src.size() {
            warnings.push(format!(
                "the image ends {} bytes before the end of the filesystem (truncated image?)",
                end - src.size()
            ));
        }
        let entry_bytes = match bpb.fat_type {
            FatType::Fat12 => (u64::from(bpb.cluster_count) + 2) * 3 / 2 + 1,
            FatType::Fat16 => (u64::from(bpb.cluster_count) + 2) * 2,
            FatType::Fat32 => (u64::from(bpb.cluster_count) + 2) * 4,
        };
        let read_len = entry_bytes.min(bpb.fat_bytes()) as usize;
        let preferred = if bpb.fat_type == FatType::Fat32 && bpb.ext_flags & 0x80 != 0 {
            u32::from(bpb.ext_flags & 0x0F).min(bpb.num_fats - 1)
        } else {
            0
        };
        let mut order = vec![preferred];
        order.extend((0..bpb.num_fats.min(2)).filter(|&i| i != preferred));
        let mut best: Option<(u64, u32, FatTable)> = None;
        for i in order {
            let raw = match src.read_vec(offset + bpb.fat_offset(i), read_len) {
                Ok(r) => r,
                Err(e) => {
                    warnings.push(format!("cannot read FAT #{}: {e}", i + 1));
                    continue;
                }
            };
            let t = FatTable::from_bytes(&raw, bpb.fat_type, bpb.cluster_count);
            let score = t.damage_score(bpb.media);
            let better = best.as_ref().is_none_or(|(s, _, _)| score < *s);
            if better {
                best = Some((score, i, t));
            }
            if score == 0 {
                break;
            }
        }
        let (score, fat_copy, fat) = best.context("no readable FAT copy")?;
        if fat_copy != preferred {
            warnings.push(format!(
                "FAT #{} looks damaged; using FAT #{} instead",
                preferred + 1,
                fat_copy + 1
            ));
        } else if score > 0 {
            warnings.push(format!(
                "the FAT looks damaged ({} impossible entries); results may be incomplete",
                score % 1_000_000
            ));
        }
        Ok(FatVolume {
            src,
            offset,
            bpb,
            fat,
            fat_copy,
            warnings,
        })
    }

    /// The underlying source.
    pub fn source(&self) -> &'a dyn Source {
        self.src
    }

    /// Absolute byte offset of cluster `c` in the source.
    pub fn cluster_abs(&self, c: u32) -> Option<u64> {
        self.bpb.cluster_offset(c).map(|o| self.offset + o)
    }

    /// Absolute byte offset of the start and end of the data region.
    pub fn data_region(&self) -> (u64, u64) {
        let start = self.offset + self.bpb.data_offset();
        (
            start,
            start + u64::from(self.bpb.cluster_count) * self.bpb.cluster_size(),
        )
    }

    fn read_cluster(&self, c: u32) -> io::Result<Vec<u8>> {
        let off = self.cluster_abs(c).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cluster {c} out of range"),
            )
        })?;
        let mut buf = vec![0u8; self.bpb.cluster_size() as usize];
        let n = self.src.read_at(off, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn max_dir_clusters(&self) -> usize {
        (MAX_DIR_BYTES / self.bpb.cluster_size()).max(1) as usize
    }

    /// Whether a cluster's content looks like directory entries. Returns
    /// `(plausible, saw_end_marker)`.
    fn looks_like_dir(data: &[u8]) -> (bool, bool) {
        for slot in data.as_chunks::<ENTRY_SIZE>().0 {
            match parse_entry(slot) {
                RawEntry::End => return (true, true),
                RawEntry::Invalid => return (false, false),
                _ => {}
            }
        }
        (true, false)
    }

    fn read_dir(&self, loc: DirLocation, warnings: &mut Vec<String>) -> Vec<u8> {
        match loc {
            DirLocation::FixedRoot => {
                let len = u64::from(self.bpb.root_entries) * ENTRY_SIZE as u64;
                let mut buf = vec![0u8; len as usize];
                match self
                    .src
                    .read_at(self.offset + self.bpb.root_dir_offset(), &mut buf)
                {
                    Ok(n) => buf.truncate(n),
                    Err(e) => {
                        warnings.push(format!("cannot read the root directory: {e}"));
                        buf.clear();
                    }
                }
                buf
            }
            DirLocation::Chain(start) => {
                let mut out = Vec::new();
                for c in self
                    .fat
                    .chain(start)
                    .into_iter()
                    .take(self.max_dir_clusters())
                {
                    match self.read_cluster(c) {
                        Ok(d) => out.extend_from_slice(&d),
                        Err(e) => {
                            warnings.push(format!("cannot read directory cluster {c}: {e}"));
                            break;
                        }
                    }
                }
                out
            }
            DirLocation::Contiguous(start) => {
                let mut out = Vec::new();
                let mut c = start;
                while out.len() < self.max_dir_clusters() * self.bpb.cluster_size() as usize {
                    if !self.bpb.is_valid_cluster(c) || !self.fat.is_free(c) {
                        break;
                    }
                    let Ok(d) = self.read_cluster(c) else { break };
                    if c == start {
                        // A directory's first cluster always starts with "." and "..".
                        let dot = matches!(&parse_entry(&d[..ENTRY_SIZE.min(d.len())]),
                            RawEntry::Short(s) if s.is_dot() && s.is_dir());
                        if !dot {
                            break;
                        }
                    }
                    let (ok, end) = Self::looks_like_dir(&d);
                    if !ok {
                        break;
                    }
                    out.extend_from_slice(&d);
                    if end {
                        break;
                    }
                    c += 1;
                }
                out
            }
        }
    }

    /// Walks the root directory and every reachable subdirectory, including
    /// deleted ones, and returns every entry found.
    pub fn walk(&self) -> WalkResult {
        let mut res = WalkResult::default();
        let mut visited: HashSet<u32> = HashSet::new();
        let root = if self.bpb.fat_type == FatType::Fat32 {
            visited.insert(self.bpb.root_cluster);
            DirLocation::Chain(self.bpb.root_cluster)
        } else {
            DirLocation::FixedRoot
        };
        // (location, parent path, inside a deleted directory, depth)
        let mut stack: Vec<(DirLocation, String, bool, usize)> =
            vec![(root, String::new(), false, 0)];
        let mut is_root = true;
        while let Some((loc, parent, parent_deleted, depth)) = stack.pop() {
            let data = self.read_dir(loc, &mut res.warnings);
            let mut pending: Vec<LfnPart> = Vec::new();
            for slot in data.as_chunks::<ENTRY_SIZE>().0 {
                let short = match parse_entry(slot) {
                    RawEntry::End => break,
                    RawEntry::Invalid => {
                        pending.clear();
                        continue;
                    }
                    RawEntry::Lfn(p) => {
                        pending.push(p);
                        continue;
                    }
                    RawEntry::Short(s) => s,
                };
                let lfn_run = std::mem::take(&mut pending);
                if short.is_volume_label() {
                    if is_root && !short.deleted && res.volume_label.is_none() {
                        let l = String::from_utf8_lossy(&short.name).trim().to_string();
                        res.volume_label = (!l.is_empty()).then_some(l);
                    }
                    continue;
                }
                if short.is_dot() {
                    continue;
                }
                let rec = self.make_record(&short, &lfn_run, &parent, parent_deleted);
                if rec.is_dir && depth < MAX_DEPTH && self.bpb.is_valid_cluster(rec.start_cluster) {
                    let c = rec.start_cluster;
                    let next = if rec.deleted {
                        DirLocation::Contiguous(c)
                    } else {
                        DirLocation::Chain(c)
                    };
                    if visited.insert(c) {
                        stack.push((next, rec.path.clone(), rec.deleted, depth + 1));
                    }
                }
                res.records.push(rec);
            }
            is_root = false;
        }
        res.records.sort_by(|a, b| a.path.cmp(&b.path));
        res
    }

    fn make_record(
        &self,
        short: &ShortEntry,
        lfn_run: &[LfnPart],
        parent: &str,
        parent_deleted: bool,
    ) -> FileRecord {
        // The LFN fragments nearest to the short entry come first (sequence 1).
        let mut parts: Vec<&LfnPart> = Vec::new();
        if let Some(last) = lfn_run.last() {
            for p in lfn_run.iter().rev() {
                if p.checksum != last.checksum || parts.len() >= 20 {
                    break;
                }
                parts.push(p);
                if !p.deleted && p.seq & 0x40 != 0 {
                    break;
                }
            }
        }
        let checksum = parts.first().map(|p| p.checksum);
        let mut name = short.name;
        let mut long_valid = false;
        let mut first_char_guessed = false;
        if short.deleted {
            match checksum.and_then(|c| recover_first_char(&name, c)) {
                Some(ch) => {
                    name[0] = if ch == DELETED { 0x05 } else { ch };
                    long_valid = true;
                }
                None => {
                    name[0] = b'_';
                    first_char_guessed = true;
                }
            }
        } else if let Some(c) = checksum {
            long_valid = lfn_checksum(&name) == c;
        }
        let short_name = format_short_name(&name, short.nt_case);
        let long = if long_valid { decode_lfn(&parts) } else { None };
        let display = long.clone().unwrap_or_else(|| short_name.clone());
        FileRecord {
            path: format!("{parent}/{display}"),
            name: display,
            short_name,
            is_dir: short.attr & ATTR_DIR != 0,
            deleted: short.deleted || parent_deleted,
            size: if short.attr & ATTR_DIR != 0 {
                0
            } else {
                short.size
            },
            start_cluster: short.cluster,
            modified: short.modified,
            long_name: long.is_some(),
            first_char_guessed,
        }
    }
}
