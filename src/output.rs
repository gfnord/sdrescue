//! Writing recovered files safely: sanitised names, never overwriting, timestamps.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Names Windows refuses to create, whatever the extension.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Makes one path component safe on Windows and Linux.
pub fn sanitize_component(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    if s.is_empty() || s == "." || s == ".." {
        s = "_".into();
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        s.insert(0, '_');
    }
    s
}

/// `name (2).ext`, `name (3).ext`, ... for the n-th alternative.
fn numbered(path: &Path, n: u32) -> PathBuf {
    let file = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (stem, ext) = match file.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (file.clone(), String::new()),
    };
    path.with_file_name(format!("{stem} ({n}){ext}"))
}

/// Creates `path` for writing without ever replacing an existing file: when
/// the name is taken, ` (2)`, ` (3)`, ... is added before the extension.
/// Returns the file and the path actually used.
pub fn create_unique(path: &Path) -> Result<(File, PathBuf)> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create folder {}", parent.display()))?;
    }
    let mut candidate = path.to_path_buf();
    for n in 2..100_000u32 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(f) => return Ok((f, candidate)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                candidate = numbered(path, n);
            }
            Err(e) => {
                return Err(e).with_context(|| format!("cannot create {}", candidate.display()));
            }
        }
    }
    anyhow::bail!("too many files named like {}", path.display())
}

/// Sets a file's modification time from Unix seconds (best effort: a failure
/// only loses the timestamp, never the data).
pub fn set_modified(file: &File, unix_secs: i64) {
    if let Ok(secs) = u64::try_from(unix_secs) {
        let _ = file.set_modified(UNIX_EPOCH + Duration::from_secs(secs));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes() {
        assert_eq!(sanitize_component("a:b*c?.jpg"), "a_b_c_.jpg");
        assert_eq!(sanitize_component("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_component("trailing. "), "trailing");
        assert_eq!(sanitize_component(".."), "_");
        assert_eq!(sanitize_component("ok name.JPG"), "ok name.JPG");
    }

    #[test]
    fn never_overwrites() {
        let dir = std::env::temp_dir().join(format!("sdrescue-out-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let p = dir.join("sub/IMG_0001.JPG");
        let (_, a) = create_unique(&p).unwrap();
        let (_, b) = create_unique(&p).unwrap();
        let (_, c) = create_unique(&p).unwrap();
        assert_eq!(a, p);
        assert_eq!(b, dir.join("sub/IMG_0001 (2).JPG"));
        assert_eq!(c, dir.join("sub/IMG_0001 (3).JPG"));
        let (_, d) = create_unique(&dir.join("noext")).unwrap();
        let (_, e) = create_unique(&dir.join("noext")).unwrap();
        assert_eq!(d, dir.join("noext"));
        assert_eq!(e, dir.join("noext (2)"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
