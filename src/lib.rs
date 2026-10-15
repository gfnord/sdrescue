//! sdrescue: recover deleted photos, videos and audio from FAT-formatted SD cards.
//!
//! Layout:
//! - [`source`]: the read-only [`source::Source`] trait and image files.
//! - [`device`]: device names, raw device access, "never work on the card" checks.
//! - [`partition`]: MBR/GPT parsing and locating the filesystem.
//! - [`fat`]: FAT12/16/32 boot sector, FAT table and directory walk.
//! - [`recover`]: recoverability status and undelete copying.
//! - [`carve`]: signature-based carving with one module per format.
//! - [`imaging`]: byte-for-byte imaging with bad-sector handling.
//! - [`commands`]: the subcommands, driven by [`cli`].

#![warn(missing_docs)]

pub mod carve;
pub mod cli;
pub mod commands;
pub mod device;
pub mod fat;
pub mod filetype;
pub mod imaging;
pub mod output;
pub mod partition;
pub mod recover;
pub mod source;
pub mod util;
