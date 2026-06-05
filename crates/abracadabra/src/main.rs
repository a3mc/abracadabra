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

    // Detect terminal truecolor capability once, before any frame
    // renders. Terminals that misparse 24-bit SGR sequences (notably
    // macOS Terminal.app) trigger the 6×6×6 cube fallback so chip
    // backgrounds and tx-pressure gradients render correctly.
    // `--no-truecolor` forces the fallback; `--force-truecolor` skips
    // the env-var ladder when the operator knows their terminal is
    // capable. clap rejects passing both at once.
    abracadabra::tui::truecolor::init(args.no_truecolor, args.force_truecolor);

    // Verify file accessibility once, up front, before either
    // classification or parsing. Both subsystems would emit their own
    // error otherwise — surface one clean line instead.
    if let Err(e) = std::fs::metadata(&args.path) {
        eprintln!(
            "abracadabra: cannot read log file {}: {e}",
            args.path.display()
        );
        return ExitCode::FAILURE;
    }

    // Classify activity *before* taking the terminal so the ~2s
    // size-delta poll does not block inside the alt screen. Only
    // matters for TUI mode; --text consumers skip the poll. classify()
    // can still fail here on a rare race (file removed between the
    // metadata check and the poll); treat that as static and let
    // runner::run produce the authoritative error.
    let activity = if want_tui {
        detect::classify(&args.path)
            .unwrap_or(detect::Activity::Static(detect::StaticReason::NoSizeGrowth))
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
