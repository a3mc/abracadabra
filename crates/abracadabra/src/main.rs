use std::io::IsTerminal;
use std::process::ExitCode;

use abracadabra::cli::Cli;
use abracadabra::live::detect;
use abracadabra::{runner, tui};
use clap::Parser as _;

fn main() -> ExitCode {
    let args = Cli::parse();
    // TUI is the default; fall back to text when stdout isn't a terminal
    // (pipe / redirect) or when the user explicitly asks for --text.
    let want_tui = !args.text && std::io::stdout().is_terminal();

    // Classify activity *before* taking the terminal so the ~2s
    // size-delta poll does not block inside the alt screen. Only matters
    // for TUI mode; --text consumers ignore it.
    let activity = if want_tui {
        match detect::classify(&args.path) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("abracadabra: activity detection failed ({e}); assuming static");
                detect::Activity::Static(detect::StaticReason::NoSizeGrowth)
            }
        }
    } else {
        detect::Activity::Static(detect::StaticReason::NoSizeGrowth)
    };

    match runner::run(args.path) {
        Ok((state, stats)) => {
            if want_tui {
                if let Err(e) = tui::run(&state, args.bucket, activity) {
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
