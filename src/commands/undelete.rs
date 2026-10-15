//! `sdrescue undelete` (in progress).

use anyhow::{Result, bail};

use super::Outcome;
use crate::cli::{GlobalOpts, UndeleteArgs};

/// Runs `undelete`.
pub fn run(_a: &UndeleteArgs, _g: &GlobalOpts) -> Result<Outcome> {
    bail!("not implemented yet")
}
