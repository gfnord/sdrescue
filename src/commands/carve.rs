//! `sdrescue carve` (in progress).

use anyhow::{Result, bail};

use super::Outcome;
use crate::cli::{CarveArgs, GlobalOpts};

/// Runs `carve`.
pub fn run(_a: &CarveArgs, _g: &GlobalOpts) -> Result<Outcome> {
    bail!("not implemented yet")
}
