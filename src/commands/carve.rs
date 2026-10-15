//! `sdrescue carve`: signature-based recovery into `<OUT>/<ext>/f<offset>.<ext>`.

use std::io::{BufWriter, Write};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};

use super::Outcome;
use crate::carve::engine::{SizeCaps, find_candidates, resolve};
use crate::carve::{Input, all_carvers};
use crate::cli::{CarveArgs, GlobalOpts};
use crate::device::{ensure_output_not_on_source, open_for_analysis};
use crate::fat::FatVolume;
use crate::output::create_unique;
use crate::partition::{FsKind, locate};
use crate::source::Source;
use crate::util::human_size;

/// Byte ranges to scan: everything outside the FAT data region plus the free
/// clusters inside it. `None` when the FAT cannot be used.
fn free_ranges(src: &dyn Source, g: &GlobalOpts) -> Option<Vec<(u64, u64)>> {
    let layout = locate(src, g.offset).ok()?;
    if !matches!(layout.fs, FsKind::Fat(_)) {
        return None;
    }
    let vol = FatVolume::open(src, layout.fs_offset).ok()?;
    let (data_start, data_end) = vol.data_region();
    let cs = vol.bpb.cluster_size();
    let mut ranges = vec![(0, data_start.min(src.size()))];
    let mut run: Option<u32> = None;
    let last = vol.bpb.cluster_count + 2;
    for c in 2..=last {
        let free = c < last && vol.fat.is_free(c);
        match (free, run) {
            (true, None) => run = Some(c),
            (false, Some(s)) => {
                let a = data_start + u64::from(s - 2) * cs;
                let b = data_start + u64::from(c - 2) * cs;
                ranges.push((a, b));
                run = None;
            }
            _ => {}
        }
    }
    if data_end < src.size() {
        ranges.push((data_end, src.size()));
    }
    ranges.retain(|(a, b)| b > a);
    Some(ranges)
}

fn bytes_bar(total: u64, msg: &'static str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "{msg:12} [{bar:36}] {bytes}/{total_bytes} {binary_bytes_per_sec} ETA {eta}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> "),
    );
    pb.set_message(msg);
    pb
}

fn count_bar(total: u64, msg: &'static str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template("{msg:12} [{bar:36}] {pos}/{len} ETA {eta}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> "),
    );
    pb.set_message(msg);
    pb
}

/// Runs `carve`.
pub fn run(a: &CarveArgs, g: &GlobalOpts) -> Result<Outcome> {
    ensure_output_not_on_source(&a.source, &a.out)?;
    let src = open_for_analysis(&a.source, g.force, "carve")?;
    let src = src.as_ref();
    let free = if a.all_space {
        None
    } else {
        let r = free_ranges(src, g);
        if r.is_none() {
            eprintln!(
                "note: no usable FAT filesystem, so free space is unknown; scanning the whole source"
            );
        }
        r
    };
    let free_only = free.is_some();
    let ranges = free.unwrap_or_else(|| vec![(0, src.size())]);
    let scan_total: u64 = ranges.iter().map(|(a, b)| b - a).sum();
    println!(
        "Scanning {} of {} for file headers{}",
        human_size(scan_total),
        human_size(src.size()),
        if free_only {
            " (free space only; use --all-space for everything)"
        } else {
            ""
        }
    );
    let carvers = all_carvers();
    let pb = bytes_bar(scan_total, "searching");
    let candidates = find_candidates(src, &ranges, &carvers, &a.filter, &pb);
    pb.finish_and_clear();
    if g.verbose {
        eprintln!("{} candidate header(s)", candidates.len());
    }
    let caps = SizeCaps {
        photo: a.max_photo,
        video: a.max_video,
        audio: a.max_audio,
    };
    let input = Input::new(src);
    let pb = count_bar(candidates.len() as u64, "analysing");
    let files = resolve(&input, &candidates, &carvers, &caps, &pb);
    pb.finish_and_clear();

    std::fs::create_dir_all(&a.out)
        .with_context(|| format!("cannot create output folder {}", a.out.display()))?;
    let (report_file, report_path) = create_unique(&a.out.join("report.csv"))?;
    let mut report = csv::Writer::from_writer(report_file);
    report.write_record([
        "offset",
        "length",
        "type",
        "end",
        "truncated",
        "file",
        "note",
    ])?;
    let total: u64 = files.iter().map(|f| f.carved.len).sum();
    let pb = bytes_bar(total, "writing");
    let mut buf = vec![0u8; 1 << 20];
    let mut written = 0usize;
    for f in &files {
        let ext = f.carved.ext;
        let dest = a.out.join(ext).join(format!("f{:09}.{ext}", f.offset));
        let (file, used) = create_unique(&dest)?;
        let mut w = BufWriter::new(file);
        let mut pos = f.offset;
        let end = f.offset + f.carved.len;
        while pos < end {
            let want = ((end - pos) as usize).min(buf.len());
            let n = src
                .read_at(pos, &mut buf[..want])
                .with_context(|| format!("read error at offset {pos}"))?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n])
                .with_context(|| format!("cannot write {}", used.display()))?;
            pos += n as u64;
            pb.inc(n as u64);
        }
        w.flush()?;
        written += 1;
        if g.verbose {
            pb.println(format!(
                "{} at {} ({}, end: {}{})",
                used.display(),
                f.offset,
                human_size(f.carved.len),
                f.carved.end,
                if f.carved.truncated {
                    ", truncated"
                } else {
                    ""
                }
            ));
        }
        let rel = used.strip_prefix(&a.out).unwrap_or(&used);
        report.write_record([
            f.offset.to_string(),
            f.carved.len.to_string(),
            ext.to_string(),
            f.carved.end.to_string(),
            if f.carved.truncated { "yes" } else { "no" }.to_string(),
            rel.to_string_lossy().into_owned(),
            f.carved.note.clone().unwrap_or_default(),
        ])?;
    }
    pb.finish_and_clear();
    report.flush()?;
    let mut by_ext: Vec<(&str, usize)> = Vec::new();
    for f in &files {
        match by_ext.iter_mut().find(|(e, _)| *e == f.carved.ext) {
            Some((_, n)) => *n += 1,
            None => by_ext.push((f.carved.ext, 1)),
        }
    }
    by_ext.sort();
    let summary: Vec<String> = by_ext.iter().map(|(e, n)| format!("{n} {e}")).collect();
    println!(
        "Carved {written} file(s) ({}) into {}{}",
        human_size(total),
        a.out.display(),
        if summary.is_empty() {
            String::new()
        } else {
            format!(": {}", summary.join(", "))
        }
    );
    let notes = files.iter().filter(|f| f.carved.note.is_some()).count();
    if notes > 0 {
        println!("{notes} file(s) have notes in the report (for example videos that need repair).");
    }
    println!("Report: {}", report_path.display());
    Ok(if written == 0 {
        Outcome::NothingFound
    } else {
        Outcome::Done
    })
}
