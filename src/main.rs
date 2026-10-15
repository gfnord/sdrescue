use std::process::ExitCode;

use clap::Parser;

use sdrescue::cli::Cli;
use sdrescue::commands::{Outcome, run};

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            // --help / --version are not errors; usage errors exit with 1
            // (clap's default of 2 means "nothing found" for sdrescue).
            return if e.use_stderr() {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    match run(cli) {
        Ok(Outcome::Done) => ExitCode::SUCCESS,
        Ok(Outcome::NothingFound) => ExitCode::from(2),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}
