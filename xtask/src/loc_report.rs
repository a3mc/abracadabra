#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI build tool requires console output"
)]

use std::fs;
use std::path::{Path, PathBuf};

const WARN_THRESHOLD: usize = 500;
const STRONG_WARN_THRESHOLD: usize = 800;

struct FileLoc {
    path: PathBuf,
    lines: usize,
}

pub fn run_loc_report() -> Result<(), String> {
    println!("--- File LOC report (workspace .rs files) ---\n");

    let workspace_root =
        std::env::current_dir().map_err(|e| format!("Failed to get current directory: {e}"))?;

    let mut files: Vec<FileLoc> = Vec::new();
    collect_rs_files(&workspace_root, &mut files)?;

    files.sort_by_key(|b| std::cmp::Reverse(b.lines));

    let mut warn_count = 0_usize;
    let mut strong_warn_count = 0_usize;

    println!("{:>6}  FILE", "LINES");
    println!("{}", "-".repeat(78));

    for f in &files {
        let rel = f.path.strip_prefix(&workspace_root).unwrap_or(&f.path);
        let tag = match f.lines {
            n if n > STRONG_WARN_THRESHOLD => {
                strong_warn_count += 1;
                "[!!]"
            }
            n if n > WARN_THRESHOLD => {
                warn_count += 1;
                "[! ]"
            }
            _ => "    ",
        };
        println!("{:>6}  {tag} {}", f.lines, rel.display());
    }

    println!("\n{}", "=".repeat(78));
    println!("Total files scanned: {}", files.len());
    println!("  [! ] warn (>{WARN_THRESHOLD} LOC):        {warn_count}");
    println!("  [!!] strong warn (>{STRONG_WARN_THRESHOLD} LOC): {strong_warn_count}");
    println!("{}", "=".repeat(78));

    if strong_warn_count > 0 {
        println!("\n[WARN] {strong_warn_count} file(s) exceed {STRONG_WARN_THRESHOLD} LOC. Consider splitting.");
    } else if warn_count > 0 {
        println!("\n[INFO] {warn_count} file(s) exceed {WARN_THRESHOLD} LOC.");
    } else {
        println!("\n[PASS] All files within size targets.");
    }

    // Always succeed: this is a soft signal, not a build gate.
    Ok(())
}

fn collect_rs_files(dir: &Path, out: &mut Vec<FileLoc>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("dir entry under {}: {e}", dir.display()))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if path.is_dir() {
            if matches!(
                name.as_ref(),
                "target" | ".git" | ".claude" | "audit" | "node_modules"
            ) {
                continue;
            }
            collect_rs_files(&path, out)?;
            continue;
        }

        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }

        let content =
            fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let lines = content.lines().count();
        out.push(FileLoc { path, lines });
    }

    Ok(())
}
