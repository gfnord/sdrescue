//! Read-only access to the bytes being analysed.
//!
//! Everything above this module (partition parsing, FAT, carving) works
//! through the [`Source`] trait, so it does not care whether the bytes come
//! from an image file or from a raw device.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A read-only, random-access byte source (an image file or a raw device).
pub trait Source: Send + Sync {
    /// Total size in bytes.
    fn size(&self) -> u64;

    /// Logical sector size in bytes (512 for image files).
    fn sector_size(&self) -> u32 {
        512
    }

    /// Reads up to `buf.len()` bytes starting at `offset`.
    ///
    /// Returns the number of bytes read; this is only shorter than `buf`
    /// when the end of the source is reached.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

    /// Human-readable description, e.g. the path that was opened.
    fn describe(&self) -> String;

    /// Reads exactly `buf.len()` bytes at `offset`, failing at end of source.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let n = self.read_at(offset, buf)?;
        if n < buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "read of {} bytes at offset {offset} goes past the end of the source",
                    buf.len()
                ),
            ));
        }
        Ok(())
    }

    /// Reads `len` bytes at `offset` into a new vector, failing at end of source.
    fn read_vec(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        self.read_exact_at(offset, &mut buf)?;
        Ok(buf)
    }
}

/// Positional read on a [`File`] that loops over short reads.
pub(crate) fn file_read_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while !buf.is_empty() {
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(file, buf, offset);
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_read(file, buf, offset);
        match n {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                offset += n as u64;
                buf = &mut buf[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

/// A disk image stored in a regular file.
pub struct ImageFile {
    file: File,
    path: PathBuf,
    size: u64,
}

impl ImageFile {
    /// Opens an image file read-only.
    pub fn open(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("cannot open image {}", path.display()))?;
        let meta = file
            .metadata()
            .with_context(|| format!("cannot read metadata of {}", path.display()))?;
        if meta.is_dir() {
            anyhow::bail!("{} is a folder, not an image file", path.display());
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            size: meta.len(),
        })
    }
}

impl Source for ImageFile {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.size {
            return Ok(0);
        }
        let avail = (self.size - offset).min(buf.len() as u64) as usize;
        file_read_at(&self.file, offset, &mut buf[..avail])
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

/// An in-memory source, used by tests.
#[cfg(test)]
pub struct MemSource(pub Vec<u8>);

#[cfg(test)]
impl Source for MemSource {
    fn size(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let Ok(off) = usize::try_from(offset) else {
            return Ok(0);
        };
        if off >= self.0.len() {
            return Ok(0);
        }
        let n = buf.len().min(self.0.len() - off);
        buf[..n].copy_from_slice(&self.0[off..off + n]);
        Ok(n)
    }
    fn describe(&self) -> String {
        "memory".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_source_short_reads() {
        let s = MemSource((0u8..10).collect());
        let mut b = [0u8; 4];
        assert_eq!(s.read_at(8, &mut b).unwrap(), 2);
        assert_eq!(&b[..2], &[8, 9]);
        assert!(s.read_exact_at(8, &mut b).is_err());
        assert_eq!(s.read_at(100, &mut b).unwrap(), 0);
    }
}
