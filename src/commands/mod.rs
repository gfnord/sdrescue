//! Implementations of the subcommands.

mod carve;
mod image;
mod info;
mod list;
mod undelete;

use anyhow::{Result, bail};

use crate::cli::{Cli, Command, GlobalOpts};
use crate::fat::FatVolume;
use crate::partition::{FsKind, Layout, locate};
use crate::source::Source;

/// How a successful command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Something was found / done (exit code 0).
    Done,
    /// Finished, but nothing was found or recovered (exit code 2).
    NothingFound,
}

/// Runs the parsed command line.
pub fn run(cli: Cli) -> Result<Outcome> {
    let g = &cli.global;
    match cli.command {
        Command::Disks => {
            let n = crate::device::print_disks(g.verbose)?;
            Ok(if n == 0 {
                Outcome::NothingFound
            } else {
                Outcome::Done
            })
        }
        Command::Image(a) => image::run(&a, g),
        Command::Info(a) => info::run(&a.source, g),
        Command::List(a) => list::run(&a, g),
        Command::Undelete(a) => undelete::run(&a, g),
        Command::Carve(a) => carve::run(&a, g),
    }
}

/// Explains why `undelete`/`list` cannot work on a non-FAT filesystem.
fn unsupported_fs(layout: &Layout) -> String {
    match &layout.fs {
        FsKind::ExFat | FsKind::Ntfs => format!(
            "the filesystem at offset {} is {}. `list` and `undelete` only support FAT12/16/32 \
             for now, but `sdrescue carve` works on any filesystem:\n    sdrescue carve <image> --out carved",
            layout.fs_offset,
            layout.fs.name()
        ),
        FsKind::Unknown(e) => format!(
            "no FAT filesystem found at offset {}: {e}.\n\
             Run `sdrescue info` to see the partition table, try --offset <BYTES>, or use \
             `sdrescue carve`, which does not need a filesystem.",
            layout.fs_offset
        ),
        FsKind::Fat(_) => String::new(),
    }
}

/// Locates and opens the FAT volume of `src`, printing warnings to stderr.
fn open_volume<'a>(src: &'a dyn Source, g: &GlobalOpts) -> Result<(Layout, FatVolume<'a>)> {
    let layout = locate(src, g.offset)?;
    if !matches!(layout.fs, FsKind::Fat(_)) {
        bail!("{}", unsupported_fs(&layout));
    }
    let vol = FatVolume::open(src, layout.fs_offset)?;
    for w in &vol.warnings {
        eprintln!("warning: {w}");
    }
    if g.verbose {
        eprintln!(
            "{} filesystem at offset {} ({} clusters of {} bytes)",
            vol.bpb.fat_type,
            vol.offset,
            vol.bpb.cluster_count,
            vol.bpb.cluster_size()
        );
    }
    Ok((layout, vol))
}
