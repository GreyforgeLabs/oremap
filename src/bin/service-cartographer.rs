//! Deprecated `service-cartographer` alias for `oremap`.
//!
//! The tool was renamed to `oremap` in 0.4.0. This alias keeps existing
//! scripts and cron entries working for one release: it prints a one-line
//! note to stderr, then behaves exactly like `oremap`.

use std::io::Write;

fn main() {
    let _ = writeln!(
        std::io::stderr(),
        "service-cartographer: renamed to oremap; this alias will be removed in the next release"
    );
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    std::process::exit(oremap::cli::main(args));
}
