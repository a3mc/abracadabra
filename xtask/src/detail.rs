#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI build tool requires console output"
)]

use std::collections::HashMap;
use std::process::{Command, Stdio};

use crate::dashboard::{Diagnostic, DiagnosticTarget};

#[derive(Debug)]
struct WarningDetail {
    file: String,
    line: usize,
    lint_code: String,
    message: String,
    is_test: bool,
}

fn collect_warnings(
    stdout: &str,
    workspace_root_str: &str,
    filter_lint: Option<&str>,
    track_test_flag: bool,
) -> Result<Vec<WarningDetail>, String> {
    let mut warnings: Vec<WarningDetail> = Vec::new();

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
                    if let Some(filter) = filter_lint {
                        if !code.code.contains(filter) {
                            continue;
                        }
                    }

                    if let Some(span) = message.spans.first() {
                        if span.file_name.starts_with('/')
                            && !span.file_name.starts_with(workspace_root_str)
                        {
                            continue;
                        }

                        let file_path = span
                            .file_name
                            .strip_prefix(&format!("{workspace_root_str}/"))
                            .unwrap_or(&span.file_name)
                            .to_string();

                        let is_test = if track_test_flag {
                            file_path.contains("/tests/")
                                || file_path.ends_with("_tests.rs")
                                || file_path.ends_with("_test.rs")
                                || diagnostic
                                    .target
                                    .as_ref()
                                    .is_some_and(|t: &DiagnosticTarget| t.name.ends_with("_tests"))
                        } else {
                            false
                        };

                        let msg = message
                            .rendered
                            .as_deref()
                            .unwrap_or("")
                            .lines()
                            .find(|l| l.starts_with("warning:"))
                            .unwrap_or("")
                            .strip_prefix("warning: ")
                            .unwrap_or("")
                            .to_string();

                        warnings.push(WarningDetail {
                            file: file_path,
                            line: span.line_start,
                            lint_code: code.code.clone(),
                            message: msg,
                            is_test,
                        });
                    }
                }
            }
        }
    }

    warnings.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    Ok(warnings)
}

fn print_detail_report(warnings: &[WarningDetail], show_test_split: bool) {
    let mut by_lint: HashMap<String, Vec<&WarningDetail>> = HashMap::new();
    for w in warnings {
        by_lint.entry(w.lint_code.clone()).or_default().push(w);
    }

    let mut lint_list: Vec<_> = by_lint.iter().collect();
    lint_list.sort_by_key(|b| std::cmp::Reverse(b.1.len()));

    for (lint, items) in &lint_list {
        if show_test_split {
            let test_count = items.iter().filter(|w| w.is_test).count();
            let prod_count = items.len() - test_count;
            println!(
                "\n|- {} [{} total: {} prod, {} test]",
                lint,
                items.len(),
                prod_count,
                test_count
            );
        } else {
            println!("\n|- {} [{} occurrences]", lint, items.len());
        }

        for w in *items {
            if show_test_split {
                let marker = if w.is_test { "T" } else { "P" };
                println!("|  [{}] {}:{}", marker, w.file, w.line);
            } else {
                println!("|  {}:{}", w.file, w.line);
            }
            if !w.message.is_empty() {
                println!("|     {}", w.message);
            }
        }
    }

    println!("\n{}", "=".repeat(80));

    if show_test_split {
        let test_count = warnings.iter().filter(|w| w.is_test).count();
        let prod_count = warnings.len() - test_count;
        println!("Total: {} warnings", warnings.len());
        println!("  Production: {prod_count}");
        println!("  Test code:  {test_count}");
    } else {
        println!(
            "Total: {} production warnings across {} lint types",
            warnings.len(),
            by_lint.len()
        );
    }
    println!("  Lint types: {}", by_lint.len());
    println!("{}", "=".repeat(80));

    if warnings.is_empty() {
        if show_test_split {
            println!("\n[PASS] No warnings");
        } else {
            println!("\n[PASS] Production code: zero warnings");
        }
    }
}

fn run_detail_inner(
    scope: &str,
    clippy_args: &[&str],
    filter_lint: Option<&str>,
    show_test_split: bool,
) -> Result<(), String> {
    println!("--- {scope}: Detailed Warning Report ---\n");

    if let Some(lint) = filter_lint {
        println!("Filtering: {lint}\n");
    }

    let workspace_root =
        std::env::current_dir().map_err(|e| format!("Failed to get current directory: {e}"))?;
    let workspace_root_str = workspace_root.to_string_lossy();

    let output = Command::new("cargo")
        .args(clippy_args)
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("Failed to run clippy: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let warnings = collect_warnings(&stdout, &workspace_root_str, filter_lint, show_test_split)?;
    print_detail_report(&warnings, show_test_split);

    Ok(())
}

pub fn run_detail(filter_lint: Option<&str>) -> Result<(), String> {
    run_detail_inner(
        "All targets",
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
        filter_lint,
        true,
    )
}

pub fn run_detail_production(filter_lint: Option<&str>) -> Result<(), String> {
    run_detail_inner(
        "Production Code",
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
        filter_lint,
        false,
    )
}
