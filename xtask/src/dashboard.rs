#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI build tool requires console output"
)]

use serde::Deserialize;
use std::collections::HashMap;
use std::process::{Command, Stdio};

#[derive(Debug, Deserialize)]
pub struct Diagnostic {
    pub reason: Option<String>,
    pub message: Option<DiagnosticMessage>,
    pub target: Option<DiagnosticTarget>,
}

#[derive(Debug, Deserialize)]
pub struct DiagnosticTarget {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct DiagnosticMessage {
    pub code: Option<DiagnosticCode>,
    pub rendered: Option<String>,
    pub spans: Vec<DiagnosticSpan>,
    pub level: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DiagnosticCode {
    pub code: String,
}

#[derive(Debug, Deserialize)]
pub struct DiagnosticSpan {
    pub file_name: String,
    pub line_start: usize,
}

fn print_top_items(counts: &HashMap<String, usize>, limit: usize) {
    let mut items: Vec<_> = counts.iter().collect();
    items.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));

    for (name, count) in items.iter().take(limit) {
        println!("  {:3} x {name}", count);
    }

    if items.len() > limit {
        println!("       ... and {} more", items.len() - limit);
    }
}

fn run_dashboard_inner(scope: &str, clippy_args: &[&str]) -> Result<(), String> {
    println!("--- Clippy Dashboard: {scope} ---\n");

    let workspace_root =
        std::env::current_dir().map_err(|e| format!("Failed to get current directory: {e}"))?;
    let workspace_root_str = workspace_root.to_string_lossy();

    let output = Command::new("cargo")
        .args(clippy_args)
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("Failed to run clippy: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lint_counts: HashMap<String, usize> = HashMap::new();
    let mut crate_counts: HashMap<String, usize> = HashMap::new();
    let mut file_counts: HashMap<String, usize> = HashMap::new();

    for line in stdout.lines() {
        if let Ok(diagnostic) = serde_json::from_str::<Diagnostic>(line) {
            if diagnostic.reason.as_deref() != Some("compiler-message") {
                continue;
            }

            if let Some(message) = diagnostic.message {
                if message.level.as_deref() != Some("warning") {
                    continue;
                }

                if let Some(code) = message.code {
                    *lint_counts.entry(code.code.clone()).or_default() += 1;

                    if let Some(target) = diagnostic.target.as_ref() {
                        *crate_counts.entry(target.name.clone()).or_default() += 1;
                    }

                    if let Some(span) = message.spans.first() {
                        if !span.file_name.starts_with('/')
                            || span.file_name.starts_with(workspace_root_str.as_ref())
                        {
                            let file_path = span
                                .file_name
                                .strip_prefix(&format!("{}/", workspace_root_str))
                                .unwrap_or(&span.file_name);
                            *file_counts.entry(file_path.to_string()).or_default() += 1;
                        }
                    }
                }
            }
        }
    }

    let total_warnings: usize = lint_counts.values().sum();

    println!("Top Lints:");
    println!("{}", "-".repeat(60));
    print_top_items(&lint_counts, 15);

    println!("\nTop Crates by Warnings:");
    println!("{}", "-".repeat(60));
    print_top_items(&crate_counts, 10);

    println!("\nTop Files by Warnings:");
    println!("{}", "-".repeat(60));
    print_top_items(&file_counts, 15);

    println!("\n{}", "=".repeat(60));
    println!(
        "Total: {total_warnings} warnings across {} lint types",
        lint_counts.len()
    );
    println!(
        "Affected: {} crates, {} files",
        crate_counts.len(),
        file_counts.len()
    );
    println!("{}", "=".repeat(60));

    if total_warnings > 0 {
        println!("\n[WARN] {total_warnings} warnings found");
    } else {
        let pass_msg = if scope.contains("production") {
            "Production code: zero warnings"
        } else {
            "No warnings"
        };
        println!("\n[PASS] {pass_msg}");
    }

    Ok(())
}

pub fn run_dashboard() -> Result<(), String> {
    run_dashboard_inner(
        "all targets",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--message-format=json",
            "--",
            "-W",
            "clippy::all",
        ],
    )
}

pub fn run_dashboard_production() -> Result<(), String> {
    run_dashboard_inner(
        "production code (excludes xtask)",
        &[
            "clippy",
            "--workspace",
            "--exclude",
            "xtask",
            "--bins",
            "--message-format=json",
            "--",
            "-W",
            "clippy::all",
        ],
    )
}
