//! The File Allocation Table: decoding FAT12/16/32 entries, chains and health checks.

use super::bpb::FatType;

/// Meaning of one FAT entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatEntry {
    /// The cluster is unallocated.
    Free,
    /// The chain continues at this cluster.
    Next(u32),
    /// Last cluster of a chain.
    End,
    /// The cluster is marked bad.
    Bad,
    /// Reserved value, or a link pointing outside the volume.
    Invalid,
}

/// An in-memory copy of one FAT, with entries widened to `u32`.
#[derive(Debug, Clone)]
pub struct FatTable {
    entries: Vec<u32>,
    fat_type: FatType,
    cluster_count: u32,
}

/// Decodes raw FAT bytes into `count` entries (missing bytes decode as 0).
pub fn decode_entries(raw: &[u8], fat_type: FatType, count: usize) -> Vec<u32> {
    let get = |i: usize| raw.get(i).copied().unwrap_or(0);
    (0..count)
        .map(|n| match fat_type {
            FatType::Fat12 => {
                let o = n + n / 2;
                let v = u16::from_le_bytes([get(o), get(o + 1)]);
                u32::from(if n & 1 == 1 { v >> 4 } else { v & 0x0FFF })
            }
            FatType::Fat16 => u32::from(u16::from_le_bytes([get(2 * n), get(2 * n + 1)])),
            FatType::Fat32 => {
                u32::from_le_bytes([get(4 * n), get(4 * n + 1), get(4 * n + 2), get(4 * n + 3)])
                    & 0x0FFF_FFFF
            }
        })
        .collect()
}

impl FatTable {
    /// Builds a table from raw FAT bytes for a volume with `cluster_count` data clusters.
    pub fn from_bytes(raw: &[u8], fat_type: FatType, cluster_count: u32) -> FatTable {
        let entries = decode_entries(raw, fat_type, cluster_count as usize + 2);
        FatTable {
            entries,
            fat_type,
            cluster_count,
        }
    }

    /// Raw value stored for cluster `c` (0 if out of range).
    pub fn raw(&self, c: u32) -> u32 {
        self.entries.get(c as usize).copied().unwrap_or(0)
    }

    fn classify(&self, v: u32) -> FatEntry {
        let (bad, eoc_min) = match self.fat_type {
            FatType::Fat12 => (0xFF7, 0xFF8),
            FatType::Fat16 => (0xFFF7, 0xFFF8),
            FatType::Fat32 => (0x0FFF_FFF7, 0x0FFF_FFF8),
        };
        if v == 0 {
            FatEntry::Free
        } else if v == bad {
            FatEntry::Bad
        } else if v >= eoc_min {
            FatEntry::End
        } else if v >= 2 && v < self.cluster_count + 2 {
            FatEntry::Next(v)
        } else {
            FatEntry::Invalid
        }
    }

    /// Decoded entry for cluster `c`.
    pub fn entry(&self, c: u32) -> FatEntry {
        if c < 2 || c >= self.cluster_count + 2 {
            return FatEntry::Invalid;
        }
        self.classify(self.raw(c))
    }

    /// True if cluster `c` is a valid data cluster marked free.
    pub fn is_free(&self, c: u32) -> bool {
        self.entry(c) == FatEntry::Free
    }

    /// Number of data clusters.
    pub fn cluster_count(&self) -> u32 {
        self.cluster_count
    }

    /// Number of free data clusters.
    pub fn free_count(&self) -> u32 {
        (2..self.cluster_count + 2)
            .filter(|&c| self.is_free(c))
            .count() as u32
    }

    /// Follows the chain starting at `start`. Stops at end-of-chain, at any
    /// invalid/free/bad link, and when a cluster repeats (loop guard).
    pub fn chain(&self, start: u32) -> Vec<u32> {
        let mut out = Vec::new();
        if start < 2 || start >= self.cluster_count + 2 {
            return out;
        }
        let mut seen = vec![false; self.cluster_count as usize + 2];
        let mut c = start;
        loop {
            if seen[c as usize] {
                break;
            }
            seen[c as usize] = true;
            out.push(c);
            match self.entry(c) {
                FatEntry::Next(n) => c = n,
                _ => break,
            }
        }
        out
    }

    /// A health score: number of entries that hold impossible values, plus a
    /// large penalty when entry 0 does not carry the media descriptor.
    pub fn damage_score(&self, media: u8) -> u64 {
        let mask = match self.fat_type {
            FatType::Fat12 => 0xFFF,
            FatType::Fat16 => 0xFFFF,
            FatType::Fat32 => 0x0FFF_FFFF,
        };
        let e0 = self.raw(0);
        let expected0 = (mask & !0xFF) | u32::from(media);
        let mut score = if e0 == expected0 { 0 } else { 1_000_000 };
        score += (2..self.cluster_count + 2)
            .filter(|&c| self.entry(c) == FatEntry::Invalid)
            .count() as u64;
        score
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_fat12_packing() {
        // Entries: 0xFF8, 0xFFF, 0x003, 0xFFF (cluster 2 -> 3 -> end).
        let raw = [0xF8, 0xFF, 0xFF, 0x03, 0xF0, 0xFF];
        let e = decode_entries(&raw, FatType::Fat12, 4);
        assert_eq!(e, vec![0xFF8, 0xFFF, 0x003, 0xFFF]);
        let t = FatTable::from_bytes(&raw, FatType::Fat12, 10);
        assert_eq!(t.entry(2), FatEntry::Next(3));
        assert_eq!(t.entry(3), FatEntry::End);
        assert_eq!(t.chain(2), vec![2, 3]);
        assert_eq!(t.damage_score(0xF8), 0);
    }

    #[test]
    fn decodes_fat16_and_fat32() {
        let raw16 = [
            0xF8, 0xFF, 0xFF, 0xFF, 0x03, 0x00, 0xFF, 0xFF, 0xF7, 0xFF, 0, 0,
        ];
        let t = FatTable::from_bytes(&raw16, FatType::Fat16, 10);
        assert_eq!(t.entry(2), FatEntry::Next(3));
        assert_eq!(t.entry(3), FatEntry::End);
        assert_eq!(t.entry(4), FatEntry::Bad);
        assert_eq!(t.entry(5), FatEntry::Free);
        let mut raw32 = vec![0u8; 24];
        raw32[0..4].copy_from_slice(&0x0FFF_FFF8u32.to_le_bytes());
        raw32[8..12].copy_from_slice(&0xF000_0003u32.to_le_bytes()); // high nibble ignored
        raw32[12..16].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
        raw32[16..20].copy_from_slice(&999u32.to_le_bytes()); // out of range
        let t = FatTable::from_bytes(&raw32, FatType::Fat32, 4);
        assert_eq!(t.entry(2), FatEntry::Next(3));
        assert_eq!(t.entry(3), FatEntry::End);
        assert_eq!(t.entry(4), FatEntry::Invalid);
        assert_eq!(t.free_count(), 1);
        assert_eq!(t.entry(100), FatEntry::Invalid);
    }

    #[test]
    fn chain_stops_on_loops() {
        // 2 -> 3 -> 2 (loop)
        let raw = [0xF8, 0xFF, 0xFF, 0xFF, 3, 0, 2, 0];
        let t = FatTable::from_bytes(&raw, FatType::Fat16, 4);
        assert_eq!(t.chain(2), vec![2, 3]);
        assert!(t.chain(0).is_empty());
    }
}
