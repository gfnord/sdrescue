//! Byte-for-byte imaging of a card with bad-sector handling.
//!
//! The source is read in large chunks; when a chunk fails, it is retried one
//! sector at a time and sectors that still fail are written as zeros and
//! recorded. A sidecar log records the source, geometry, timing, SHA-256 of
//! the image and the bad sector ranges. Interrupted images can be resumed.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use indicatif::ProgressBar;
use sha2::{Digest, Sha256};

use crate::source::Source;
use crate::util::{format_utc, human_size};

/// Default read size.
pub const CHUNK: usize = 4 << 20;
const LOG_BAD_PREFIX: &str = "bad range: sectors ";

/// A run of unreadable sectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadRange {
    /// First bad sector (in units of the source sector size).
    pub first: u64,
    /// Number of consecutive bad sectors.
    pub count: u64,
}

/// Summary of a finished image.
#[derive(Debug, Clone)]
pub struct ImageSummary {
    /// Bytes in the image.
    pub size: u64,
    /// Sector size used for reads.
    pub sector_size: u32,
    /// SHA-256 of the whole image, lower-case hex.
    pub sha256: String,
    /// Unreadable sectors (filled with zeros in the image).
    pub bad: Vec<BadRange>,
    /// Where this run started (non-zero when resuming).
    pub resumed_at: u64,
}

/// Path of the sidecar log: `<OUTPUT>.log`.
pub fn log_path(output: &Path) -> PathBuf {
    let mut s = output.as_os_str().to_owned();
    s.push(".log");
    PathBuf::from(s)
}

fn push_bad(bad: &mut Vec<BadRange>, sector: u64) -> bool {
    if let Some(last) = bad.last_mut()
        && last.first + last.count == sector
    {
        last.count += 1;
        return false;
    }
    bad.push(BadRange {
        first: sector,
        count: 1,
    });
    true
}

fn format_bad(r: &BadRange, ss: u32) -> String {
    let ss = u64::from(ss);
    format!(
        "{LOG_BAD_PREFIX}{}-{} ({} sectors, bytes {}-{})",
        r.first,
        r.first + r.count - 1,
        r.count,
        r.first * ss,
        (r.first + r.count) * ss - 1
    )
}

/// Parses the bad ranges recorded in an earlier (possibly unfinished) log,
/// merging overlapping and adjacent ranges.
pub fn parse_bad_ranges(log: &str) -> Vec<BadRange> {
    let mut ranges: Vec<BadRange> = Vec::new();
    for line in log.lines() {
        let Some(rest) = line.strip_prefix(LOG_BAD_PREFIX) else {
            continue;
        };
        let span = rest.split_whitespace().next().unwrap_or("");
        let Some((a, b)) = span.split_once('-') else {
            continue;
        };
        if let (Ok(a), Ok(b)) = (a.parse::<u64>(), b.parse::<u64>())
            && b >= a
        {
            ranges.push(BadRange {
                first: a,
                count: b - a + 1,
            });
        }
    }
    ranges.sort_by_key(|r| r.first);
    let mut out: Vec<BadRange> = Vec::new();
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.first <= last.first + last.count => {
                let end = (last.first + last.count).max(r.first + r.count);
                last.count = end - last.first;
            }
            _ => out.push(r),
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct Log {
    path: PathBuf,
    header: Vec<String>,
}

impl Log {
    /// Rewrites the log: header lines, then extra lines, then bad ranges.
    fn write(&self, extra: &[String], bad: &[BadRange], ss: u32) -> io::Result<()> {
        let mut text = self.header.join("\n");
        text.push('\n');
        for l in extra {
            text.push_str(l);
            text.push('\n');
        }
        let total: u64 = bad.iter().map(|r| r.count).sum();
        text.push_str(&format!("bad sectors: {total}\n"));
        for r in bad {
            text.push_str(&format_bad(r, ss));
            text.push('\n');
        }
        std::fs::write(&self.path, text)
    }

    fn append(&self, line: &str) -> io::Result<()> {
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        writeln!(f, "{line}")
    }
}

/// Copies `src` into `output`, writing `<output>.log`.
///
/// Without `resume`, `output` and its log must not exist yet. With `resume`,
/// an existing partial image is continued from its last whole sector.
pub fn image(
    src: &dyn Source,
    source_name: &str,
    output: &Path,
    resume: bool,
    progress: &ProgressBar,
) -> Result<ImageSummary> {
    let size = src.size();
    let ss = src.sector_size().max(1);
    let ss64 = u64::from(ss);
    let log_file = log_path(output);
    let started = SystemTime::now();

    let (mut out, start, mut bad) = if resume && output.exists() {
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(output)
            .with_context(|| format!("cannot open {} to resume", output.display()))?;
        let existing = f.metadata()?.len();
        if existing > size {
            bail!(
                "{} is larger ({existing} bytes) than the source ({size} bytes); it is not an image of this source",
                output.display()
            );
        }
        let start = existing / ss64 * ss64;
        f.set_len(start)?;
        let prev = std::fs::read_to_string(&log_file).unwrap_or_default();
        if let Some(line) = prev.lines().find(|l| l.starts_with("size: ")) {
            let recorded = line
                .trim_start_matches("size: ")
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u64>().ok());
            if recorded.is_some_and(|r| r != size) {
                bail!(
                    "the log {} was written for a source of a different size; refusing to resume",
                    log_file.display()
                );
            }
        }
        let bad: Vec<BadRange> = parse_bad_ranges(&prev)
            .into_iter()
            .filter(|r| r.first * ss64 < start)
            .collect();
        f.seek(SeekFrom::End(0))?;
        (f, start, bad)
    } else {
        if output.exists() {
            bail!(
                "{} already exists. sdrescue never overwrites files: choose another name, or add --resume to continue an interrupted image.",
                output.display()
            );
        }
        if log_file.exists() {
            bail!(
                "{} already exists. Move it away or choose another image name.",
                log_file.display()
            );
        }
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(output)
            .with_context(|| format!("cannot create {}", output.display()))?;
        (f, 0, Vec::new())
    };

    // Hash what is already there when resuming.
    let mut hasher = Sha256::new();
    if start > 0 {
        progress.set_message("hashing existing part");
        let mut rf = File::open(output)?;
        let mut buf = vec![0u8; CHUNK];
        let mut left = start;
        while left > 0 {
            let want = left.min(CHUNK as u64) as usize;
            rf.read_exact(&mut buf[..want])?;
            hasher.update(&buf[..want]);
            left -= want as u64;
        }
    }

    let log = Log {
        path: log_file.clone(),
        header: vec![
            format!("sdrescue {} image log", env!("CARGO_PKG_VERSION")),
            format!("source: {source_name}"),
            format!("output: {}", output.display()),
            format!("size: {size} bytes ({})", human_size(size)),
            format!("sector size: {ss} bytes"),
            format!("started: {}", format_utc(started)),
        ],
    };
    let mut extra = vec!["status: in progress".to_string()];
    if start > 0 {
        extra.push(format!("resumed at: byte {start}"));
    }
    log.write(&extra, &bad, ss)
        .with_context(|| format!("cannot write {}", log_file.display()))?;

    progress.set_message("imaging");
    progress.set_length(size);
    progress.set_position(start);
    let chunk = (CHUNK as u64 / ss64).max(1) * ss64;
    let mut buf = vec![0u8; chunk as usize];
    let mut pos = start;
    while pos < size {
        let len = (size - pos).min(chunk) as usize;
        let data = &mut buf[..len];
        if src.read_exact_at(pos, data).is_err() {
            // Retry sector by sector; unreadable sectors become zeros.
            let mut off = 0usize;
            while off < len {
                let n = (len - off).min(ss as usize);
                let sector_buf = &mut data[off..off + n];
                let abs = pos + off as u64;
                if src.read_exact_at(abs, sector_buf).is_err() {
                    sector_buf.fill(0);
                    let sector = abs / ss64;
                    push_bad(&mut bad, sector);
                    progress.suspend(|| {
                        eprintln!("warning: unreadable sector {sector} (filled with zeros)")
                    });
                    let _ = log.append(&format_bad(
                        &BadRange {
                            first: sector,
                            count: 1,
                        },
                        ss,
                    ));
                }
                off += n;
            }
        }
        out.write_all(data)
            .with_context(|| format!("cannot write to {} (disk full?)", output.display()))?;
        hasher.update(&*data);
        pos += len as u64;
        progress.set_position(pos);
    }
    out.flush()?;
    out.sync_all()?;
    let sha256 = hex(&hasher.finalize());
    let finished = SystemTime::now();
    let mut extra = vec![
        format!("finished: {}", format_utc(finished)),
        "status: complete".to_string(),
        format!("sha256: {sha256}"),
    ];
    if start > 0 {
        extra.push(format!("resumed at: byte {start}"));
    }
    log.write(&extra, &bad, ss)
        .with_context(|| format!("cannot write {}", log_file.display()))?;
    Ok(ImageSummary {
        size,
        sector_size: ss,
        sha256,
        bad,
        resumed_at: start,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source whose sectors in `bad` fail to read.
    struct Flaky {
        data: Vec<u8>,
        bad: Vec<u64>,
    }

    impl Source for Flaky {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            let end = (offset + buf.len() as u64).min(self.size());
            for s in &self.bad {
                let (a, b) = (s * 512, s * 512 + 512);
                if offset < b && end > a {
                    return Err(io::Error::other("CRC error"));
                }
            }
            let n = (end - offset) as usize;
            buf[..n].copy_from_slice(&self.data[offset as usize..end as usize]);
            Ok(n)
        }
        fn describe(&self) -> String {
            "flaky".into()
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sdrescue-img-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn zero_fills_bad_sectors_and_logs_them() {
        let data: Vec<u8> = (0..(CHUNK * 2 + 4096))
            .map(|i| (i % 251) as u8 | 1)
            .collect();
        let src = Flaky {
            data: data.clone(),
            bad: vec![3, 4, 5, 9000],
        };
        let d = tmp("bad");
        let out = d.join("card.img");
        let s = image(&src, "flaky", &out, false, &ProgressBar::hidden()).unwrap();
        let img = std::fs::read(&out).unwrap();
        assert_eq!(img.len(), data.len());
        assert!(img[3 * 512..6 * 512].iter().all(|&b| b == 0));
        assert_eq!(&img[..3 * 512], &data[..3 * 512]);
        assert_eq!(&img[6 * 512..9000 * 512], &data[6 * 512..9000 * 512]);
        assert_eq!(
            s.bad,
            vec![
                BadRange { first: 3, count: 3 },
                BadRange {
                    first: 9000,
                    count: 1
                }
            ]
        );
        assert_eq!(s.sha256, hex(&Sha256::digest(&img)));
        let log = std::fs::read_to_string(log_path(&out)).unwrap();
        assert!(log.contains("status: complete"));
        assert!(log.contains(&format!("sha256: {}", s.sha256)));
        assert_eq!(parse_bad_ranges(&log), s.bad);
        // Never overwrites.
        assert!(image(&src, "flaky", &out, false, &ProgressBar::hidden()).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn resume_continues_and_hashes_everything() {
        let data: Vec<u8> = (0..(CHUNK + 10_000))
            .map(|i| (i * 31 % 256) as u8)
            .collect();
        let src = Flaky {
            data: data.clone(),
            bad: vec![],
        };
        let d = tmp("resume");
        let out = d.join("card.img");
        // Simulate an interrupted run: a partial image with a torn last sector.
        std::fs::write(&out, &data[..1_000_000 + 100]).unwrap();
        let s = image(&src, "flaky", &out, true, &ProgressBar::hidden()).unwrap();
        assert_eq!(s.resumed_at, 1_000_000 / 512 * 512);
        assert_eq!(std::fs::read(&out).unwrap(), data);
        assert_eq!(s.sha256, hex(&Sha256::digest(&data)));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
