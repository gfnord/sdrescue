//! Linux (and other Unix) device access: block devices under `/dev`,
//! disk listing from `/sys/block`, and device-ID comparisons.

use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{DeviceSpec, RawDevice, existing_ancestor};
use crate::source::Source;
use crate::util::human_size;

/// Opens a block device read-only (no write access is requested).
pub fn open_device(dev: &DeviceSpec) -> Result<Box<dyn Source>> {
    let DeviceSpec::Unix(path) = dev else {
        bail!(
            "'{}' is a Windows device name; on Linux use a path such as /dev/sdb or /dev/mmcblk0",
            dev.os_path()
        );
    };
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => bail!(
            "permission denied opening {}.\nReading a raw device needs root: run the command again with sudo.",
            path.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("cannot open {}", path.display())),
    };
    let size = file
        .seek(SeekFrom::End(0))
        .with_context(|| format!("cannot determine the size of {}", path.display()))?;
    let sector = sysfs_dir(path)
        .and_then(|d| logical_block_size(&d))
        .unwrap_or(512);
    Ok(Box::new(RawDevice::new(
        file,
        size,
        sector,
        path.display().to_string(),
    )))
}

/// `/sys/dev/block/MAJ:MIN` resolved to its canonical sysfs directory.
fn sysfs_dir_for_rdev(rdev: u64) -> Option<PathBuf> {
    let (maj, min) = (libc::major(rdev), libc::minor(rdev));
    fs::canonicalize(format!("/sys/dev/block/{maj}:{min}")).ok()
}

fn sysfs_dir(dev_path: &Path) -> Option<PathBuf> {
    let meta = fs::metadata(dev_path).ok()?;
    if !meta.file_type().is_block_device() {
        return None;
    }
    sysfs_dir_for_rdev(meta.rdev())
}

fn logical_block_size(sys: &Path) -> Option<u32> {
    // Partitions have no `queue` directory of their own; use the parent disk's.
    for dir in [sys.to_path_buf(), sys.parent()?.to_path_buf()] {
        if let Ok(s) = fs::read_to_string(dir.join("queue/logical_block_size")) {
            return s.trim().parse().ok();
        }
    }
    None
}

/// True when `out` would be written to a filesystem that lives on `dev`
/// (the same block device, or one of its partitions).
pub fn output_on_device(dev: &DeviceSpec, out: &Path) -> Result<bool> {
    let DeviceSpec::Unix(dev_path) = dev else {
        return Ok(false);
    };
    // Writing the image straight to the device node itself.
    if let (Ok(a), Ok(b)) = (fs::canonicalize(dev_path), fs::canonicalize(out))
        && a == b
    {
        return Ok(true);
    }
    let Some(src_sys) = sysfs_dir(dev_path) else {
        return Ok(false);
    };
    let Some(anc) = existing_ancestor(out) else {
        return Ok(false);
    };
    let meta = fs::metadata(&anc)?;
    if meta.file_type().is_block_device()
        && sysfs_dir_for_rdev(meta.rdev()).as_ref() == Some(&src_sys)
    {
        return Ok(true);
    }
    let Some(out_sys) = sysfs_dir_for_rdev(meta.dev()) else {
        return Ok(false);
    };
    // Same device, or a partition of the source disk, or the disk that holds
    // the source partition.
    Ok(out_sys.starts_with(&src_sys) || src_sys.starts_with(&out_sys))
}

/// Free bytes available to the user on the filesystem holding `path`.
pub fn free_space(path: &Path) -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let anc = existing_ancestor(path).context("cannot resolve output folder")?;
    let c = CString::new(anc.as_os_str().as_bytes())?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a valid out-pointer.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(io::Error::last_os_error()).context("statvfs failed");
    }
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn mounts() -> Vec<(String, String)> {
    let Ok(text) = fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), it.next()?.replace("\\040", " ")))
        })
        .collect()
}

/// Prints block devices from `/sys/block`. Returns how many were listed.
pub fn print_disks(verbose: bool) -> Result<usize> {
    let mut names: Vec<String> = fs::read_dir("/sys/block")
        .context("cannot read /sys/block")?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| verbose || !(n.starts_with("ram") || n.starts_with("zram")))
        .collect();
    names.sort();
    let mounts = mounts();
    println!(
        "{:<16} {:>10} {:<10} {:<28} MOUNTED AT",
        "DEVICE", "SIZE", "REMOVABLE", "MODEL"
    );
    let mut count = 0;
    for name in names {
        let sys = Path::new("/sys/block").join(&name);
        let sectors: u64 = read_trim(&sys.join("size"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if sectors == 0 && !verbose {
            continue;
        }
        let removable = read_trim(&sys.join("removable")).as_deref() == Some("1");
        let model = [
            read_trim(&sys.join("device/vendor")),
            read_trim(&sys.join("device/model")),
            read_trim(&sys.join("device/name")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
        let dev = format!("/dev/{name}");
        let mnt = |d: &str| {
            mounts
                .iter()
                .filter(|(src, _)| src == d)
                .map(|(_, m)| m.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!(
            "{:<16} {:>10} {:<10} {:<28} {}",
            dev,
            human_size(sectors * 512),
            if removable { "yes" } else { "no" },
            model,
            mnt(&dev)
        );
        count += 1;
        let mut parts: Vec<String> = fs::read_dir(&sys)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().join("partition").exists())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        parts.sort();
        for p in parts {
            let psz: u64 = read_trim(&sys.join(&p).join("size"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let pdev = format!("/dev/{p}");
            println!(
                "  {:<14} {:>10} {:<10} {:<28} {}",
                pdev,
                human_size(psz * 512),
                "",
                "",
                mnt(&pdev)
            );
        }
    }
    println!();
    println!("Image the whole card (e.g. /dev/sdb or /dev/mmcblk0, not a partition) with:");
    println!("    sudo sdrescue image /dev/sdX card.img");
    Ok(count)
}

/// Converts a local wall-clock time (as stored by FAT) to Unix seconds using
/// the system time zone (`TZ` / `/etc/localtime`).
pub fn local_to_unix(y: u16, mo: u16, d: u16, h: u16, mi: u16, s: u16) -> Option<i64> {
    // SAFETY: a zeroed `tm` is valid; mktime only reads/normalises it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = i32::from(y) - 1900;
    tm.tm_mon = i32::from(mo) - 1;
    tm.tm_mday = i32::from(d);
    tm.tm_hour = i32::from(h);
    tm.tm_min = i32::from(mi);
    tm.tm_sec = i32::from(s);
    tm.tm_isdst = -1;
    // SAFETY: `tm` is a valid, initialised struct.
    let t = unsafe { libc::mktime(&mut tm) };
    (t != -1).then_some(t as i64)
}
