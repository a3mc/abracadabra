use std::io::IsTerminal;
use std::process::ExitCode;

use abracadabra::cli::Cli;
use abracadabra::{runner, tui};
use clap::Parser as _;

fn main() -> ExitCode {
    let args = Cli::parse();
    // TUI is the default; fall back to text when stdout isn't a terminal
    // (pipe / redirect) or when the user explicitly asks for --text.
    let want_tui = !args.text && std::io::stdout().is_terminal();

    match runner::run(args.path) {
        Ok((state, stats)) => {
            if want_tui {
                if let Err(e) = tui::run(&state, args.bucket) {
                    eprintln!("abracadabra: TUI error: {e}");
                    return ExitCode::FAILURE;
                }
            } else {
                runner::print_summary(&state, &stats);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("abracadabra: {e}");
            ExitCode::FAILURE
        }
    }
}
