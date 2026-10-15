//! `sdrescue image`: copy a card byte for byte into an image file.

use anyhow::{Result, bail};
use indicatif::{ProgressBar, ProgressStyle};

use super::Outcome;
use crate::cli::{GlobalOpts, ImageArgs};
use crate::device::{DeviceSpec, ensure_output_not_on_source, free_space, open_for_imaging};
use crate::imaging::{image, log_path};
use crate::util::human_size;

/// Runs `image`.
pub fn run(a: &ImageArgs, _g: &GlobalOpts) -> Result<Outcome> {
    let log = log_path(&a.output);
    ensure_output_not_on_source(&a.source, &a.output)?;
    ensure_output_not_on_source(&a.source, &log)?;
    if !a.resume && a.output.exists() {
        bail!(
            "{} already exists. sdrescue never overwrites files: choose another name, or add --resume to continue an interrupted image.",
            a.output.display()
        );
    }
    let src = open_for_imaging(&a.source)?;
    let src = src.as_ref();
    let size = src.size();
    if size == 0 {
        bail!(
            "{} reports a size of 0 bytes (is a card inserted in the reader?)",
            src.describe()
        );
    }
    let existing = if a.resume {
        std::fs::metadata(&a.output).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    let needed = size.saturating_sub(existing);
    match free_space(&a.output) {
        Ok(free) if free < needed => bail!(
            "not enough free space for the image: it needs {} ({needed} bytes) but only {} is free there.\n\
             Choose a folder on a drive with more space.",
            human_size(needed),
            human_size(free)
        ),
        Ok(_) => {}
        Err(e) => eprintln!("warning: could not check free space: {e:#}"),
    }
    println!(
        "Source:      {} ({}, {} bytes, {}-byte sectors)",
        src.describe(),
        human_size(size),
        size,
        src.sector_size()
    );
    println!("Image:       {}", a.output.display());
    println!("Log:         {}", log.display());
    if matches!(DeviceSpec::parse(&a.source), Some(DeviceSpec::WinVolume(_))) {
        println!(
            "Note:        imaging a volume (drive letter) copies only that partition. Imaging the\n             \
             physical drive (\\\\.\\PhysicalDriveN, see `sdrescue disks`) also captures the partition table."
        );
    }
    let pb = ProgressBar::new(size);
    pb.set_style(
        ProgressStyle::with_template(
            "{msg:10} [{bar:36}] {bytes}/{total_bytes} {binary_bytes_per_sec} ETA {eta}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> "),
    );
    let summary = image(src, &a.source, &a.output, a.resume, &pb)?;
    pb.finish_and_clear();
    let bad: u64 = summary.bad.iter().map(|r| r.count).sum();
    if summary.resumed_at > 0 {
        println!("Resumed at:  byte {}", summary.resumed_at);
    }
    println!(
        "Done:        {} written, SHA-256 {}",
        human_size(summary.size),
        summary.sha256
    );
    if bad > 0 {
        println!(
            "Warning:     {bad} unreadable sector(s) ({}) were filled with zeros; see {}",
            human_size(bad * u64::from(summary.sector_size)),
            log.display()
        );
    } else {
        println!("Bad sectors: none");
    }
    println!();
    println!("Next steps (on the image, not the card):");
    println!("    sdrescue info {}", a.output.display());
    println!("    sdrescue list {}", a.output.display());
    Ok(Outcome::Done)
}
