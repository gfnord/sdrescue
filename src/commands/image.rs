//! `sdrescue image` (in progress).

use anyhow::{Result, bail};

use super::Outcome;
use crate::cli::{GlobalOpts, ImageArgs};

/// Runs `image`.
pub fn run(_a: &ImageArgs, _g: &GlobalOpts) -> Result<Outcome> {
    bail!("not implemented yet")
}
