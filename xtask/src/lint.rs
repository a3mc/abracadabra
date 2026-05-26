#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI build tool requires console output"
)]

use std::collections::HashMap;
use std::process::Command;

/// Count warning and error lines in clippy text output.
pub fn count_issues(output: &str) -> (usize, usize) {
    let warnings = output.lines().filter(|l| l.starts_with("warning:")).count();
    let errors = output.lines().filter(|l| l.starts_with("error:")).count();
    (warnings, errors)
}

/// Parse and summarize clippy text output by lint type.
pub fn summarize_clippy_output(output: &str, scope: &str) {
    let mut lint_counts: HashMap<String, Vec<String>> = HashMap::new();

    for line in output.lines() {
        if let Some(stripped) = line.strip_prefix("warning: ") {
            let message = stripped
                .split('\n')
                .next()
                .unwrap_or(stripped)
                .trim()
                .to_string();

            let location = if let Some(next_line) = output.lines().skip_while(|l| l != &line).nth(1)
            {
                if next_line.trim().starts_with("-->") {
                    next_line
                        .trim()
                        .strip_prefix("-->")
                        .unwrap_or("")
                        .trim()
                        .to_string()
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            lint_counts.entry(message).or_default().push(location);
        }
    }

    if lint_counts.is_empty() {
        return;
    }

    println!("\n  Lint Summary ({scope}):");
    println!("{}", "=".repeat(80));

    let mut lint_vec: Vec<_> = lint_counts.iter().collect();
    lint_vec.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));

    for (message, locations) in lint_vec.iter().take(20) {
        let count = locations.len();
        println!("\n  [{count:>3}] {message}");

        for loc in locations.iter().take(3) {
            if !loc.is_empty() {
                println!("        {loc}");
            }
        }

        if locations.len() > 3 {
            println!("        ... and {} more", locations.len() - 3);
        }
    }

    let total_count: usize = lint_vec.iter().map(|(_, v)| v.len()).sum();
    let displayed = lint_vec.len().min(20);

    if lint_vec.len() > 20 {
        println!("\n  ... and {} more lint types", lint_vec.len() - 20);
    }

    println!("\n{}", "=".repeat(80));
    println!("Total: {total_count} warnings across {displayed} lint types");
}

pub fn run_lint() -> Result<(), String> {
    println!("--- Clippy: all targets ---\n");

    let output = Command::new("cargo")
        .args([
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--message-format=short",
            "--",
            "-D",
            "warnings",
        ])
        .output()
        .map_err(|e| format!("Failed to run clippy: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    print!("{combined}");

    let (warnings, errors) = count_issues(&combined);
    summarize_clippy_output(&combined, "All Targets");

    if errors > 0 {
        println!("\n[FAIL] {errors} errors found");
        return Err("Clippy found errors".to_string());
    }

    if warnings > 0 {
        println!("\n[WARN] {warnings} warnings found");
    } else {
        println!("\n[PASS] No warnings");
    }

    Ok(())
}

pub fn run_lint_production() -> Result<(), String> {
    println!("--- Clippy: production code (excludes xtask) ---\n");

    let output = Command::new("cargo")
        .args([
            "clippy",
            "--workspace",
            "--exclude",
            "xtask",
            "--bins",
            "--message-format=short",
        ])
        .output()
        .map_err(|e| format!("Failed to run clippy: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    print!("{combined}");

    let (warnings, errors) = count_issues(&combined);
    summarize_clippy_output(&combined, "Production Code");

    if errors > 0 {
        println!("\n[FAIL] {errors} errors found");
        return Err("Clippy found errors".to_string());
    }

    if warnings > 0 {
        println!("\n[WARN] {warnings} warnings found");
    } else {
        println!("\n[PASS] Production code: zero warnings");
    }

    Ok(())
}

pub fn run_tests() -> Result<(), String> {
    println!("--- Test suite ---\n");

    let status = Command::new("cargo")
        .args(["test", "--workspace", "--all-features"])
        .status()
        .map_err(|e| format!("Failed to run tests: {e}"))?;

    if !status.success() {
        return Err("Tests failed".to_string());
    }

    println!("\n[PASS] All tests passed");
    Ok(())
}

pub fn run_check() -> Result<(), String> {
    println!("--- Quick compile check ---\n");

    let status = Command::new("cargo")
        .args(["check", "--workspace", "--all-features"])
        .status()
        .map_err(|e| format!("Failed to run check: {e}"))?;

    if !status.success() {
        return Err("Check failed".to_string());
    }

    println!("\n[PASS] Compilation successful");
    Ok(())
}
