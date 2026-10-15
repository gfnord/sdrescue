//! `sdrescue undelete`: recover deleted files with their names and folders.

use std::io::{BufWriter, Write};

use anyhow::{Context, Result};

use super::list::collect;
use super::{Outcome, open_volume};
use crate::cli::{GlobalOpts, UndeleteArgs};
use crate::device::{ensure_output_not_on_source, open_for_analysis};
use crate::output::{create_unique, set_modified};
use crate::recover::{Status, copy_contiguous, destination};

/// Runs `undelete`.
pub fn run(a: &UndeleteArgs, g: &GlobalOpts) -> Result<Outcome> {
    ensure_output_not_on_source(&a.source, &a.out)?;
    let src = open_for_analysis(&a.source, g.force, "undelete")?;
    let (_, vol) = open_volume(src.as_ref(), g)?;
    let rows = collect(&vol, &a.filter, false);
    std::fs::create_dir_all(&a.out)
        .with_context(|| format!("cannot create output folder {}", a.out.display()))?;
    let (report_file, report_path) = create_unique(&a.out.join("report.csv"))?;
    let mut report = csv::Writer::from_writer(report_file);
    report.write_record([
        "original_path",
        "recovered_path",
        "size",
        "status",
        "signature",
    ])?;
    let (mut recovered, mut skipped, mut failed) = (0usize, 0usize, 0usize);
    for r in &rows {
        if r.status == Status::Overwritten && !a.include_overwritten {
            skipped += 1;
            if g.verbose {
                eprintln!("skipping {} (overwritten)", r.rec.path);
            }
            continue;
        }
        let dest = destination(&a.out, &r.rec.path);
        let result = (|| -> Result<(std::path::PathBuf, u64)> {
            let (file, used) = create_unique(&dest)?;
            let mut w = BufWriter::new(file);
            let n = copy_contiguous(&vol, &r.rec, &mut w)?;
            w.flush()?;
            let file = w.into_inner().map_err(|e| e.into_error())?;
            if let Some(t) = r.rec.modified.and_then(|m| m.to_unix()) {
                set_modified(&file, t);
            }
            Ok((used, n))
        })();
        match result {
            Ok((used, n)) => {
                recovered += 1;
                if n < u64::from(r.rec.size) {
                    eprintln!(
                        "warning: {} is truncated ({} of {} bytes; the image ends early)",
                        r.rec.path, n, r.rec.size
                    );
                }
                if g.verbose {
                    println!("{} -> {}", r.rec.path, used.display());
                }
                let rel = used.strip_prefix(&a.out).unwrap_or(&used);
                report.write_record([
                    r.rec.path.as_str(),
                    &rel.to_string_lossy(),
                    &r.rec.size.to_string(),
                    r.status.as_str(),
                    r.signature.as_str(),
                ])?;
            }
            Err(e) => {
                failed += 1;
                eprintln!("error: could not recover {}: {e:#}", r.rec.path);
                report.write_record([
                    r.rec.path.as_str(),
                    "",
                    &r.rec.size.to_string(),
                    "failed",
                    r.signature.as_str(),
                ])?;
            }
        }
    }
    report.flush()?;
    println!(
        "Recovered {recovered} file(s) into {}{}{}",
        a.out.display(),
        if skipped > 0 {
            format!(", skipped {skipped} overwritten (use --include-overwritten to try them)")
        } else {
            String::new()
        },
        if failed > 0 {
            format!(", {failed} failed")
        } else {
            String::new()
        }
    );
    println!("Report: {}", report_path.display());
    Ok(if recovered == 0 {
        Outcome::NothingFound
    } else {
        Outcome::Done
    })
}
