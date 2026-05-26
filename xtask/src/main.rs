#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI build tool requires console output"
)]

mod dashboard;
mod detail;
mod lint;
mod loc_report;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let subcommand = args.get(1).map(String::as_str);

    let result = match subcommand {
        Some("lint") => lint::run_lint(),
        Some("lint-prod") => lint::run_lint_production(),
        Some("test") => lint::run_tests(),
        Some("check") => lint::run_check(),
        Some("dashboard") => dashboard::run_dashboard(),
        Some("dashboard-prod") => dashboard::run_dashboard_production(),
        Some("detail") => detail::run_detail(args.get(2).map(String::as_str)),
        Some("detail-prod") => detail::run_detail_production(args.get(2).map(String::as_str)),
        Some("loc-report") => loc_report::run_loc_report(),
        Some("fix") => {
            eprintln!(
                "[DENIED] Automatic fixes are disabled. Every change must be manually reviewed."
            );
            eprintln!("         See: CLAUDE.md, section 'Denied Patterns'");
            Err("cargo xtask fix is disabled".to_string())
        }
        _ => {
            print_help();
            Ok(())
        }
    };

    if let Err(e) = result {
        eprintln!("[FAIL] {e}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "Usage: cargo xtask <COMMAND>

Commands:
  lint              Clippy on all workspace targets
  lint-prod         Clippy on production code only (excludes xtask)
  test              Full workspace test suite
  check             Quick compile check
  dashboard         Clippy dashboard grouped by lint / crate / file (all targets)
  dashboard-prod    Dashboard for production code only
  detail [LINT]     Detailed warning report with file:line (all targets)
  detail-prod [LINT]  Detailed warning report for production only
  loc-report        File size report. Warns at >500 LOC, strong-warns at >800.

Run from the workspace root.

Examples:
  cargo xtask lint-prod
  cargo xtask dashboard-prod
  cargo xtask detail clippy::unwrap_used
  cargo xtask loc-report"
    );
}
