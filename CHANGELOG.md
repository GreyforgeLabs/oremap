# Changelog

## Unreleased

### Fixed

- `git status` no longer runs filter drivers defined in a scanned repository's configuration. A `clean` or `process` filter assigned through `.gitattributes` or `.git/info/attributes` used to run whenever a file's cached stat data was stale, so scanning a repository with a crafted `.git/config` executed its commands, and a configured `git lfs` filter ran and could write under `.git/lfs`. Each configured driver is now switched off with `-c filter.<name>.clean=`, `-c filter.<name>.process=`, and `-c filter.<name>.required=false`. The drivers are listed with `git config --get-regexp`, which runs only when the repository's configuration (including a linked worktree's common configuration) or `/etc/gitconfig` mentions `filter` or `include`; on the 62 repositories of the reference workspace that is none, so reports and process counts are unchanged there. A driver name that cannot be passed back with `-c` (it contains `=`), or a failed driver query, skips `git status` and records a warning. A file whose stat data is stale and whose content differs from the index only by what a disabled filter would have changed is now counted as dirty. Git still checks submodule work trees with their own configuration.
- External commands read standard input from `/dev/null` instead of sharing oremap's.

## 0.4.1 - 2026-10-14

### Fixed

- Failed, activating, and not-found units keep their `active` state. `systemctl list-units` marks them with `●` (`*` in the C locale), and those lines were skipped, so a failed unit showed an empty state and a not-found unit was missing from the report. On the reference host this restores 9 unit states and 11 discovered units.
- `git status` can no longer start a background process: every `git` call passes `-c core.fsmonitor=`, so a repository with `core.fsmonitor=true` no longer makes the scan launch `git fsmonitor--daemon` and write its state under `.git`.
- Bare repositories are reported with status `bare` instead of a failed `git status` warning, so a scan that finds one is no longer marked incomplete (exit 1).
- A unit directory that exists but cannot be listed is recorded as an `inventory-error` warning and marks the report incomplete, as wrapper directories already did. It used to be skipped silently.
- `--include-env-names` never lists text from inside a value. A line of a quoted PEM block that looked like `BASE64TEXT=` was listed as a variable name; lines inside `$'...'` values and lines continued with a trailing backslash are treated as part of the value too.
- `export NAME=value` lines (the usual `.envrc` form) count as variables.
- Cron command previews end at the first unescaped `%`. Cron feeds the rest of the line to the command's standard input, so it is shown as `%<redacted>`.
- Markdown reports write line breaks inside a cell or warning as `\n` and `\r`. A file name containing a line break split its table row in two.
- Wrapper `#!` detection reads at most one 8 KiB block from a file that cannot start with `#!`, and at most 64 KiB from one that does. A large executable without line breaks was read whole: 50 MB took 162 ms and 100 MB of memory, now 2.7 ms and 2.8 MB.

### Faster

- One `git status --porcelain=v2 --branch` call reports both the branch and the changes, replacing the separate `git rev-parse` (still used for bare repositories and when `status` fails). A scan of 64 repositories runs 129 processes instead of 193 and finishes in 254 ms instead of 316 ms; a default scan runs 16 instead of 22. Reported branches and change counts are unchanged.
- Repository discovery checks for `.git`, `HEAD`, and `objects` in the directory listing it already has, and the walker no longer calls `stat` on symlinks or `lstat` on every sub-directory. Discovery over `/usr/share` (about 9,000 directories, 46,000 symlinks) takes 163 ms instead of 328 ms; an env-file walk of about 26,000 directories takes 183 ms instead of 200 ms.

### Added

- `scripts/build-release.sh` builds the release binaries and a reproducible `dist/oremap-<version>-<target>.tar.gz` with a SHA-256 checksum file. Build paths are remapped, so the archive is the same from any checkout location and the binaries name no local user.

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
