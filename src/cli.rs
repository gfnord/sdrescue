//! Command-line interface definitions (clap derive API).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

pub use crate::filetype::Category;
use crate::util::parse_size;

const ABOUT: &str = "Recover deleted photos, videos and audio from FAT-formatted SD cards.";

const LONG_ABOUT: &str = "\
Recover deleted photos, videos and audio from FAT12/FAT16/FAT32 SD cards.

Work on a copy, never on the card: first make an image of the card with
`sdrescue image`, then run `info`, `list`, `undelete` and `carve` on that image.
Analysis commands refuse to run on a device unless --force is given.";

const EXAMPLES: &str = "\
Examples (Windows, in a terminal opened with \"Run as administrator\"):
  sdrescue disks
  sdrescue image \\\\.\\PhysicalDrive2 card.img
  sdrescue info card.img
  sdrescue list card.img --type photo
  sdrescue undelete card.img --out recovered
  sdrescue carve card.img --out carved

Exit codes: 0 = success, 1 = error, 2 = finished but nothing was found or recovered.";

/// Top-level command line.
#[derive(Debug, Parser)]
#[command(name = "sdrescue", version, about = ABOUT, long_about = LONG_ABOUT, after_help = EXAMPLES)]
pub struct Cli {
    /// Options shared by all commands.
    #[command(flatten)]
    pub global: GlobalOpts,
    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Options accepted by every command.
#[derive(Debug, Args, Clone)]
pub struct GlobalOpts {
    /// Start of the FAT filesystem inside the source, in bytes (overrides partition detection).
    /// Accepts suffixes: 1048576, 1M, 2048s (sectors of 512 bytes), 0x100000.
    #[arg(long, global = true, value_name = "BYTES", value_parser = parse_size)]
    pub offset: Option<u64>,

    /// Allow analysis commands to run directly on a device instead of an image file.
    #[arg(long, global = true)]
    pub force: bool,

    /// Show more details.
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

/// Parsed `--type` filter. Empty means "all categories".
#[derive(Debug, Args, Clone, Default)]
pub struct TypeFilter {
    /// Only these categories (comma-separated): photo, video, audio.
    #[arg(long = "type", value_name = "TYPES", value_delimiter = ',')]
    pub types: Vec<Category>,
}

impl TypeFilter {
    /// Whether a file of category `cat` (None = not a media file) passes the filter.
    pub fn accepts(&self, cat: Option<Category>) -> bool {
        self.types.is_empty() || cat.is_some_and(|c| self.types.contains(&c))
    }
}

/// The subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// List drives so you can find your SD card.
    #[command(after_help = "Examples:\n  sdrescue disks\n  sdrescue disks -v")]
    Disks,

    /// Copy the whole card, byte for byte, into an image file (the first step).
    #[command(after_help = "\
Examples:
  sdrescue image \\\\.\\PhysicalDrive2 card.img   (whole card: recommended)
  sdrescue image F: card.img                    (only the partition behind F:)
  sdrescue image /dev/mmcblk0 card.img          (Linux)
  sdrescue image \\\\.\\PhysicalDrive2 card.img --resume

Imaging the physical drive is recommended because it also captures the partition
table and any data outside the partition. A <OUTPUT>.log file is written next to the
image with the SHA-256 hash and the list of unreadable sectors.")]
    Image(ImageArgs),

    /// Show the partition table and the filesystem geometry of an image.
    #[command(
        after_help = "Examples:\n  sdrescue info card.img\n  sdrescue info card.img --offset 4M"
    )]
    Info(SourceArg),

    /// List deleted files found in the directory structure.
    #[command(after_help = "\
Examples:
  sdrescue list card.img
  sdrescue list card.img --type photo,video
  sdrescue list card.img --all
  sdrescue list card.img --json > files.json

Status: good = all its clusters are still free, partial = some were reused,
overwritten = the first cluster now belongs to another file.")]
    List(ListArgs),

    /// Recover deleted files with their original names and folders.
    #[command(after_help = "\
Examples:
  sdrescue undelete card.img --out recovered
  sdrescue undelete card.img --out recovered --type photo
  sdrescue undelete card.img --out recovered --include-overwritten

A report.csv with one row per file is written to the output folder.")]
    Undelete(UndeleteArgs),

    /// Find files by their content signatures (works even after reformatting).
    #[command(after_help = "\
Examples:
  sdrescue carve card.img --out carved
  sdrescue carve card.img --out carved --type video
  sdrescue carve card.img --out carved --all-space
  sdrescue carve card.img --out carved --max-video 8G
  sdrescue carve card.img --out carved --avi-reference good_clip.avi

Files are saved as <OUT>/<ext>/f<byte offset>.<ext> and listed in <OUT>/report.csv.
AVI videos whose header was overwritten (common after a reformat) are rebuilt as
<OUT>/avi/f<byte offset>-rebuilt.avi. Without --avi-reference, MJPEG video is
assumed to run at --avi-fps and audio is assumed to be 16-bit PCM.")]
    Carve(CarveArgs),
}

/// Arguments of `image`.
#[derive(Debug, Args)]
pub struct ImageArgs {
    /// Device to copy: F:, \\.\F:, \\.\PhysicalDriveN (Windows) or /dev/sdX (Linux).
    pub source: String,
    /// Image file to create (must not exist yet, unless --resume).
    pub output: PathBuf,
    /// Continue an interrupted image from where it stopped.
    #[arg(long)]
    pub resume: bool,
}

/// A single source argument.
#[derive(Debug, Args)]
pub struct SourceArg {
    /// Image file (or, with --force, a device).
    pub source: String,
}

/// Arguments of `list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Image file (or, with --force, a device).
    pub source: String,
    /// Also show existing (not deleted) files.
    #[arg(long)]
    pub all: bool,
    /// Category filter.
    #[command(flatten)]
    pub filter: TypeFilter,
    /// Print machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

/// Arguments of `undelete`.
#[derive(Debug, Args)]
pub struct UndeleteArgs {
    /// Image file (or, with --force, a device).
    pub source: String,
    /// Folder to write recovered files to (must not be on the card).
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    /// Category filter.
    #[command(flatten)]
    pub filter: TypeFilter,
    /// Also try to recover files whose first cluster was reused (usually garbage).
    #[arg(long)]
    pub include_overwritten: bool,
}

/// Arguments of `carve`.
#[derive(Debug, Args)]
pub struct CarveArgs {
    /// Image file (or, with --force, a device).
    pub source: String,
    /// Folder to write carved files to (must not be on the card).
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    /// Category filter.
    #[command(flatten)]
    pub filter: TypeFilter,
    /// Scan the whole source, not just the free (unallocated) clusters.
    #[arg(long)]
    pub all_space: bool,
    /// Maximum size of a carved photo.
    #[arg(long, value_name = "SIZE", value_parser = parse_size, default_value = "64M")]
    pub max_photo: u64,
    /// Maximum size of a carved video.
    #[arg(long, value_name = "SIZE", value_parser = parse_size, default_value = "4G")]
    pub max_video: u64,
    /// Maximum size of a carved audio file.
    #[arg(long, value_name = "SIZE", value_parser = parse_size, default_value = "1G")]
    pub max_audio: u64,
    /// Frame rate assumed for rebuilt AVI videos whose header was lost
    /// (ignored with --avi-reference).
    #[arg(long, value_name = "FPS", default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=240))]
    pub avi_fps: u32,
    /// A working AVI recorded by the same camera with the same settings; its
    /// header (codec, frame size, frame rate, audio format) is used to rebuild
    /// AVI videos whose header was lost.
    #[arg(long, value_name = "FILE")]
    pub avi_reference: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_type_list_and_globals() {
        let cli = Cli::try_parse_from([
            "sdrescue",
            "list",
            "card.img",
            "--type",
            "photo,audio",
            "--offset",
            "1M",
            "-v",
        ])
        .unwrap();
        assert_eq!(cli.global.offset, Some(1 << 20));
        assert!(cli.global.verbose);
        let Command::List(a) = cli.command else {
            panic!("expected list")
        };
        assert_eq!(a.filter.types, vec![Category::Photo, Category::Audio]);
        assert!(a.filter.accepts(Some(Category::Audio)));
        assert!(!a.filter.accepts(Some(Category::Video)));
        assert!(!a.filter.accepts(None));
    }
}
