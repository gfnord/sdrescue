//! Raw device handling and the "never work on the card" safety rules.
//!
//! Recognising device names is platform-independent (so the safety checks can
//! be tested anywhere); opening devices, listing disks and comparing volumes
//! is delegated to `linux.rs` / `windows.rs`.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::source::{ImageFile, Source, file_read_at};

#[cfg(unix)]
mod linux;
#[cfg(unix)]
use linux as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

pub use platform::{free_space, local_to_unix, print_disks};

/// A source name that refers to a device rather than an image file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSpec {
    /// A Windows volume: `F:`, `F:\`, `\\.\F:`.
    WinVolume(char),
    /// A Windows physical disk: `\\.\PhysicalDrive2`.
    WinPhysical(u32),
    /// Any other Windows device namespace path (`\\.\...`, `\\?\Volume{...}`).
    WinOther(String),
    /// A Unix device node such as `/dev/sdb` or `/dev/mmcblk0`.
    Unix(PathBuf),
}

impl DeviceSpec {
    /// Recognises device names. Returns `None` for ordinary (image) paths.
    pub fn parse(spec: &str) -> Option<DeviceSpec> {
        let s = spec.trim();
        if let Some(letter) = drive_letter_only(s) {
            return Some(DeviceSpec::WinVolume(letter));
        }
        let lower = s.to_ascii_lowercase();
        for prefix in [r"\\.\", r"\\?\", "//./", "//?/"] {
            if let Some(rest) = lower.strip_prefix(prefix) {
                let orig_rest = &s[prefix.len()..];
                if let Some(letter) = drive_letter_only(orig_rest) {
                    return Some(DeviceSpec::WinVolume(letter));
                }
                if let Some(num) = rest.strip_prefix("physicaldrive")
                    && let Ok(n) = num.parse::<u32>()
                {
                    return Some(DeviceSpec::WinPhysical(n));
                }
                // `\\?\C:\Users\...` is a long *file* path, not a device.
                if orig_rest.len() > 3 && drive_letter_only(&orig_rest[..2]).is_some() {
                    return None;
                }
                return Some(DeviceSpec::WinOther(s.to_string()));
            }
        }
        if s.starts_with("/dev/") {
            return Some(DeviceSpec::Unix(PathBuf::from(s)));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            if let Ok(meta) = std::fs::metadata(s) {
                let ft = meta.file_type();
                if ft.is_block_device() || ft.is_char_device() {
                    return Some(DeviceSpec::Unix(PathBuf::from(s)));
                }
            }
        }
        None
    }

    /// The path to pass to the operating system when opening the device.
    pub fn os_path(&self) -> String {
        match self {
            DeviceSpec::WinVolume(l) => format!(r"\\.\{l}:"),
            DeviceSpec::WinPhysical(n) => format!(r"\\.\PhysicalDrive{n}"),
            DeviceSpec::WinOther(s) => s.clone(),
            DeviceSpec::Unix(p) => p.display().to_string(),
        }
    }
}

/// `F:` or `F:\` (a bare drive letter) → `Some('F')`.
fn drive_letter_only(s: &str) -> Option<char> {
    let b = s.as_bytes();
    let ok = match b.len() {
        2 => b[1] == b':',
        3 => b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'),
        _ => false,
    };
    (ok && b[0].is_ascii_alphabetic()).then(|| b[0].to_ascii_uppercase() as char)
}

/// Builds the error shown when an analysis command is pointed at a device.
pub fn refusal_message(spec: &str, command: &str) -> String {
    let image_src = match DeviceSpec::parse(spec) {
        Some(DeviceSpec::WinVolume(l)) => format!(r"\\.\{l}:"),
        _ => spec.to_string(),
    };
    let physical_tip = match DeviceSpec::parse(spec) {
        Some(DeviceSpec::WinVolume(_)) => {
            "\nTip: imaging the whole card (\\\\.\\PhysicalDriveN, see `sdrescue disks`) is even better,\n\
             because it also captures the partition table and any data outside the partition.\n"
        }
        _ => "",
    };
    format!(
        "'{spec}' is a device (a real card or disk), not an image file.\n\n\
         Working directly on the card is risky: every extra read stresses a card that may be\n\
         failing, and any mistake (or Windows deciding to \"repair\" it) can destroy the files\n\
         you want back. Make a byte-for-byte image first and work on the copy instead:\n\n    \
         sdrescue image {image_src} card.img\n    \
         sdrescue {command} card.img\n{physical_tip}\n\
         If you really want to run directly on the device, add --force."
    )
}

/// Opens `spec` for an analysis command (`info`, `list`, `undelete`, `carve`).
///
/// Devices are refused unless `force` is set.
pub fn open_for_analysis(spec: &str, force: bool, command: &str) -> Result<Box<dyn Source>> {
    match DeviceSpec::parse(spec) {
        Some(dev) => {
            if !force {
                bail!("{}", refusal_message(spec, command));
            }
            open_device(&dev)
        }
        None => Ok(Box::new(ImageFile::open(Path::new(spec))?)),
    }
}

/// Opens a source for imaging: devices are allowed without `--force`.
pub fn open_for_imaging(spec: &str) -> Result<Box<dyn Source>> {
    match DeviceSpec::parse(spec) {
        Some(dev) => open_device(&dev),
        None => Ok(Box::new(ImageFile::open(Path::new(spec))?)),
    }
}

/// Opens a device read-only.
pub fn open_device(dev: &DeviceSpec) -> Result<Box<dyn Source>> {
    platform::open_device(dev)
}

/// Refuses outputs that would land on the source device (or on the source image itself).
pub fn ensure_output_not_on_source(source: &str, out: &Path) -> Result<()> {
    let explain = |what: &str| -> Result<()> {
        bail!(
            "refusing to write {} onto the source '{source}'.\n\
             Writing to the card you are recovering from can overwrite the very files you want\n\
             back. Choose an output location on another drive (for example your PC's C: drive).",
            what
        )
    };
    let Some(dev) = DeviceSpec::parse(source) else {
        // Source is an image file: only refuse writing over the image itself.
        if let (Ok(a), Ok(b)) = (std::fs::canonicalize(source), std::fs::canonicalize(out))
            && a == b
        {
            return explain("the output");
        }
        return Ok(());
    };
    if lexically_on_device(&dev, out) || platform::output_on_device(&dev, out)? {
        return explain("output");
    }
    Ok(())
}

/// Platform-independent check based on the spelling of the paths.
fn lexically_on_device(dev: &DeviceSpec, out: &Path) -> bool {
    let s = out.to_string_lossy();
    match dev {
        DeviceSpec::WinVolume(letter) => {
            let mut rest: &str = &s;
            for prefix in [r"\\?\", r"\\.\", "//?/", "//./"] {
                if let Some(r) = rest.strip_prefix(prefix) {
                    rest = r;
                    break;
                }
            }
            let b = rest.as_bytes();
            b.len() >= 2 && b[1] == b':' && b[0].eq_ignore_ascii_case(&(*letter as u8))
        }
        DeviceSpec::WinPhysical(_) | DeviceSpec::WinOther(_) => DeviceSpec::parse(&s).is_some(),
        DeviceSpec::Unix(p) => out.starts_with(p),
    }
}

/// Nearest existing ancestor of `path` (made absolute), used to find which
/// filesystem an output that does not exist yet would end up on.
pub(crate) fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    let abs = std::path::absolute(path).ok()?;
    let mut cur: &Path = &abs;
    loop {
        if cur.exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

/// A read-only raw device. Reads are always issued at sector-aligned offsets
/// with sector-multiple lengths, as Windows requires for raw volumes and disks.
pub(crate) struct RawDevice {
    file: File,
    size: u64,
    sector: u32,
    name: String,
}

impl RawDevice {
    pub(crate) fn new(file: File, size: u64, sector: u32, name: String) -> Self {
        let sector = if sector.is_power_of_two() && sector >= 512 {
            sector
        } else {
            512
        };
        Self {
            file,
            size,
            sector,
            name,
        }
    }
}

impl Source for RawDevice {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.sector
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let want = (self.size - offset).min(buf.len() as u64) as usize;
        let ss = u64::from(self.sector);
        if offset.is_multiple_of(ss) && (want as u64).is_multiple_of(ss) {
            return file_read_at(&self.file, offset, &mut buf[..want]);
        }
        let start = offset / ss * ss;
        let end = (offset + want as u64).div_ceil(ss) * ss;
        let mut bounce = vec![0u8; (end - start) as usize];
        let got = file_read_at(&self.file, start, &mut bounce)?;
        let skip = (offset - start) as usize;
        let n = got.saturating_sub(skip).min(want);
        buf[..n].copy_from_slice(&bounce[skip..skip + n]);
        Ok(n)
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_names() {
        assert_eq!(DeviceSpec::parse("F:"), Some(DeviceSpec::WinVolume('F')));
        assert_eq!(DeviceSpec::parse("f:\\"), Some(DeviceSpec::WinVolume('F')));
        assert_eq!(
            DeviceSpec::parse(r"\\.\F:"),
            Some(DeviceSpec::WinVolume('F'))
        );
        assert_eq!(
            DeviceSpec::parse(r"\\.\PhysicalDrive2"),
            Some(DeviceSpec::WinPhysical(2))
        );
        assert_eq!(
            DeviceSpec::parse(r"\\.\physicaldrive10"),
            Some(DeviceSpec::WinPhysical(10))
        );
        assert!(matches!(
            DeviceSpec::parse(r"\\?\Volume{1234}"),
            Some(DeviceSpec::WinOther(_))
        ));
        assert_eq!(
            DeviceSpec::parse("/dev/mmcblk0"),
            Some(DeviceSpec::Unix("/dev/mmcblk0".into()))
        );
        assert_eq!(DeviceSpec::parse("card.img"), None);
        assert_eq!(DeviceSpec::parse(r"C:\cards\card.img"), None);
        assert_eq!(DeviceSpec::parse(r"\\?\C:\cards\card.img"), None);
        assert_eq!(DeviceSpec::parse("/home/u/card.img"), None);
    }

    #[test]
    fn refuses_device_without_force() {
        let err = open_for_analysis("F:", false, "list").err().unwrap();
        let msg = err.to_string();
        assert!(msg.contains("--force"));
        assert!(msg.contains(r"sdrescue image \\.\F: card.img"));
        let err = open_for_analysis("/dev/sdz", false, "carve").err().unwrap();
        assert!(err.to_string().contains("sdrescue image /dev/sdz card.img"));
    }

    #[test]
    fn refuses_output_on_source_lexically() {
        assert!(ensure_output_not_on_source("F:", Path::new(r"F:\recovered")).is_err());
        assert!(ensure_output_not_on_source(r"\\.\f:", Path::new(r"f:\card.img")).is_err());
        assert!(ensure_output_not_on_source("F:", Path::new(r"\\?\F:\x")).is_err());
        assert!(ensure_output_not_on_source("/dev/sdz", Path::new("/dev/sdz")).is_err());
        assert!(ensure_output_not_on_source("F:", Path::new(r"C:\recovered")).is_ok());
    }

    #[test]
    fn raw_device_aligns_reads() {
        let dir = std::env::temp_dir().join(format!("sdrescue-raw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("dev.bin");
        let data: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let dev = RawDevice::new(File::open(&p).unwrap(), 4096, 512, "t".into());
        let mut b = [0u8; 700];
        assert_eq!(dev.read_at(100, &mut b).unwrap(), 700);
        assert_eq!(&b[..], &data[100..800]);
        assert_eq!(dev.read_at(4000, &mut b).unwrap(), 96);
        assert_eq!(&b[..96], &data[4000..]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
