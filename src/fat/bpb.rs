//! BIOS Parameter Block (FAT boot sector) parsing and validation.

use std::fmt;

/// FAT variant, decided from the cluster count (Microsoft's rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FatType {
    /// Fewer than 4085 clusters, 12-bit FAT entries.
    Fat12,
    /// Fewer than 65525 clusters, 16-bit FAT entries.
    Fat16,
    /// 65525 clusters or more, 28-bit FAT entries.
    Fat32,
}

impl fmt::Display for FatType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FatType::Fat12 => "FAT12",
            FatType::Fat16 => "FAT16",
            FatType::Fat32 => "FAT32",
        })
    }
}

impl FatType {
    /// Microsoft's rule: <4085 clusters is FAT12, <65525 is FAT16, otherwise FAT32.
    pub fn from_cluster_count(clusters: u64) -> FatType {
        if clusters < 4085 {
            FatType::Fat12
        } else if clusters < 65525 {
            FatType::Fat16
        } else {
            FatType::Fat32
        }
    }
}

/// Parsed and validated boot sector geometry.
#[derive(Debug, Clone)]
pub struct Bpb {
    /// OEM name (bytes 3..11), informative only.
    pub oem_name: String,
    /// Bytes per logical sector (512, 1024, 2048 or 4096).
    pub bytes_per_sector: u32,
    /// Sectors per cluster (power of two, 1..=128).
    pub sectors_per_cluster: u32,
    /// Sectors before the first FAT.
    pub reserved_sectors: u32,
    /// Number of FAT copies.
    pub num_fats: u32,
    /// Fixed root directory entries (FAT12/16; 0 on FAT32).
    pub root_entries: u32,
    /// Total sectors in the volume.
    pub total_sectors: u64,
    /// Media descriptor byte (0xF8 for fixed media).
    pub media: u8,
    /// Sectors occupied by one FAT.
    pub fat_size_sectors: u32,
    /// Hidden sectors (the partition's LBA, as recorded by the formatter).
    pub hidden_sectors: u32,
    /// First cluster of the root directory (FAT32 only).
    pub root_cluster: u32,
    /// FAT32 extended flags (bit 7 = mirroring disabled, bits 0-3 = active FAT).
    pub ext_flags: u16,
    /// Volume serial number, if an extended boot signature is present.
    pub volume_id: Option<u32>,
    /// Volume label from the boot sector, if present.
    pub label: Option<String>,
    /// Filesystem type string from the boot sector (informative only).
    pub fs_type_label: Option<String>,
    /// FAT variant derived from the cluster count.
    pub fat_type: FatType,
    /// Number of data clusters (clusters are numbered 2..cluster_count+2).
    pub cluster_count: u32,
    /// Sectors used by the fixed root directory (0 on FAT32).
    pub root_dir_sectors: u32,
    /// First sector of the data region (cluster 2).
    pub first_data_sector: u64,
}

/// Why a sector is not a usable FAT boot sector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BpbError(pub String);

impl fmt::Display for BpbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a valid FAT boot sector: {}", self.0)
    }
}

impl std::error::Error for BpbError {}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn text(b: &[u8]) -> Option<String> {
    let s: String = b
        .iter()
        .map(|&c| {
            if (0x20..0x7F).contains(&c) {
                c as char
            } else {
                ' '
            }
        })
        .collect();
    let t = s.trim().to_string();
    (!t.is_empty() && t != "NO NAME").then_some(t)
}

impl Bpb {
    /// Parses and validates a boot sector (at least 512 bytes).
    pub fn parse(sector: &[u8]) -> Result<Bpb, BpbError> {
        let err = |m: String| Err(BpbError(m));
        if sector.len() < 512 {
            return err(format!("boot sector is only {} bytes", sector.len()));
        }
        if &sector[3..11] == b"EXFAT   " {
            return err("this is an exFAT filesystem".into());
        }
        if &sector[3..11] == b"NTFS    " {
            return err("this is an NTFS filesystem".into());
        }
        let jump_ok = sector[0] == 0xEB || sector[0] == 0xE9 || sector[0] == 0xE8;
        if !jump_ok {
            return err(format!(
                "first byte is 0x{:02X}, expected a jump instruction (0xEB or 0xE9)",
                sector[0]
            ));
        }
        let bps = u32::from(le16(sector, 11));
        if !matches!(bps, 512 | 1024 | 2048 | 4096) {
            return err(format!(
                "bytes per sector is {bps} (must be 512, 1024, 2048 or 4096)"
            ));
        }
        let spc = u32::from(sector[13]);
        if spc == 0 || !spc.is_power_of_two() {
            return err(format!(
                "sectors per cluster is {spc} (must be a power of two from 1 to 128)"
            ));
        }
        if bps * spc > 256 * 1024 {
            return err(format!("cluster size {} bytes is too large", bps * spc));
        }
        let reserved = u32::from(le16(sector, 14));
        if reserved == 0 {
            return err("reserved sector count is 0".into());
        }
        let num_fats = u32::from(sector[16]);
        if !(1..=4).contains(&num_fats) {
            return err(format!("number of FATs is {num_fats} (expected 1 or 2)"));
        }
        let root_entries = u32::from(le16(sector, 17));
        let ts16 = u64::from(le16(sector, 19));
        let media = sector[21];
        let fsz16 = u32::from(le16(sector, 22));
        let hidden = le32(sector, 28);
        let ts32 = u64::from(le32(sector, 32));
        let total = if ts16 != 0 { ts16 } else { ts32 };
        if total == 0 {
            return err("total sector count is 0".into());
        }
        let fat32_layout = fsz16 == 0;
        let fat_size = if fat32_layout {
            le32(sector, 36)
        } else {
            fsz16
        };
        if fat_size == 0 {
            return err("FAT size is 0".into());
        }
        let root_dir_sectors = (root_entries * 32).div_ceil(bps);
        let first_data = u64::from(reserved)
            + u64::from(num_fats) * u64::from(fat_size)
            + u64::from(root_dir_sectors);
        if first_data >= total {
            return err(format!(
                "the FATs and root directory ({first_data} sectors) do not fit in the volume ({total} sectors)"
            ));
        }
        let clusters = (total - first_data) / u64::from(spc);
        if clusters == 0 {
            return err("the volume has no data clusters".into());
        }
        if clusters > 0x0FFF_FFF5 {
            return err(format!("cluster count {clusters} is too large for FAT"));
        }
        let fat_type = FatType::from_cluster_count(clusters);
        // The FAT must be big enough to describe every cluster.
        let fat_bytes = u64::from(fat_size) * u64::from(bps);
        let needed = match fat_type {
            FatType::Fat12 => (clusters + 2) * 3 / 2,
            FatType::Fat16 => (clusters + 2) * 2,
            FatType::Fat32 => (clusters + 2) * 4,
        };
        if fat_bytes < needed {
            return err(format!(
                "FAT size ({fat_bytes} bytes) is too small for {clusters} clusters"
            ));
        }
        let (root_cluster, ext_flags, sig_off) = if fat_type == FatType::Fat32 {
            if root_entries != 0 {
                return err(format!(
                    "FAT32 volume declares {root_entries} fixed root entries (must be 0)"
                ));
            }
            let rc = le32(sector, 44);
            if rc < 2 || u64::from(rc) >= clusters + 2 {
                return err(format!("root directory cluster {rc} is out of range"));
            }
            (rc, le16(sector, 40), 64)
        } else {
            if root_entries == 0 {
                return err(format!("{fat_type} volume has no root directory entries"));
            }
            (0, 0, 36)
        };
        let (volume_id, label, fs_type_label) = if sector[sig_off + 2] == 0x29 {
            (
                Some(le32(sector, sig_off + 3)),
                text(&sector[sig_off + 7..sig_off + 18]),
                text(&sector[sig_off + 18..sig_off + 26]),
            )
        } else {
            (None, None, None)
        };
        Ok(Bpb {
            oem_name: text(&sector[3..11]).unwrap_or_default(),
            bytes_per_sector: bps,
            sectors_per_cluster: spc,
            reserved_sectors: reserved,
            num_fats,
            root_entries,
            total_sectors: total,
            media,
            fat_size_sectors: fat_size,
            hidden_sectors: hidden,
            root_cluster,
            ext_flags,
            volume_id,
            label,
            fs_type_label,
            fat_type,
            cluster_count: clusters as u32,
            root_dir_sectors,
            first_data_sector: first_data,
        })
    }

    /// Cluster size in bytes.
    pub fn cluster_size(&self) -> u64 {
        u64::from(self.bytes_per_sector) * u64::from(self.sectors_per_cluster)
    }

    /// Byte offset of FAT copy `index` (0-based), relative to the volume start.
    pub fn fat_offset(&self, index: u32) -> u64 {
        (u64::from(self.reserved_sectors) + u64::from(index) * u64::from(self.fat_size_sectors))
            * u64::from(self.bytes_per_sector)
    }

    /// Size of one FAT copy in bytes.
    pub fn fat_bytes(&self) -> u64 {
        u64::from(self.fat_size_sectors) * u64::from(self.bytes_per_sector)
    }

    /// Byte offset of the fixed root directory (FAT12/16), relative to the volume start.
    pub fn root_dir_offset(&self) -> u64 {
        self.fat_offset(self.num_fats)
    }

    /// Byte offset of the data region (cluster 2), relative to the volume start.
    pub fn data_offset(&self) -> u64 {
        self.first_data_sector * u64::from(self.bytes_per_sector)
    }

    /// Size of the volume in bytes.
    pub fn volume_bytes(&self) -> u64 {
        self.total_sectors * u64::from(self.bytes_per_sector)
    }

    /// Whether `c` is a valid data cluster number.
    pub fn is_valid_cluster(&self, c: u32) -> bool {
        c >= 2 && u64::from(c) < u64::from(self.cluster_count) + 2
    }

    /// Byte offset of cluster `c`, relative to the volume start. `None` if out of range.
    pub fn cluster_offset(&self, c: u32) -> Option<u64> {
        self.is_valid_cluster(c)
            .then(|| self.data_offset() + u64::from(c - 2) * self.cluster_size())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a minimal boot sector for tests.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn boot_sector(
        bps: u16,
        spc: u8,
        reserved: u16,
        fats: u8,
        root_entries: u16,
        total: u32,
        fat_size: u32,
        fat32: bool,
    ) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[0] = 0xEB;
        s[1] = 0x3C;
        s[2] = 0x90;
        s[3..11].copy_from_slice(b"MSDOS5.0");
        s[11..13].copy_from_slice(&bps.to_le_bytes());
        s[13] = spc;
        s[14..16].copy_from_slice(&reserved.to_le_bytes());
        s[16] = fats;
        s[17..19].copy_from_slice(&root_entries.to_le_bytes());
        if total < 0x10000 && !fat32 {
            s[19..21].copy_from_slice(&(total as u16).to_le_bytes());
        } else {
            s[32..36].copy_from_slice(&total.to_le_bytes());
        }
        s[21] = 0xF8;
        if fat32 {
            s[36..40].copy_from_slice(&fat_size.to_le_bytes());
            s[44..48].copy_from_slice(&2u32.to_le_bytes());
            s[66] = 0x29;
            s[67..71].copy_from_slice(&0x1234_5678u32.to_le_bytes());
            s[71..82].copy_from_slice(b"MYCARD     ");
            s[82..90].copy_from_slice(b"FAT32   ");
        } else {
            s[22..24].copy_from_slice(&(fat_size as u16).to_le_bytes());
            s[38] = 0x29;
            s[39..43].copy_from_slice(&0xCAFE_BABEu32.to_le_bytes());
            s[43..54].copy_from_slice(b"NO NAME    ");
            s[54..62].copy_from_slice(b"FAT16   ");
        }
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    #[test]
    fn fat_type_thresholds() {
        assert_eq!(FatType::from_cluster_count(4084), FatType::Fat12);
        assert_eq!(FatType::from_cluster_count(4085), FatType::Fat16);
        assert_eq!(FatType::from_cluster_count(65524), FatType::Fat16);
        assert_eq!(FatType::from_cluster_count(65525), FatType::Fat32);
    }

    #[test]
    fn parses_fat16() {
        // 16 MiB volume, 2 KiB clusters.
        let s = boot_sector(512, 4, 4, 2, 512, 32768, 32, false);
        let b = Bpb::parse(&s).unwrap();
        assert_eq!(b.fat_type, FatType::Fat16);
        assert_eq!(b.root_dir_sectors, 32);
        assert_eq!(b.first_data_sector, 4 + 64 + 32);
        assert_eq!(b.cluster_count, (32768 - 100) / 4);
        assert_eq!(b.cluster_size(), 2048);
        assert_eq!(b.label, None);
        assert_eq!(b.volume_id, Some(0xCAFE_BABE));
        assert_eq!(b.cluster_offset(2), Some(100 * 512));
        assert_eq!(b.cluster_offset(1), None);
    }

    #[test]
    fn parses_fat12_and_fat32() {
        let s = boot_sector(512, 4, 1, 2, 512, 8192, 6, false);
        assert_eq!(Bpb::parse(&s).unwrap().fat_type, FatType::Fat12);
        let s = boot_sector(512, 1, 32, 2, 0, 1_048_576, 8192, true);
        let b = Bpb::parse(&s).unwrap();
        assert_eq!(b.fat_type, FatType::Fat32);
        assert_eq!(b.root_cluster, 2);
        assert_eq!(b.label.as_deref(), Some("MYCARD"));
    }

    #[test]
    fn rejects_nonsense() {
        let good = boot_sector(512, 4, 4, 2, 512, 32768, 32, false);
        let mut s = good.clone();
        s[11] = 37;
        s[12] = 0;
        assert!(Bpb::parse(&s).unwrap_err().0.contains("bytes per sector"));
        let mut s = good.clone();
        s[13] = 3;
        assert!(
            Bpb::parse(&s)
                .unwrap_err()
                .0
                .contains("sectors per cluster")
        );
        let mut s = good.clone();
        s[16] = 0;
        assert!(Bpb::parse(&s).is_err());
        let mut s = good.clone();
        s[22] = 1; // FAT far too small
        s[23] = 0;
        assert!(Bpb::parse(&s).unwrap_err().0.contains("too small"));
        let mut s = good;
        s[3..11].copy_from_slice(b"EXFAT   ");
        assert!(Bpb::parse(&s).unwrap_err().0.contains("exFAT"));
        assert!(Bpb::parse(&[0u8; 512]).is_err());
        assert!(Bpb::parse(&[0u8; 10]).is_err());
    }
}
