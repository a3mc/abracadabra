use std::io::IsTerminal;
use std::process::ExitCode;

use abracadabra::cli::Cli;
use abracadabra::live::detect;
use abracadabra::source::LogSource;
use abracadabra::{runner, tui};
use clap::Parser as _;

fn main() -> ExitCode {
    let args = Cli::parse();
    let want_tui = !args.text && std::io::stdout().is_terminal();

    abracadabra::tui::truecolor::init(args.no_truecolor, args.force_truecolor);

    // Build the log source from the mutually-exclusive path / unit args.
    // clap's ArgGroup already guarantees exactly one is set.
    let source = match (args.path, args.unit) {
        (Some(p), None) => LogSource::File(p),
        (None, Some(u)) => LogSource::Journal {
            unit: u,
            since: args.since,
        },
        _ => unreachable!("clap ArgGroup enforces exactly one of path/unit"),
    };

    // For file sources: verify accessibility up front so we surface one
    // clean error instead of two (stat + open). For journal sources: no
    // pre-flight — journalctl availability is discovered at spawn time.
    if let LogSource::File(ref p) = source {
        if let Err(e) = std::fs::metadata(p) {
            eprintln!("abracadabra: cannot read log file {}: {e}", p.display());
            return ExitCode::FAILURE;
        }
    }

    // Classify activity before taking the terminal so the ~2s size-delta
    // poll does not block inside the alt screen. Journal sources are
    // always live; file sources use the existing detector.
    let activity = if want_tui {
        match &source {
            LogSource::Journal { .. } => detect::Activity::Active,
            LogSource::File(p) => detect::classify(p)
                .unwrap_or(detect::Activity::Static(detect::StaticReason::NoSizeGrowth)),
        }
    } else {
        detect::Activity::Static(detect::StaticReason::NoSizeGrowth)
    };

    match runner::run(source) {
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
