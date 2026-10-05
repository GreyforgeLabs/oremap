# Changelog

## 0.4.0 - 2026-10-05

### Renamed

- Renamed from service-cartographer to oremap. This is the first oremap release; 0.3.0 and earlier were released as `service-cartographer`. The crate, binary, and library are now `oremap`, the repository moves to `GreyforgeLabs/oremap`, help and error text use the program name `oremap`, and the Markdown report title is now `# oremap Report`.
- The README banner is now `docs/assets/openforge-oremap.webp`; the old `openforge-service-cartographer.webp` was removed.
- JSON reports keep the schema identifier `service-cartographer.report.v1`, so existing report consumers keep working unchanged.
- Deprecated alias: `cargo install` also installs a `service-cartographer` binary for this release only. It prints `service-cartographer: renamed to oremap; this alias will be removed in the next release` to stderr, then behaves exactly like `oremap` (same output, help text, and exit status). Update scripts and cron entries to `oremap`.

### Changed

- Rewrote the tool in Rust (edition 2024, MSRV 1.88). Apart from the program name and Markdown title (see Renamed), the 0.3.0 command line, help and error text, argparse abbreviation rules, report formats (JSON keys and layout, Markdown, spreadsheet-safe CSV), privacy and redaction rules, incomplete-scan reporting, classifier, report file mode, and exit codes are unchanged. Differential tests compare the binary with the Python 0.3.0 reference on help and usage errors at 13 terminal widths, on fixture scans, and on real-host scans. Apart from `generated_at`, the renamed program name and Markdown title, and the deviations below, the output was identical byte for byte.
- Measured on the reference host (`docs/benchmarks.md`): `--version` 56.56 ms to 1.93 ms; a filesystem-only scan 64.48 ms to 3.68 ms; a full default scan 265.75 ms to 137.12 ms. Peak RSS drops from 17,864 KiB to 2,664 KiB for `--version` and from 19,140 KiB to 6,040 KiB for the default scan, where the external tools now set the peak. The installed artifact is one 852,144-byte stripped binary with no interpreter dependency (plus, for this release only, the deprecated alias binary).
- Independent read-only queries now run at the same time: the `systemctl` version probe and both unit listings, the `crontab` probe and listing, the `git` probe and repository discovery, and per-repository `git` queries (up to 8 workers). Results merge in the original order, so items and warnings are unchanged.
- Python packaging, the pytest suite, and the Python CI were replaced by Cargo, Rust unit and integration tests, and a Rust CI job (fmt, clippy `-D warnings`, `test --locked`), plus an MSRV job that builds and tests with Rust 1.88.

### Intentional deviations from 0.3.0

- `git` subprocesses run with `GIT_OPTIONAL_LOCKS=0`, so `git status --porcelain` can no longer refresh and rewrite a scanned repository's index. Reported branch, dirty count, and status are unchanged.
- Several inputs crashed 0.3.0 with a Python traceback. 0.4.0 reports them instead:
  - A wrapper directory that exists but cannot be listed is recorded as an `inventory-error: <dir>: <reason>` warning. The report is marked incomplete and the exit status is 1.
  - A `stat` failure other than "not found" while reading a modification time (for example a unit file that is a symlink loop) leaves `last_activity` empty. A wrapper that disappears between listing and `stat` is skipped.
  - An unknown `~user` root (or a `HOME` that cannot be expanded), a report file that cannot be written, and a stdout that cannot be written (for example a closed pipe) each print one line, `oremap: error: <reason>`, on stderr and exit with status 1. Python printed a traceback, or exited 120 when only the final flush failed.
  - Non-finite or out-of-range `--timeout` values (`inf`, `nan`, `1e300`) mean "no deadline". Python raised `OverflowError` or `ValueError` once a subprocess ran.
- Bytes that are not valid UTF-8 in file names, arguments, the hostname, or tool output are replaced with U+FFFD. 0.3.0 emitted surrogate escapes such as `\udcff` in JSON and crashed when writing Markdown or CSV, or when a tool printed invalid UTF-8.
- Help and usage text match Python 3.14 argparse, including `COLUMNS` and terminal-width wrapping, but are never colorized. Python 3.14 colorizes on a TTY; Python 3.11-3.13 did not.
- `~user` paths are resolved from `/etc/passwd` only, not other NSS sources.

## 0.3.0 - 2026-09-27

- Preserve the user bus address and runtime directory for `systemctl --user` while keeping other subprocess environments minimal.
- Mark reports incomplete and exit 1 when a requested collector emits warnings or fails.
- Stop inferring retirement from old file-change or commit timestamps; only operator-supplied retire keywords can create retirement candidates, and active or enabled items remain keep candidates.
- Atomically write private (`0600`) report files, including replacements of existing reports.

## 0.2.0 - 2026-08-28

- Resolve external inventory tools only from reviewed absolute paths and record versions.
- Run probes with a minimal environment and distinct timeout, execution-error, and nonzero states.
- Neutralize spreadsheet-formula prefixes in every CSV string cell while keeping JSON exact.
- Recognize worktree and bare Git layouts without following marker symlinks.
- Bound env reads, reject env symlinks, and record disappearance/permission races without aborting.

## 0.1.0 - 2026-05-02

- Initial public release candidate.
- Added read-only collectors for systemd units, crontab entries, executable wrappers, Git repositories, and env-file metadata.
- Added Markdown, JSON, and CSV report formats.
- Added privacy defaults for hostname, home paths, cron previews, and env files.
- Added test suite, CI workflow, setup script, and release documentation.
