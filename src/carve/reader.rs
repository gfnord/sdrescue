//! Cached random-access reader used by the format carvers.

use std::cell::RefCell;

use crate::source::Source;

const BLOCK: usize = 1 << 20;

struct Block {
    start: u64,
    data: Vec<u8>,
}

/// Read-only view of the source with a 1 MiB block cache, so carvers can do
/// many small structured reads (box headers, frame headers) cheaply.
///
/// Read errors are treated like the end of the data: a carver simply sees
/// fewer bytes and stops there.
pub struct Input<'a> {
    src: &'a dyn Source,
    cache: RefCell<Block>,
}

impl<'a> Input<'a> {
    /// Wraps a source.
    pub fn new(src: &'a dyn Source) -> Input<'a> {
        Input {
            src,
            cache: RefCell::new(Block {
                start: u64::MAX,
                data: Vec::new(),
            }),
        }
    }

    /// The wrapped source.
    pub fn source(&self) -> &'a dyn Source {
        self.src
    }

    /// Total size of the source.
    pub fn len(&self) -> u64 {
        self.src.size()
    }

    /// True for an empty source.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads up to `buf.len()` bytes at `off`; returns how many were read.
    pub fn read(&self, off: u64, buf: &mut [u8]) -> usize {
        if buf.len() > BLOCK / 2 {
            return self.src.read_at(off, buf).unwrap_or(0);
        }
        let mut done = 0;
        while done < buf.len() {
            let pos = off + done as u64;
            let mut c = self.cache.borrow_mut();
            if pos < c.start || pos >= c.start.saturating_add(c.data.len() as u64) {
                let start = pos - pos % BLOCK as u64;
                c.data.resize(BLOCK, 0);
                let n = self.src.read_at(start, &mut c.data).unwrap_or(0);
                c.data.truncate(n);
                c.start = start;
                if pos >= start + n as u64 {
                    break;
                }
            }
            let inner = (pos - c.start) as usize;
            let n = (c.data.len() - inner).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&c.data[inner..inner + n]);
            done += n;
        }
        done
    }

    /// Reads exactly `buf.len()` bytes; false if not available.
    pub fn read_exact(&self, off: u64, buf: &mut [u8]) -> bool {
        self.read(off, buf) == buf.len()
    }

    /// `len` bytes at `off`, or `None` if not all available.
    pub fn bytes(&self, off: u64, len: usize) -> Option<Vec<u8>> {
        let mut v = vec![0u8; len];
        self.read_exact(off, &mut v).then_some(v)
    }

    /// Fixed-size array at `off`.
    pub fn array<const N: usize>(&self, off: u64) -> Option<[u8; N]> {
        let mut a = [0u8; N];
        self.read_exact(off, &mut a).then_some(a)
    }

    /// Byte at `off`.
    pub fn u8(&self, off: u64) -> Option<u8> {
        self.array::<1>(off).map(|a| a[0])
    }

    /// Big-endian u16 at `off`.
    pub fn be16(&self, off: u64) -> Option<u16> {
        self.array(off).map(u16::from_be_bytes)
    }

    /// Big-endian u32 at `off`.
    pub fn be32(&self, off: u64) -> Option<u32> {
        self.array(off).map(u32::from_be_bytes)
    }

    /// Big-endian u64 at `off`.
    pub fn be64(&self, off: u64) -> Option<u64> {
        self.array(off).map(u64::from_be_bytes)
    }

    /// Little-endian u16 at `off`.
    pub fn le16(&self, off: u64) -> Option<u16> {
        self.array(off).map(u16::from_le_bytes)
    }

    /// Little-endian u32 at `off`.
    pub fn le32(&self, off: u64) -> Option<u32> {
        self.array(off).map(u32::from_le_bytes)
    }

    /// Little-endian u64 at `off`.
    pub fn le64(&self, off: u64) -> Option<u64> {
        self.array(off).map(u64::from_le_bytes)
    }

    /// Calls `f(absolute_offset, chunk)` over `[from, to)` in chunks of up to
    /// 64 KiB until `f` returns `Some`, which is then returned.
    pub fn scan<T>(
        &self,
        from: u64,
        to: u64,
        mut f: impl FnMut(u64, &[u8]) -> Option<T>,
    ) -> Option<T> {
        let mut buf = vec![0u8; 64 * 1024];
        let mut pos = from;
        while pos < to {
            let want = ((to - pos) as usize).min(buf.len());
            let n = self.read(pos, &mut buf[..want]);
            if n == 0 {
                return None;
            }
            if let Some(t) = f(pos, &buf[..n]) {
                return Some(t);
            }
            pos += n as u64;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemSource;

    #[test]
    fn reads_across_blocks_and_end() {
        let data: Vec<u8> = (0..(BLOCK * 2 + 100)).map(|i| (i % 253) as u8).collect();
        let src = MemSource(data.clone());
        let inp = Input::new(&src);
        let mut b = [0u8; 300];
        let off = BLOCK as u64 - 150;
        assert!(inp.read_exact(off, &mut b));
        assert_eq!(&b[..], &data[BLOCK - 150..BLOCK + 150]);
        assert_eq!(inp.read(data.len() as u64 - 10, &mut b), 10);
        assert_eq!(inp.be16(0), Some(u16::from_be_bytes([0, 1])));
        assert_eq!(inp.u8(data.len() as u64), None);
        let found = inp.scan(0, data.len() as u64, |o, c| {
            c.iter().position(|&x| x == 252).map(|p| o + p as u64)
        });
        assert_eq!(found, Some(252));
    }
}
