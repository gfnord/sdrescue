//! Partition tables (MBR and GPT) and locating the filesystem to analyse.

use anyhow::{Result, bail};

use crate::fat::bpb::{Bpb, BpbError};
use crate::source::Source;

/// MBR partition types that hold FAT filesystems.
pub const FAT_MBR_TYPES: [u8; 6] = [0x01, 0x04, 0x06, 0x0B, 0x0C, 0x0E];

/// Which kind of partition table was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// Classic DOS/MBR partition table.
    Mbr,
    /// GUID partition table.
    Gpt,
}

/// Partition type identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartType {
    /// MBR type byte.
    Mbr(u8),
    /// GPT type GUID (on-disk byte order).
    Gpt([u8; 16]),
}

/// One partition entry.
#[derive(Debug, Clone)]
pub struct Partition {
    /// 1-based index in the table.
    pub index: usize,
    /// Partition type.
    pub kind: PartType,
    /// Start in bytes from the beginning of the source.
    pub start: u64,
    /// Length in bytes.
    pub size: u64,
    /// MBR active flag.
    pub bootable: bool,
    /// GPT partition name.
    pub name: Option<String>,
}

impl Partition {
    /// Human-readable partition type.
    pub fn type_name(&self) -> String {
        match self.kind {
            PartType::Mbr(t) => format!("0x{t:02X} {}", mbr_type_name(t)),
            PartType::Gpt(g) => {
                let s = format_guid(&g);
                let name = match s.as_str() {
                    "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" => "Microsoft basic data",
                    "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" => "EFI system",
                    "E3C9E316-0B5C-4DB8-817D-F92DF00215AE" => "Microsoft reserved",
                    "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux filesystem",
                    _ => "unknown",
                };
                format!("{s} {name}")
            }
        }
    }

    /// Whether the type says this partition holds FAT (or Windows basic data).
    pub fn is_fat_type(&self) -> bool {
        match self.kind {
            PartType::Mbr(t) => FAT_MBR_TYPES.contains(&t),
            PartType::Gpt(g) => {
                let s = format_guid(&g);
                s == "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
                    || s == "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
            }
        }
    }
}

/// A parsed partition table.
#[derive(Debug, Clone)]
pub struct PartitionTable {
    /// MBR or GPT.
    pub scheme: Scheme,
    /// Non-empty partitions in table order.
    pub partitions: Vec<Partition>,
}

/// Name of an MBR partition type byte.
pub fn mbr_type_name(t: u8) -> &'static str {
    match t {
        0x01 => "FAT12",
        0x04 => "FAT16 (<32 MB)",
        0x05 => "Extended",
        0x06 => "FAT16",
        0x07 => "NTFS/exFAT",
        0x0B => "FAT32 (CHS)",
        0x0C => "FAT32 (LBA)",
        0x0E => "FAT16 (LBA)",
        0x0F => "Extended (LBA)",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0xEE => "GPT protective",
        0xEF => "EFI system",
        _ => "unknown",
    }
}

/// Formats a GPT GUID stored in on-disk (mixed-endian) order.
pub fn format_guid(g: &[u8; 16]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        g[3],
        g[2],
        g[1],
        g[0],
        g[5],
        g[4],
        g[7],
        g[6],
        g[8],
        g[9],
        g[10],
        g[11],
        g[12],
        g[13],
        g[14],
        g[15]
    )
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or([0; 8]))
}

/// Parses the four primary MBR entries of `sector0`. `None` if there is no
/// valid MBR signature or the entries are nonsense.
pub fn parse_mbr(sector0: &[u8], source_size: u64) -> Option<Vec<Partition>> {
    if sector0.len() < 512 || sector0[510] != 0x55 || sector0[511] != 0xAA {
        return None;
    }
    let mut parts = Vec::new();
    for i in 0..4 {
        let e = &sector0[446 + i * 16..446 + (i + 1) * 16];
        let status = e[0];
        let kind = e[4];
        let lba = u64::from(le32(e, 8));
        let count = u64::from(le32(e, 12));
        if kind == 0 || count == 0 {
            continue;
        }
        if status != 0x00 && status != 0x80 {
            return None;
        }
        if lba == 0 {
            return None;
        }
        // Tolerate partitions running past the end of a truncated image, but
        // not ones starting beyond it.
        if lba * 512 >= source_size.max(512) {
            return None;
        }
        parts.push(Partition {
            index: i + 1,
            kind: PartType::Mbr(kind),
            start: lba * 512,
            size: count * 512,
            bootable: status == 0x80,
            name: None,
        });
    }
    (!parts.is_empty()).then_some(parts)
}

/// Parses a GPT whose header is at LBA 1 with the given sector size.
fn parse_gpt(src: &dyn Source, sector: u64) -> Option<Vec<Partition>> {
    let hdr = src.read_vec(sector, 92).ok()?;
    if &hdr[0..8] != b"EFI PART" {
        return None;
    }
    let entries_lba = le64(&hdr, 72);
    let count = le32(&hdr, 80).min(1024) as usize;
    let esize = le32(&hdr, 84) as usize;
    if !(128..=1024).contains(&esize) {
        return None;
    }
    let table = src
        .read_vec(entries_lba.checked_mul(sector)?, count * esize)
        .ok()?;
    let mut parts = Vec::new();
    for (i, e) in table.chunks_exact(esize).enumerate() {
        let mut kind = [0u8; 16];
        kind.copy_from_slice(&e[0..16]);
        if kind == [0u8; 16] {
            continue;
        }
        let first = le64(e, 32);
        let last = le64(e, 40);
        if last < first {
            continue;
        }
        let units: Vec<u16> = e[56..128]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .take_while(|&u| u != 0)
            .collect();
        let name = String::from_utf16_lossy(&units);
        parts.push(Partition {
            index: i + 1,
            kind: PartType::Gpt(kind),
            start: first.saturating_mul(sector),
            size: (last - first + 1).saturating_mul(sector),
            bootable: false,
            name: (!name.is_empty()).then_some(name),
        });
    }
    Some(parts)
}

/// Reads the partition table of `src`, if it has one.
pub fn read_partition_table(src: &dyn Source) -> Result<Option<PartitionTable>> {
    let s0 = src.read_vec(0, 512)?;
    let Some(mbr) = parse_mbr(&s0, src.size()) else {
        return Ok(None);
    };
    if mbr.iter().any(|p| p.kind == PartType::Mbr(0xEE)) {
        for ss in [512u64, 4096] {
            if let Some(parts) = parse_gpt(src, ss) {
                return Ok(Some(PartitionTable {
                    scheme: Scheme::Gpt,
                    partitions: parts,
                }));
            }
        }
    }
    Ok(Some(PartitionTable {
        scheme: Scheme::Mbr,
        partitions: mbr,
    }))
}

/// What filesystem lives at a given offset.
#[derive(Debug, Clone)]
pub enum FsKind {
    /// A FAT12/16/32 filesystem.
    Fat(Box<Bpb>),
    /// exFAT (not supported by `undelete` yet).
    ExFat,
    /// NTFS (not supported by `undelete`).
    Ntfs,
    /// Nothing recognisable; carries the BPB validation error.
    Unknown(BpbError),
}

impl FsKind {
    /// Short name for messages.
    pub fn name(&self) -> String {
        match self {
            FsKind::Fat(b) => b.fat_type.to_string(),
            FsKind::ExFat => "exFAT".into(),
            FsKind::Ntfs => "NTFS".into(),
            FsKind::Unknown(_) => "unknown".into(),
        }
    }
}

/// Identifies the filesystem whose boot sector is at `offset`.
pub fn probe_fs(src: &dyn Source, offset: u64) -> FsKind {
    let Ok(s) = src.read_vec(offset, 512) else {
        return FsKind::Unknown(BpbError(format!(
            "cannot read 512 bytes at offset {offset}"
        )));
    };
    match &s[3..11] {
        b"EXFAT   " => return FsKind::ExFat,
        b"NTFS    " => return FsKind::Ntfs,
        _ => {}
    }
    match Bpb::parse(&s) {
        Ok(b) => FsKind::Fat(Box::new(b)),
        Err(e) => FsKind::Unknown(e),
    }
}

/// Where the filesystem to analyse is, and what it is.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Partition table, if sector 0 holds one.
    pub table: Option<PartitionTable>,
    /// Index into `table.partitions` of the chosen partition.
    pub chosen: Option<usize>,
    /// Byte offset of the filesystem's boot sector.
    pub fs_offset: u64,
    /// The filesystem found there.
    pub fs: FsKind,
}

/// Locates the filesystem: `--offset` if given, else a FAT boot sector in
/// sector 0, else the FAT partition of the MBR/GPT (or the first non-empty one).
pub fn locate(src: &dyn Source, offset_override: Option<u64>) -> Result<Layout> {
    if src.size() < 512 {
        bail!(
            "the source is only {} bytes; that is not a disk image",
            src.size()
        );
    }
    let table = read_partition_table(src)?;
    if let Some(off) = offset_override {
        return Ok(Layout {
            table,
            chosen: None,
            fs_offset: off,
            fs: probe_fs(src, off),
        });
    }
    let at0 = probe_fs(src, 0);
    if !matches!(at0, FsKind::Unknown(_)) {
        return Ok(Layout {
            table: None,
            chosen: None,
            fs_offset: 0,
            fs: at0,
        });
    }
    let Some(t) = table else {
        return Ok(Layout {
            table: None,
            chosen: None,
            fs_offset: 0,
            fs: at0,
        });
    };
    let probes: Vec<FsKind> = t
        .partitions
        .iter()
        .map(|p| probe_fs(src, p.start))
        .collect();
    let chosen = probes
        .iter()
        .position(|f| matches!(f, FsKind::Fat(_)))
        .or_else(|| t.partitions.iter().position(Partition::is_fat_type))
        .or_else(|| (!t.partitions.is_empty()).then_some(0));
    let (fs_offset, fs) = match chosen {
        Some(i) => (t.partitions[i].start, probes[i].clone()),
        None => (0, at0),
    };
    Ok(Layout {
        table: Some(t),
        chosen,
        fs_offset,
        fs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fat::bpb::tests::boot_sector;
    use crate::source::MemSource;

    fn mbr_with(entries: &[(u8, u8, u32, u32)]) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        for (i, &(status, kind, lba, count)) in entries.iter().enumerate() {
            let e = &mut s[446 + i * 16..446 + (i + 1) * 16];
            e[0] = status;
            e[4] = kind;
            e[8..12].copy_from_slice(&lba.to_le_bytes());
            e[12..16].copy_from_slice(&count.to_le_bytes());
        }
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    #[test]
    fn finds_fat_partition_behind_mbr() {
        let mut img = mbr_with(&[(0x00, 0x83, 8, 8), (0x80, 0x0C, 16, 32768)]);
        img.resize(16 * 512, 0);
        img.extend(boot_sector(512, 4, 4, 2, 512, 32768, 32, false));
        img.resize(16 * 512 + 32768 * 512, 0);
        let src = MemSource(img);
        let l = locate(&src, None).unwrap();
        assert_eq!(l.fs_offset, 16 * 512);
        assert_eq!(l.chosen, Some(1));
        assert!(matches!(l.fs, FsKind::Fat(_)));
        let t = l.table.unwrap();
        assert_eq!(t.scheme, Scheme::Mbr);
        assert_eq!(t.partitions.len(), 2);
        assert!(t.partitions[1].bootable);
    }

    #[test]
    fn boot_sector_at_zero_wins() {
        let mut img = boot_sector(512, 4, 4, 2, 512, 32768, 32, false);
        img.resize(32768 * 512, 0);
        let l = locate(&MemSource(img), None).unwrap();
        assert_eq!(l.fs_offset, 0);
        assert!(l.table.is_none());
        assert!(matches!(l.fs, FsKind::Fat(_)));
    }

    #[test]
    fn rejects_bad_mbr_entries() {
        assert!(parse_mbr(&mbr_with(&[(0x33, 0x0C, 16, 100)]), 1 << 20).is_none());
        assert!(parse_mbr(&mbr_with(&[(0x00, 0x0C, 1 << 30, 100)]), 1 << 20).is_none());
        assert!(parse_mbr(&[0u8; 512], 1 << 20).is_none());
    }

    #[test]
    fn detects_exfat() {
        let mut s = vec![0u8; 4096];
        s[3..11].copy_from_slice(b"EXFAT   ");
        assert!(matches!(probe_fs(&MemSource(s), 0), FsKind::ExFat));
    }

    #[test]
    fn guid_format() {
        let g = [
            0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26,
            0x99, 0xC7,
        ];
        assert_eq!(format_guid(&g), "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7");
    }
}
