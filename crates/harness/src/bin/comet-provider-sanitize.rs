use std::path::PathBuf;

use comet_harness::capture::{
    render_escaped_paths_report, render_novel_paths_report, sanitize_dir,
};

const HELP: &str = r#"Sanitize one ignored raw provider capture into ignored staging.

Usage:
  comet-provider-sanitize <RAW_CAPTURE_DIR> <STAGING_DIR>

The staging path must be beneath a .comet-provider-captures/staging directory.
"#;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(raw_dir) = args.next() else {
        exit_with_help();
    };
    if raw_dir == "-h" || raw_dir == "--help" {
        print!("{HELP}");
        return;
    }
    let Some(staging_dir) = args.next() else {
        exit_with_help();
    };
    if args.next().is_some() {
        exit_with_help();
    }

    let raw_dir = PathBuf::from(raw_dir);
    let staging_dir = PathBuf::from(staging_dir);
    match sanitize_dir(&raw_dir, &staging_dir) {
        Ok(report) => {
            println!(
                "Sanitized capture written to {} ({} event bytes, {} manifest bytes).",
                staging_dir.display(),
                report.events_bytes.len(),
                report.manifest_bytes.len()
            );
            println!();
            println!("{}", render_novel_paths_report(&report.novel_paths));
            println!();
            println!("{}", render_escaped_paths_report(&report.escaped_paths));
        }
        Err(error) => {
            eprintln!("Sanitization failed. {error}");
            std::process::exit(2);
        }
    }
}

fn exit_with_help() -> ! {
    eprintln!("Choose one raw capture directory and one staging directory.\n\n{HELP}");
    std::process::exit(2)
}
