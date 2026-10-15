//! `sdrescue list`: table (or JSON) of deleted files found in the directories.

use anyhow::Result;

use super::{Outcome, open_volume};
use crate::cli::{GlobalOpts, ListArgs, TypeFilter};
use crate::device::open_for_analysis;
use crate::fat::FatVolume;
use crate::recover::{Analyzed, Status, analyze};
use crate::util::human_size;

/// Analyses every file (not directory) that passes the filters.
pub(super) fn collect(
    vol: &FatVolume,
    filter: &TypeFilter,
    include_existing: bool,
) -> Vec<Analyzed> {
    let walk = vol.walk();
    for w in &walk.warnings {
        eprintln!("warning: {w}");
    }
    walk.records
        .iter()
        .filter(|r| !r.is_dir && (r.deleted || include_existing))
        .map(|r| analyze(vol, r))
        .filter(|a| filter.accepts(a.category))
        .collect()
}

/// Runs `list`.
pub fn run(a: &ListArgs, g: &GlobalOpts) -> Result<Outcome> {
    let src = open_for_analysis(&a.source, g.force, "list")?;
    let (_, vol) = open_volume(src.as_ref(), g)?;
    let rows = collect(&vol, &a.filter, a.all);
    let deleted = rows.iter().filter(|r| r.rec.deleted).count();
    if a.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print_table(&rows, g.verbose);
        let count = |s: Status| rows.iter().filter(|r| r.status == s).count();
        println!();
        println!(
            "{deleted} deleted file(s): {} good, {} partial, {} overwritten{}",
            count(Status::Good),
            count(Status::Partial),
            count(Status::Overwritten),
            if a.all {
                format!("; {} existing", count(Status::Existing))
            } else {
                String::new()
            }
        );
    }
    Ok(if deleted == 0 {
        Outcome::NothingFound
    } else {
        Outcome::Done
    })
}

fn print_table(rows: &[Analyzed], verbose: bool) {
    let headers = [
        "PATH",
        "SIZE",
        "DATE",
        "START CLUSTER",
        "STATUS",
        "SIGNATURE",
    ];
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|r| {
            let mut path = r.rec.path.clone();
            if verbose && r.rec.first_char_guessed {
                path.push_str("  (first letter unknown)");
            }
            [
                path,
                if verbose {
                    r.rec.size.to_string()
                } else {
                    human_size(u64::from(r.rec.size))
                },
                r.modified.clone().unwrap_or_else(|| "-".into()),
                r.rec.start_cluster.to_string(),
                r.status.as_str().to_string(),
                r.signature.as_str().to_string(),
            ]
        })
        .collect();
    let mut w = headers.map(str::len);
    for c in &cells {
        for (i, s) in c.iter().enumerate() {
            w[i] = w[i].max(s.chars().count());
        }
    }
    let line = |c: &[String; 6]| {
        println!(
            "{:<w0$}  {:>w1$}  {:<w2$}  {:>w3$}  {:<w4$}  {}",
            c[0],
            c[1],
            c[2],
            c[3],
            c[4],
            c[5],
            w0 = w[0],
            w1 = w[1],
            w2 = w[2],
            w3 = w[3],
            w4 = w[4]
        )
    };
    line(&headers.map(String::from));
    for c in &cells {
        line(c);
    }
}
