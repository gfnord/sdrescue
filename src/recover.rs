//! Deciding whether a deleted file can still be recovered, and recovering it.
//!
//! A deleted file's FAT chain is zeroed, so its data is assumed to be stored
//! in consecutive clusters starting at the start cluster.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::fat::{FatVolume, FileRecord};
use crate::filetype::{
    Category, SIGNATURE_PEEK, SignatureCheck, category_of_name, check_signature, extension,
};

/// Recoverability of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Every cluster the file needs is still free.
    Good,
    /// Some (not the first) of the needed clusters were reused.
    Partial,
    /// The first cluster belongs to another file now (or the entry has no data location).
    Overwritten,
    /// Not deleted (shown with `list --all`).
    Existing,
}

impl Status {
    /// Text used in tables and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Good => "good",
            Status::Partial => "partial",
            Status::Overwritten => "overwritten",
            Status::Existing => "existing",
        }
    }
}

/// A file record with its recoverability analysis.
#[derive(Debug, Clone, Serialize)]
pub struct Analyzed {
    /// The directory entry.
    #[serde(flatten)]
    pub rec: FileRecord,
    /// Modified time as text (`YYYY-MM-DD HH:MM:SS`), if valid.
    pub modified: Option<String>,
    /// Recoverability.
    pub status: Status,
    /// Magic-number check of the first bytes.
    pub signature: SignatureCheck,
    /// Media category from the extension.
    pub category: Option<Category>,
}

/// Number of clusters a file of `size` bytes occupies.
fn clusters_needed(vol: &FatVolume, size: u32) -> u64 {
    u64::from(size).div_ceil(vol.bpb.cluster_size())
}

/// Status of a deleted file assumed to be stored contiguously from `start`.
pub fn deleted_status(vol: &FatVolume, start: u32, size: u32) -> Status {
    let n = clusters_needed(vol, size);
    if n == 0 {
        return Status::Good;
    }
    if !vol.bpb.is_valid_cluster(start) || !vol.fat.is_free(start) {
        return Status::Overwritten;
    }
    let last_valid = u64::from(vol.bpb.cluster_count) + 1;
    let end = u64::from(start) + n - 1;
    let all_free = end <= last_valid && (u64::from(start)..=end).all(|c| vol.fat.is_free(c as u32));
    if all_free {
        Status::Good
    } else {
        Status::Partial
    }
}

/// Reads the first bytes of the file's data (from its start cluster).
fn read_head(vol: &FatVolume, start: u32, size: u32) -> Option<Vec<u8>> {
    let off = vol.cluster_abs(start)?;
    let len = SIGNATURE_PEEK.min(size as usize);
    let mut buf = vec![0u8; len];
    let n = vol.source().read_at(off, &mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Analyses one record (files only; directories get `n/a`).
pub fn analyze(vol: &FatVolume, rec: &FileRecord) -> Analyzed {
    let status = if rec.deleted {
        deleted_status(vol, rec.start_cluster, rec.size)
    } else {
        Status::Existing
    };
    let signature = match extension(&rec.name) {
        Some(ext) if !rec.is_dir && rec.size > 0 => {
            match read_head(vol, rec.start_cluster, rec.size) {
                Some(head) if !head.is_empty() => check_signature(&ext, &head),
                _ => SignatureCheck::NotApplicable,
            }
        }
        _ => SignatureCheck::NotApplicable,
    };
    Analyzed {
        rec: rec.clone(),
        modified: rec.modified.map(|m| m.to_string()),
        status,
        signature,
        category: category_of_name(&rec.name),
    }
}

/// Copies a deleted file's data (assumed contiguous) into `out`.
/// Returns the number of bytes written (less than the size if the image ends early).
pub fn copy_contiguous(vol: &FatVolume, rec: &FileRecord, out: &mut impl Write) -> Result<u64> {
    let size = u64::from(rec.size);
    if size == 0 {
        return Ok(0);
    }
    let start = vol
        .cluster_abs(rec.start_cluster)
        .with_context(|| format!("start cluster {} is outside the volume", rec.start_cluster))?;
    let (_, data_end) = vol.data_region();
    let end = (start + size).min(data_end).min(vol.source().size());
    let mut pos = start;
    let mut buf = vec![0u8; 1 << 20];
    while pos < end {
        let want = ((end - pos) as usize).min(buf.len());
        let n = vol
            .source()
            .read_at(pos, &mut buf[..want])
            .with_context(|| format!("read error at offset {pos}"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        pos += n as u64;
    }
    Ok(pos - start)
}

/// Destination path of a recovered file inside `out_dir`, keeping folders.
pub fn destination(out_dir: &Path, rec_path: &str) -> std::path::PathBuf {
    let mut p = out_dir.to_path_buf();
    for comp in rec_path.split('/').filter(|c| !c.is_empty()) {
        p.push(crate::output::sanitize_component(comp));
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_keeps_structure() {
        let d = destination(Path::new("out"), "/DCIM/100CANON/IMG:1.JPG");
        assert_eq!(
            d,
            Path::new("out")
                .join("DCIM")
                .join("100CANON")
                .join("IMG_1.JPG")
        );
    }
}
