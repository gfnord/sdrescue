//! Windows raw device access through `CreateFileW` / `DeviceIoControl`.
//!
//! Devices are opened with `GENERIC_READ` only and shared for read and write,
//! so Windows keeps working normally and nothing is ever written to the card.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::ptr::{null, null_mut};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GENERIC_READ,
    INVALID_HANDLE_VALUE, SYSTEMTIME,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDiskFreeSpaceExW, GetDriveTypeW,
    GetLogicalDrives, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DISK_GEOMETRY_EX, FSCTL_ALLOW_EXTENDED_DASD_IO, GET_LENGTH_INFORMATION,
    IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_GET_DEVICE_NUMBER,
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
};
use windows_sys::Win32::System::Time::TzSpecificLocalTimeToSystemTime;

use super::{DeviceSpec, RawDevice, existing_ancestor};
use crate::source::Source;
use crate::util::{days_from_civil, human_size};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Opens a device path with the given access mask (0 = query-only, no admin needed).
fn open_raw(path: &str, access: u32) -> io::Result<File> {
    let w = wide(path);
    // SAFETY: `w` is NUL-terminated; all other pointers are null/optional.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `h` is a valid, owned handle; `File` closes it on drop.
    Ok(unsafe { File::from_raw_handle(h) })
}

/// Issues an IOCTL and returns the raw output bytes.
fn ioctl(file: &File, code: u32, input: &[u8], out_len: usize) -> io::Result<Vec<u8>> {
    use std::os::windows::io::AsRawHandle;
    let mut out = vec![0u8; out_len];
    let mut returned = 0u32;
    // SAFETY: buffers are valid for the given lengths; the handle is open.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            if input.is_empty() {
                null()
            } else {
                input.as_ptr().cast::<c_void>()
            },
            input.len() as u32,
            out.as_mut_ptr().cast::<c_void>(),
            out.len() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    out.truncate(returned as usize);
    Ok(out)
}

/// Reads a `T` from the start of an IOCTL output buffer.
fn read_struct<T: Copy>(buf: &[u8]) -> Option<T> {
    (buf.len() >= std::mem::size_of::<T>())
        // SAFETY: length checked; `read_unaligned` tolerates any alignment.
        .then(|| unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<T>()) })
}

fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: plain-old-data FFI struct viewed as bytes for the IOCTL input.
    unsafe { std::slice::from_raw_parts((v as *const T).cast::<u8>(), std::mem::size_of::<T>()) }
}

fn disk_length(file: &File) -> Option<u64> {
    let out = ioctl(
        file,
        IOCTL_DISK_GET_LENGTH_INFO,
        &[],
        std::mem::size_of::<GET_LENGTH_INFORMATION>(),
    )
    .ok()?;
    read_struct::<GET_LENGTH_INFORMATION>(&out).map(|g| g.Length as u64)
}

fn disk_geometry(file: &File) -> Option<DISK_GEOMETRY_EX> {
    let out = ioctl(file, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], 256).ok()?;
    read_struct::<DISK_GEOMETRY_EX>(&out)
}

fn device_number(file: &File) -> Option<u32> {
    let out = ioctl(
        file,
        IOCTL_STORAGE_GET_DEVICE_NUMBER,
        &[],
        std::mem::size_of::<STORAGE_DEVICE_NUMBER>(),
    )
    .ok()?;
    read_struct::<STORAGE_DEVICE_NUMBER>(&out).map(|n| n.DeviceNumber)
}

/// Physical disk numbers backing a volume (usually one).
fn volume_disks(volume_device: &str) -> Vec<u32> {
    let Ok(f) = open_raw(volume_device, 0) else {
        return Vec::new();
    };
    let Ok(out) = ioctl(&f, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], 8 + 24 * 16) else {
        return Vec::new();
    };
    if out.len() < 8 {
        return Vec::new();
    }
    let count = u32::from_le_bytes([out[0], out[1], out[2], out[3]]) as usize;
    // DISK_EXTENT is { u32 DiskNumber; i64 StartingOffset; i64 ExtentLength } with 8-byte alignment.
    (0..count)
        .filter_map(|i| {
            let o = 8 + i * 24;
            out.get(o..o + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        })
        .collect()
}

fn access_denied_message(path: &str) -> String {
    format!(
        "access denied opening {path}.\n\
         Reading a raw card needs administrator rights. Close this window, then open the\n\
         Start menu, right-click \"Terminal\" (or \"Command Prompt\"/\"PowerShell\") and choose\n\
         \"Run as administrator\", and run the command again."
    )
}

/// Opens a volume or physical disk read-only.
pub fn open_device(dev: &DeviceSpec) -> Result<Box<dyn Source>> {
    if let DeviceSpec::Unix(p) = dev {
        bail!(
            "'{}' is a Linux device path; on Windows use F:, \\\\.\\F: or \\\\.\\PhysicalDriveN",
            p.display()
        );
    }
    let path = dev.os_path();
    let file = match open_raw(&path, GENERIC_READ) {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            bail!("{}", access_denied_message(&path))
        }
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(c) if c == ERROR_FILE_NOT_FOUND as i32 || c == ERROR_PATH_NOT_FOUND as i32
            ) =>
        {
            bail!("{path} does not exist. Run `sdrescue disks` to see the available drives.")
        }
        Err(e) => return Err(e).with_context(|| format!("cannot open {path}")),
    };
    if matches!(dev, DeviceSpec::WinVolume(_)) {
        // Lets us read the whole volume, including sectors past the end of the
        // filesystem. It only changes how reads are checked; nothing is written.
        let _ = ioctl(&file, FSCTL_ALLOW_EXTENDED_DASD_IO, &[], 0);
    }
    let geometry = disk_geometry(&file);
    let size = disk_length(&file)
        .or_else(|| geometry.map(|g| g.DiskSize as u64))
        .with_context(|| format!("cannot determine the size of {path}"))?;
    let sector = geometry.map(|g| g.Geometry.BytesPerSector).unwrap_or(512);
    Ok(Box::new(RawDevice::new(file, size, sector, path)))
}

/// Root of the volume holding `path` (e.g. `C:\`).
fn volume_root(path: &Path) -> Option<String> {
    let anc = existing_ancestor(path)?;
    let w = wide(&anc.to_string_lossy());
    let mut buf = [0u16; 1024];
    // SAFETY: valid input string and output buffer with its length.
    let ok = unsafe { GetVolumePathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    (ok != 0).then(|| from_wide(&buf))
}

/// `\\?\Volume{GUID}` (no trailing backslash) for a volume root such as `C:\`.
fn volume_guid_path(root: &str) -> Option<String> {
    let w = wide(root);
    let mut buf = [0u16; 128];
    // SAFETY: valid input string and output buffer with its length.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
    };
    (ok != 0).then(|| from_wide(&buf).trim_end_matches('\\').to_string())
}

struct VolumeInfo {
    label: String,
    fs: String,
    serial: u32,
}

fn volume_info(root: &str) -> Option<VolumeInfo> {
    let w = wide(root);
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    let mut serial = 0u32;
    let mut maxlen = 0u32;
    let mut flags = 0u32;
    // SAFETY: all buffers are valid for their declared lengths.
    let ok = unsafe {
        GetVolumeInformationW(
            w.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            &mut serial,
            &mut maxlen,
            &mut flags,
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    (ok != 0).then(|| VolumeInfo {
        label: from_wide(&label),
        fs: from_wide(&fs),
        serial,
    })
}

/// True when `out` would be written onto the device being read.
pub fn output_on_device(dev: &DeviceSpec, out: &Path) -> Result<bool> {
    let Some(out_root) = volume_root(out) else {
        return Ok(false);
    };
    let out_dev = volume_guid_path(&out_root)
        .unwrap_or_else(|| format!(r"\\.\{}", out_root.trim_end_matches('\\')));
    let out_disks = volume_disks(&out_dev);
    match dev {
        DeviceSpec::WinVolume(letter) => {
            let src_root = format!("{letter}:\\");
            if out_root.eq_ignore_ascii_case(&src_root) {
                return Ok(true);
            }
            if let (Some(a), Some(b)) = (volume_info(&src_root), volume_info(&out_root))
                && a.serial == b.serial
                && a.serial != 0
            {
                return Ok(true);
            }
            let src_disks = volume_disks(&format!(r"\\.\{letter}:"));
            Ok(src_disks.iter().any(|d| out_disks.contains(d)))
        }
        DeviceSpec::WinPhysical(n) => Ok(out_disks.contains(n)),
        DeviceSpec::WinOther(_) | DeviceSpec::Unix(_) => Ok(false),
    }
}

/// Free bytes available to the user on the volume holding `path`.
pub fn free_space(path: &Path) -> Result<u64> {
    let anc = existing_ancestor(path).context("cannot resolve output folder")?;
    let w = wide(&anc.to_string_lossy());
    let mut avail = 0u64;
    // SAFETY: valid string; optional out-pointers are null.
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, null_mut(), null_mut()) };
    if ok == 0 {
        return Err(io::Error::last_os_error()).context("GetDiskFreeSpaceExW failed");
    }
    Ok(avail)
}

fn volume_total_size(root: &str) -> Option<u64> {
    let w = wide(root);
    let mut total = 0u64;
    // SAFETY: valid string; optional out-pointers are null.
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), null_mut(), &mut total, null_mut()) };
    (ok != 0).then_some(total)
}

struct PhysicalInfo {
    size: Option<u64>,
    removable: bool,
    bus: &'static str,
    model: String,
}

fn physical_info(n: u32) -> Option<PhysicalInfo> {
    let f = open_raw(&format!(r"\\.\PhysicalDrive{n}"), 0).ok()?;
    let size = disk_geometry(&f).map(|g| g.DiskSize as u64);
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut info = PhysicalInfo {
        size,
        removable: false,
        bus: "?",
        model: String::new(),
    };
    if let Ok(out) = ioctl(&f, IOCTL_STORAGE_QUERY_PROPERTY, as_bytes(&query), 1024)
        && let Some(d) = read_struct::<STORAGE_DEVICE_DESCRIPTOR>(&out)
    {
        info.removable = d.RemovableMedia;
        info.bus = match d.BusType {
            1 => "SCSI",
            3 => "ATA",
            7 => "USB",
            8 => "RAID",
            10 => "SAS",
            11 => "SATA",
            12 => "SD",
            13 => "MMC",
            14 | 15 => "Virtual",
            17 => "NVMe",
            _ => "other",
        };
        let cstr = |off: u32| -> String {
            let off = off as usize;
            if off == 0 || off >= out.len() {
                return String::new();
            }
            let end = out[off..]
                .iter()
                .position(|&b| b == 0)
                .map_or(out.len(), |p| off + p);
            String::from_utf8_lossy(&out[off..end]).trim().to_string()
        };
        info.model = [cstr(d.VendorIdOffset), cstr(d.ProductIdOffset)]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
    }
    Some(info)
}

/// Lists volumes (drive letters) and physical drives. Returns how many were listed.
pub fn print_disks(_verbose: bool) -> Result<usize> {
    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    let mut count = 0;
    let mut letters_by_disk: Vec<(u32, char)> = Vec::new();
    println!("Volumes:");
    println!(
        "  {:<6} {:<16} {:<8} {:>10} {:<10} PHYSICAL DRIVE",
        "DRIVE", "LABEL", "FS", "SIZE", "TYPE"
    );
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let w = wide(&root);
        // SAFETY: valid NUL-terminated string.
        let kind = unsafe { GetDriveTypeW(w.as_ptr()) };
        let kind_s = match kind {
            DRIVE_REMOVABLE => "removable",
            DRIVE_FIXED => "fixed",
            DRIVE_REMOTE => "network",
            DRIVE_CDROM => "cd/dvd",
            DRIVE_RAMDISK => "ramdisk",
            _ => continue,
        };
        let vi = volume_info(&root);
        let size = volume_total_size(&root);
        let phys = if kind == DRIVE_REMOTE {
            None
        } else {
            open_raw(&format!(r"\\.\{letter}:"), 0)
                .ok()
                .and_then(|f| device_number(&f))
        };
        if let Some(n) = phys {
            letters_by_disk.push((n, letter));
        }
        println!(
            "  {:<6} {:<16} {:<8} {:>10} {:<10} {}",
            format!("{letter}:"),
            vi.as_ref()
                .map_or("(no media)".to_string(), |v| v.label.clone()),
            vi.as_ref().map_or(String::new(), |v| v.fs.clone()),
            size.map_or("?".to_string(), human_size),
            kind_s,
            phys.map_or("-".to_string(), |n| format!(r"\\.\PhysicalDrive{n}"))
        );
        count += 1;
    }
    println!();
    println!("Physical drives:");
    println!(
        "  {:<20} {:>10} {:<8} {:<10} {:<28} VOLUMES",
        "DEVICE", "SIZE", "BUS", "REMOVABLE", "MODEL"
    );
    for n in 0..64u32 {
        let Some(p) = physical_info(n) else { continue };
        let vols: Vec<String> = letters_by_disk
            .iter()
            .filter(|(d, _)| *d == n)
            .map(|(_, l)| format!("{l}:"))
            .collect();
        println!(
            "  {:<20} {:>10} {:<8} {:<10} {:<28} {}",
            format!(r"\\.\PhysicalDrive{n}"),
            p.size.map_or("?".to_string(), human_size),
            p.bus,
            if p.removable { "yes" } else { "no" },
            p.model,
            vols.join(" ")
        );
        count += 1;
    }
    println!();
    println!("To image a card, run (in a terminal opened with \"Run as administrator\"):");
    println!(r"    sdrescue image \\.\PhysicalDriveN card.img");
    Ok(count)
}

/// Converts a local wall-clock time (as stored by FAT) to Unix seconds, using
/// the current Windows time-zone rules.
pub fn local_to_unix(y: u16, mo: u16, d: u16, h: u16, mi: u16, s: u16) -> Option<i64> {
    let local = SYSTEMTIME {
        wYear: y,
        wMonth: mo,
        wDayOfWeek: 0,
        wDay: d,
        wHour: h,
        wMinute: mi,
        wSecond: s,
        wMilliseconds: 0,
    };
    // SAFETY: zeroed SYSTEMTIME is a valid out-value.
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: null time zone = current; both pointers are valid.
    let ok = unsafe { TzSpecificLocalTimeToSystemTime(null(), &local, &mut utc) };
    if ok == 0 {
        return None;
    }
    let days = days_from_civil(
        i64::from(utc.wYear),
        u32::from(utc.wMonth),
        u32::from(utc.wDay),
    );
    Some(
        days * 86_400
            + i64::from(utc.wHour) * 3600
            + i64::from(utc.wMinute) * 60
            + i64::from(utc.wSecond),
    )
}
