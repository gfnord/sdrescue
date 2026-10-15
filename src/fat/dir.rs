//! FAT directory entries: 8.3 short entries, long file name (LFN) entries,
//! the LFN checksum and recovery of the first character of deleted names.

use std::fmt;

/// Size of one directory entry.
pub const ENTRY_SIZE: usize = 32;
/// Marker written over the first name byte of a deleted entry.
pub const DELETED: u8 = 0xE5;
/// Attribute value of an LFN entry.
pub const ATTR_LFN: u8 = 0x0F;
/// Directory attribute bit.
pub const ATTR_DIR: u8 = 0x10;
/// Volume label attribute bit.
pub const ATTR_VOLUME: u8 = 0x08;

/// A FAT timestamp (local time, 2-second resolution).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FatDateTime {
    /// Year (1980..=2107).
    pub year: u16,
    /// Month 1..=12.
    pub month: u8,
    /// Day 1..=31.
    pub day: u8,
    /// Hour 0..=23.
    pub hour: u8,
    /// Minute 0..=59.
    pub minute: u8,
    /// Second 0..=58.
    pub second: u8,
}

impl FatDateTime {
    /// Decodes the packed FAT date and time words. `None` when invalid.
    pub fn decode(date: u16, time: u16) -> Option<FatDateTime> {
        let dt = FatDateTime {
            year: 1980 + (date >> 9),
            month: ((date >> 5) & 0x0F) as u8,
            day: (date & 0x1F) as u8,
            hour: (time >> 11) as u8,
            minute: ((time >> 5) & 0x3F) as u8,
            second: ((time & 0x1F) * 2) as u8,
        };
        let valid = (1..=12).contains(&dt.month)
            && (1..=31).contains(&dt.day)
            && dt.hour < 24
            && dt.minute < 60
            && dt.second < 60;
        valid.then_some(dt)
    }

    /// Converts to Unix seconds, interpreting the value as local time.
    pub fn to_unix(&self) -> Option<i64> {
        crate::device::local_to_unix(
            self.year,
            u16::from(self.month),
            u16::from(self.day),
            u16::from(self.hour),
            u16::from(self.minute),
            u16::from(self.second),
        )
    }
}

impl fmt::Display for FatDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// A parsed 32-byte directory slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawEntry {
    /// First byte 0x00: no more entries in this directory.
    End,
    /// A long-file-name fragment.
    Lfn(LfnPart),
    /// A short (8.3) entry: file, directory or volume label.
    Short(ShortEntry),
    /// A slot that is neither (garbage); skipped.
    Invalid,
}

/// One LFN fragment (13 UTF-16 code units).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfnPart {
    /// Sequence byte (0xE5 when deleted).
    pub seq: u8,
    /// Checksum of the short name this fragment belongs to.
    pub checksum: u8,
    /// The 13 UTF-16 code units at offsets 1, 14 and 28.
    pub units: [u16; 13],
    /// Whether the fragment is marked deleted.
    pub deleted: bool,
}

/// A short (8.3) directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortEntry {
    /// The 11 raw name bytes (byte 0 may be 0xE5 if deleted).
    pub name: [u8; 11],
    /// Attribute byte.
    pub attr: u8,
    /// NT reserved byte: bit 3 = lower-case base, bit 4 = lower-case extension.
    pub nt_case: u8,
    /// First cluster (high word at offset 20, low word at offset 26).
    pub cluster: u32,
    /// File size in bytes.
    pub size: u32,
    /// Last-modified timestamp.
    pub modified: Option<FatDateTime>,
    /// First name byte was 0xE5.
    pub deleted: bool,
}

impl ShortEntry {
    /// Directory?
    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIR != 0
    }

    /// Volume label?
    pub fn is_volume_label(&self) -> bool {
        self.attr & ATTR_VOLUME != 0 && self.attr & ATTR_DIR == 0
    }

    /// `.` or `..`?
    pub fn is_dot(&self) -> bool {
        self.name[0] == b'.'
            && (self.name[1] == b' ' || (self.name[1] == b'.' && self.name[2] == b' '))
    }
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

/// Characters allowed in 8.3 names besides A-Z and 0-9 (and bytes >= 0x80).
const SHORT_SPECIALS: &[u8] = b"!#$%&'()-@^_`{}~ ";

/// Whether `b` may appear in a short name.
pub fn is_valid_short_char(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit() || b >= 0x80 || SHORT_SPECIALS.contains(&b)
}

/// Parses one 32-byte directory slot.
pub fn parse_entry(e: &[u8]) -> RawEntry {
    if e.len() < ENTRY_SIZE {
        return RawEntry::Invalid;
    }
    if e[0] == 0x00 {
        return RawEntry::End;
    }
    let attr = e[11];
    if attr & 0x3F == ATTR_LFN {
        // LFN fragments have type 0 at offset 12 and cluster 0 at offset 26.
        if e[12] != 0 || le16(e, 26) != 0 {
            return RawEntry::Invalid;
        }
        let mut units = [0u16; 13];
        let offsets = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        for (u, &o) in units.iter_mut().zip(offsets.iter()) {
            *u = le16(e, o);
        }
        return RawEntry::Lfn(LfnPart {
            seq: e[0],
            checksum: e[13],
            units,
            deleted: e[0] == DELETED,
        });
    }
    if attr & 0xC0 != 0 {
        return RawEntry::Invalid;
    }
    let deleted = e[0] == DELETED;
    let mut name = [0u8; 11];
    name.copy_from_slice(&e[0..11]);
    let is_label = attr & ATTR_VOLUME != 0 && attr & ATTR_DIR == 0;
    let dot = name[0] == b'.';
    // Garbage check: every name byte after the first must be a legal character
    // (volume labels allow lower case; `.`/`..` have dots).
    if !is_label && !dot {
        if name[1..].iter().any(|&b| !is_valid_short_char(b)) {
            return RawEntry::Invalid;
        }
        if !deleted && name[0] != 0x05 && !is_valid_short_char(name[0]) {
            return RawEntry::Invalid;
        }
        if name[0] == b' ' {
            return RawEntry::Invalid;
        }
    }
    let cluster = (u32::from(le16(e, 20)) << 16) | u32::from(le16(e, 26));
    let size = u32::from_le_bytes([e[28], e[29], e[30], e[31]]);
    RawEntry::Short(ShortEntry {
        name,
        attr,
        nt_case: e[12],
        cluster,
        size,
        modified: FatDateTime::decode(le16(e, 24), le16(e, 22)),
        deleted,
    })
}

/// The LFN checksum of an 11-byte short name.
pub fn lfn_checksum(name: &[u8; 11]) -> u8 {
    name.iter().fold(0u8, |sum, &b| {
        ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(b)
    })
}

/// Finds the first byte of a deleted short name whose LFN checksum is `checksum`.
///
/// Common characters (A-Z, 0-9, specials) are tried first, then the remaining
/// valid bytes. Because the checksum is a bijection of the first byte, at most
/// one byte matches.
pub fn recover_first_char(name: &[u8; 11], checksum: u8) -> Option<u8> {
    let mut candidate = *name;
    let ordered = (b'A'..=b'Z')
        .chain(b'0'..=b'9')
        .chain(SHORT_SPECIALS.iter().copied().filter(|&c| c != b' '))
        .chain(0x80..=0xFF);
    for c in ordered {
        if c == DELETED {
            // 0xE5 is stored as 0x05; try that spelling instead.
            candidate[0] = 0x05;
        } else {
            candidate[0] = c;
        }
        if lfn_checksum(&candidate) == checksum {
            return Some(if c == DELETED { DELETED } else { c });
        }
    }
    None
}

/// Decodes LFN fragments ordered from sequence 1 upwards into a string.
/// Stops at 0x0000; 0xFFFF padding is skipped.
pub fn decode_lfn(parts_in_order: &[&LfnPart]) -> Option<String> {
    let mut units = Vec::with_capacity(parts_in_order.len() * 13);
    'outer: for p in parts_in_order {
        for &u in &p.units {
            if u == 0x0000 {
                break 'outer;
            }
            if u == 0xFFFF {
                continue;
            }
            units.push(u);
        }
    }
    if units.is_empty() {
        return None;
    }
    let s = String::from_utf16_lossy(&units);
    // A name made only of replacement characters/controls is garbage.
    if s.chars().all(|c| c == '\u{FFFD}' || c.is_control()) {
        return None;
    }
    Some(s)
}

/// Converts a short-name byte to a char (bytes >= 0x80 use code page 437).
fn short_byte_to_char(b: u8) -> char {
    const CP437_HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";
    if b < 0x80 {
        b as char
    } else {
        CP437_HIGH.chars().nth(usize::from(b - 0x80)).unwrap_or('_')
    }
}

/// Formats an 11-byte short name as `NAME.EXT`, honouring the NT lower-case flags.
pub fn format_short_name(name: &[u8; 11], nt_case: u8) -> String {
    let mut bytes = *name;
    if bytes[0] == 0x05 {
        bytes[0] = DELETED;
    }
    let conv = |s: &[u8], lower: bool| -> String {
        let t: String = s.iter().map(|&b| short_byte_to_char(b)).collect();
        let t = t.trim_end().to_string();
        if lower { t.to_lowercase() } else { t }
    };
    let base = conv(&bytes[0..8], nt_case & 0x08 != 0);
    let ext = conv(&bytes[8..11], nt_case & 0x10 != 0);
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(name: &[u8; 11]) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(name);
        e[11] = 0x20;
        e
    }

    fn lfn_slot(seq: u8, chk: u8, text: &[u16]) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[0] = seq;
        e[11] = ATTR_LFN;
        e[13] = chk;
        let offsets = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        for (i, &o) in offsets.iter().enumerate() {
            let u = text.get(i).copied().unwrap_or(0xFFFF);
            e[o..o + 2].copy_from_slice(&u.to_le_bytes());
        }
        e
    }

    #[test]
    fn checksum_matches_reference() {
        // Reference value computed with the algorithm from the FAT specification.
        let name = *b"IMG_00~1JPG";
        let mut sum: u8 = 0;
        for &b in &name {
            sum = (if sum & 1 != 0 { 0x80u8 } else { 0 })
                .wrapping_add(sum >> 1)
                .wrapping_add(b);
        }
        assert_eq!(lfn_checksum(&name), sum);
    }

    #[test]
    fn recovers_first_char() {
        let original = *b"VACATI~1JPG";
        let chk = lfn_checksum(&original);
        let mut deleted = original;
        deleted[0] = DELETED;
        assert_eq!(recover_first_char(&deleted, chk), Some(b'V'));
        let original = *b"_DSC0001NEF";
        let chk = lfn_checksum(&original);
        let mut d2 = original;
        d2[0] = DELETED;
        assert_eq!(recover_first_char(&d2, chk), Some(b'_'));
    }

    #[test]
    fn decodes_lfn_across_parts() {
        let name: Vec<u16> = "A very long photo name.jpeg".encode_utf16().collect();
        let p1 = match parse_entry(&lfn_slot(0x01, 7, &name[..13])) {
            RawEntry::Lfn(p) => p,
            other => panic!("{other:?}"),
        };
        let p2 = match parse_entry(&lfn_slot(0x02, 7, &name[13..26])) {
            RawEntry::Lfn(p) => p,
            other => panic!("{other:?}"),
        };
        let mut tail = name[26..].to_vec();
        tail.push(0);
        let p3 = match parse_entry(&lfn_slot(0x43, 7, &tail)) {
            RawEntry::Lfn(p) => p,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            decode_lfn(&[&p1, &p2, &p3]).as_deref(),
            Some("A very long photo name.jpeg")
        );
        // Deleted fragment keeps its text.
        let d = match parse_entry(&lfn_slot(DELETED, 7, &name[..13])) {
            RawEntry::Lfn(p) => p,
            other => panic!("{other:?}"),
        };
        assert!(d.deleted);
        assert_eq!(decode_lfn(&[&d]).as_deref(), Some("A very long p"));
    }

    #[test]
    fn parses_short_entries() {
        let mut e = short(b"\xE5MG_0001JPG");
        e[20..22].copy_from_slice(&1u16.to_le_bytes());
        e[26..28].copy_from_slice(&5u16.to_le_bytes());
        e[28..32].copy_from_slice(&1234u32.to_le_bytes());
        // 2023-07-14 10:30:20
        let date: u16 = ((2023 - 1980) << 9) | (7 << 5) | 14;
        let time: u16 = (10 << 11) | (30 << 5) | 10;
        e[24..26].copy_from_slice(&date.to_le_bytes());
        e[22..24].copy_from_slice(&time.to_le_bytes());
        let RawEntry::Short(s) = parse_entry(&e) else {
            panic!()
        };
        assert!(s.deleted);
        assert_eq!(s.cluster, 0x0001_0005);
        assert_eq!(s.size, 1234);
        assert_eq!(s.modified.unwrap().to_string(), "2023-07-14 10:30:20");
        assert_eq!(parse_entry(&[0u8; 32]), RawEntry::End);
        let mut junk = short(b"AB\x01DEFGHTXT");
        junk[11] = 0x20;
        assert_eq!(parse_entry(&junk), RawEntry::Invalid);
    }

    #[test]
    fn formats_short_names() {
        assert_eq!(format_short_name(b"README  TXT", 0), "README.TXT");
        assert_eq!(format_short_name(b"README  TXT", 0x18), "readme.txt");
        assert_eq!(format_short_name(b"NOEXT      ", 0), "NOEXT");
    }

    #[test]
    fn rejects_invalid_dates() {
        assert!(FatDateTime::decode(0, 0).is_none());
        assert!(FatDateTime::decode((1 << 5) | 1, 24 << 11).is_none());
    }
}
