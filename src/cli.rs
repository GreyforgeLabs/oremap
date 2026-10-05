//! Command line interface for oremap.

use std::fs::{self, OpenOptions};
use std::hash::BuildHasher;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;

use crate::argparse::{Action, ActionKind, Exit, Namespace, Parser, SubCommand, Value, ValueType};
use crate::classifier::classify_items;
use crate::collectors::{
    Exec, System, collect_cron, collect_env_files, collect_repositories, collect_systemd,
    collect_wrappers, spawn_or_run, strerror,
};
use crate::formatters::{Format, format_report};
use crate::models::{InventoryItem, InventoryReport, Metadata};
use crate::paths::{self, HomeError};
use crate::privacy::{display_host, redact_command_with_home, redact_path_with_home};
use crate::pyfmt::repr_str;
use crate::timefmt::{Micros, isoformat_utc, now_micros};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROG: &str = "oremap";
/// Report schema identifier. It keeps the pre-rename value on purpose:
/// existing report consumers match it exactly, so the rename to `oremap`
/// does not change it.
pub const SCHEMA: &str = "service-cartographer.report.v1";

/// Build the argparse-compatible parser.
pub fn build_parser() -> Parser {
    use ActionKind::{Append, Store, StoreTrue};
    use ValueType::{Float, Int, Str};

    let mut scan = Parser::new(format!("{PROG} scan"), None);
    scan.add(
        Action::option(&["--format"], "format", Store(Str))
            .choices(&["markdown", "json", "csv"])
            .default(Value::Str("markdown".into())),
    );
    scan.add(
        Action::option(&["--output"], "output", Store(Str))
            .help("write output to this path instead of stdout"),
    );
    scan.add(
        Action::option(&["--focus"], "focus", Append(Str)).help("only keep items matching a token"),
    );
    scan.add(
        Action::option(&["--kind"], "kind", Append(Str))
            .help("only keep item kinds matching this name"),
    );
    scan.add(Action::option(&["--stale-days"], "stale_days", Store(Int)).default(Value::Int(90)));
    scan.add(
        Action::option(&["--timeout"], "timeout", Store(Float))
            .default(Value::Float(5.0))
            .help("seconds per external command"),
    );
    scan.add(
        Action::option(&["--show-hostname"], "show_hostname", StoreTrue)
            .help("include the real local hostname"),
    );
    scan.add(
        Action::option(&["--absolute-paths"], "absolute_paths", StoreTrue)
            .help("do not shorten home paths to ~"),
    );
    scan.add(Action::option(&["--repo-root"], "repo_root", Append(Str)));
    scan.add(Action::option(&["--repo-depth"], "repo_depth", Store(Int)).default(Value::Int(2)));
    scan.add(Action::option(
        &["--wrapper-dir"],
        "wrapper_dir",
        Append(Str),
    ));
    scan.add(Action::option(&["--env-root"], "env_root", Append(Str)));
    scan.add(Action::option(&["--env-depth"], "env_depth", Store(Int)).default(Value::Int(2)));
    scan.add(Action::option(
        &["--include-env-names"],
        "include_env_names",
        StoreTrue,
    ));
    scan.add(
        Action::option(&["--systemd-scope"], "systemd_scope", Store(Str))
            .choices(&["user", "system", "off"])
            .default(Value::Str("user".into()))
            .help("systemd scope to inspect"),
    );
    scan.add(Action::option(&["--unit-dir"], "unit_dir", Append(Str)));
    scan.add(
        Action::option(
            &["--no-systemd-discovered"],
            "no_systemd_discovered",
            StoreTrue,
        )
        .help("only report unit files found on disk, not every systemctl-discovered unit"),
    );
    scan.add(Action::option(&["--no-cron"], "no_cron", StoreTrue));
    scan.add(Action::option(&["--no-env"], "no_env", StoreTrue));
    scan.add(Action::option(&["--no-repos"], "no_repos", StoreTrue));
    scan.add(Action::option(&["--no-wrappers"], "no_wrappers", StoreTrue));
    scan.add(Action::option(
        &["--retire-keyword"],
        "retire_keyword",
        Append(Str),
    ));
    scan.add(Action::option(
        &["--keep-keyword"],
        "keep_keyword",
        Append(Str),
    ));

    let mut parser = Parser::new(
        PROG,
        Some("Inventory local automation surfaces and produce a keep/retire matrix."),
    );
    parser.add(
        Action::option(
            &["--version"],
            "version",
            ActionKind::Version(format!("{PROG} {VERSION}")),
        )
        .help("show program's version number and exit"),
    );
    parser.add(Action {
        option_strings: Vec::new(),
        dest: "command",
        kind: ActionKind::Subparsers,
        choices: Some(vec!["scan"]),
        help: None,
        default: Value::None,
    });
    parser.subcommands.push(SubCommand {
        name: "scan",
        help: Some("scan local services and workflow artifacts"),
        parser: scan,
    });
    parser
}

/// Options for one `scan` run (paths are `pathlib` spellings).
#[derive(Debug, Clone, PartialEq)]
pub struct ScanOptions {
    pub format: Format,
    pub output: Option<String>,
    pub focus: Vec<String>,
    pub kind: Vec<String>,
    pub stale_days: i64,
    pub timeout: f64,
    pub show_hostname: bool,
    pub absolute_paths: bool,
    pub repo_root: Vec<String>,
    pub repo_depth: i64,
    pub wrapper_dir: Vec<String>,
    pub env_root: Vec<String>,
    pub env_depth: i64,
    pub include_env_names: bool,
    pub systemd_scope: String,
    pub unit_dir: Vec<String>,
    pub no_systemd_discovered: bool,
    pub no_cron: bool,
    pub no_env: bool,
    pub no_repos: bool,
    pub no_wrappers: bool,
    pub retire_keyword: Vec<String>,
    pub keep_keyword: Vec<String>,
}

impl ScanOptions {
    fn from_namespace(namespace: &Namespace) -> Self {
        let paths_of = |dest: &str| -> Vec<String> {
            namespace
                .strings(dest)
                .iter()
                .map(|value| paths::normalize(value))
                .collect()
        };
        Self {
            format: namespace
                .str("format")
                .and_then(Format::parse)
                .unwrap_or(Format::Markdown),
            output: namespace.str("output").map(paths::normalize),
            focus: namespace.strings("focus"),
            kind: namespace.strings("kind"),
            stale_days: namespace.int("stale_days"),
            timeout: namespace.float("timeout"),
            show_hostname: namespace.flag("show_hostname"),
            absolute_paths: namespace.flag("absolute_paths"),
            repo_root: paths_of("repo_root"),
            repo_depth: namespace.int("repo_depth"),
            wrapper_dir: paths_of("wrapper_dir"),
            env_root: paths_of("env_root"),
            env_depth: namespace.int("env_depth"),
            include_env_names: namespace.flag("include_env_names"),
            systemd_scope: namespace.str("systemd_scope").unwrap_or("user").to_string(),
            unit_dir: paths_of("unit_dir"),
            no_systemd_discovered: namespace.flag("no_systemd_discovered"),
            no_cron: namespace.flag("no_cron"),
            no_env: namespace.flag("no_env"),
            no_repos: namespace.flag("no_repos"),
            no_wrappers: namespace.flag("no_wrappers"),
            retire_keyword: namespace.strings("retire_keyword"),
            keep_keyword: namespace.strings("keep_keyword"),
        }
    }

    /// Parse `scan` arguments (without the sub-command word).
    pub fn parse(args: &[&str]) -> Result<Self, Exit> {
        let mut argv = vec!["scan".to_string()];
        argv.extend(args.iter().map(|arg| arg.to_string()));
        let namespace = build_parser().parse_args(&argv)?;
        Ok(Self::from_namespace(&namespace))
    }

    fn repo_roots(&self) -> Vec<String> {
        if self.repo_root.is_empty() {
            vec![paths::cwd()]
        } else {
            self.repo_root.clone()
        }
    }

    fn env_roots(&self) -> Vec<String> {
        if self.env_root.is_empty() {
            self.repo_roots()
        } else {
            self.env_root.clone()
        }
    }

    fn wrapper_dirs(&self) -> Vec<String> {
        if !self.wrapper_dir.is_empty() {
            return self.wrapper_dir.clone();
        }
        ["~/bin", "~/.local/bin"]
            .into_iter()
            .filter(|candidate| {
                paths::expanduser(candidate)
                    .is_ok_and(|path| fs::metadata(path).is_ok_and(|m| m.is_dir()))
            })
            .map(String::from)
            .collect()
    }

    fn unit_dirs(&self) -> Vec<String> {
        if !self.unit_dir.is_empty() {
            return self.unit_dir.clone();
        }
        let defaults: &[&str] = if self.systemd_scope == "system" {
            &[
                "/etc/systemd/system",
                "/usr/lib/systemd/system",
                "/lib/systemd/system",
            ]
        } else {
            &[
                "~/.config/systemd/user",
                "~/.local/share/systemd/user",
                "/etc/systemd/user",
            ]
        };
        defaults.iter().map(|dir| dir.to_string()).collect()
    }

    fn scan_roots(&self) -> Result<Vec<String>, HomeError> {
        let mut roots = Vec::new();
        if !self.no_repos {
            roots.extend(self.repo_roots());
        }
        if !self.no_env {
            roots.extend(self.env_roots());
        }
        if !self.no_wrappers {
            roots.extend(self.wrapper_dirs());
        }
        if self.systemd_scope != "off" {
            roots.extend(self.unit_dirs());
        }
        let mut deduped = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for root in roots {
            let key = paths::expanduser(&root)?;
            if seen.insert(key) {
                deduped.push(root);
            }
        }
        Ok(deduped)
    }
}

/// Keep items whose kind is listed and whose text contains a focus token.
pub fn filter_items(
    items: Vec<InventoryItem>,
    focus: &[String],
    kinds: &[String],
) -> Vec<InventoryItem> {
    let focus_terms: Vec<String> = focus.iter().map(|term| term.to_lowercase()).collect();
    let kind_terms: Vec<String> = kinds.iter().map(|term| term.to_lowercase()).collect();
    items
        .into_iter()
        .filter(|item| kind_terms.is_empty() || kind_terms.contains(&item.kind.to_lowercase()))
        .filter(|item| {
            if focus_terms.is_empty() {
                return true;
            }
            let text = item.searchable_text();
            focus_terms.iter().any(|term| text.contains(term.as_str()))
        })
        .collect()
}

/// Apply path shortening and credential redaction to one item.
pub fn redact_item(item: InventoryItem, absolute_paths: bool, home: &str) -> InventoryItem {
    let mut metadata = Metadata::new();
    for (key, value) in item.metadata.iter() {
        let redacted = if key == "command_preview" {
            redact_command_with_home(value, absolute_paths, home)
        } else if key.contains("path") {
            redact_path_with_home(value, absolute_paths, home)
        } else {
            redact_command_with_home(value, absolute_paths, home)
        };
        metadata.set(key, redacted);
    }
    InventoryItem {
        path: redact_path_with_home(&item.path, absolute_paths, home),
        metadata,
        ..item
    }
}

/// Collected inventory before filtering and classification.
#[derive(Debug, Default)]
pub struct Collected {
    pub items: Vec<InventoryItem>,
    pub warnings: Vec<String>,
}

/// Run every requested collector. Independent collectors run concurrently;
/// results are merged in the reference order (systemd, cron, wrappers,
/// repositories, env files) so items and warnings are unchanged.
pub fn collect_all(options: &ScanOptions, exec: &dyn Exec) -> Collected {
    let scope = options.systemd_scope.as_str();
    let unit_dirs = options.unit_dirs();
    let repo_roots = options.repo_roots();
    let env_roots = options.env_roots();
    let wrapper_dirs = options.wrapper_dirs();
    thread::scope(|s| {
        let systemd = (scope != "off").then(|| {
            spawn_or_run(s, || {
                collect_systemd(
                    exec,
                    scope,
                    &unit_dirs,
                    !options.no_systemd_discovered,
                    options.timeout,
                )
            })
        });
        let cron = (!options.no_cron).then(|| {
            spawn_or_run(s, || {
                collect_cron(exec, options.timeout, options.absolute_paths)
            })
        });
        let repos = (!options.no_repos).then(|| {
            spawn_or_run(s, || {
                collect_repositories(exec, &repo_roots, options.repo_depth, options.timeout)
            })
        });

        let mut wrapper_warnings = Vec::new();
        let wrapper_items = if options.no_wrappers {
            Vec::new()
        } else {
            collect_wrappers(&wrapper_dirs, &mut wrapper_warnings)
        };
        let mut env_warnings = Vec::new();
        let env_items = if options.no_env {
            Vec::new()
        } else {
            collect_env_files(
                &env_roots,
                options.env_depth,
                options.include_env_names,
                &mut env_warnings,
            )
        };

        let mut collected = Collected::default();
        let mut absorb = |result: Option<(Vec<InventoryItem>, Vec<String>)>| {
            if let Some((items, warnings)) = result {
                collected.items.extend(items);
                collected.warnings.extend(warnings);
            }
        };
        absorb(systemd.map(|handle| handle.join().unwrap_or_default()));
        absorb(cron.map(|handle| handle.join().unwrap_or_default()));
        absorb(Some((wrapper_items, wrapper_warnings)));
        absorb(repos.map(|handle| handle.join().unwrap_or_default()));
        absorb(Some((env_items, env_warnings)));
        collected
    })
}

/// Filter, classify, redact, and sort collected inventory into a report.
pub fn build_report(
    options: &ScanOptions,
    collected: Collected,
    roots: &[String],
    now: Micros,
    hostname: &str,
    home: &str,
) -> InventoryReport {
    let mut items = filter_items(collected.items, &options.focus, &options.kind);
    classify_items(
        &mut items,
        options.stale_days,
        &options.retire_keyword,
        &options.keep_keyword,
        now,
    );
    let mut items: Vec<InventoryItem> = items
        .into_iter()
        .map(|item| redact_item(item, options.absolute_paths, home))
        .collect();
    items.sort_by(|a, b| {
        (&a.suggested_action, &a.kind, &a.name, &a.path).cmp(&(
            &b.suggested_action,
            &b.kind,
            &b.name,
            &b.path,
        ))
    });
    let complete = collected.warnings.is_empty();
    InventoryReport {
        schema: SCHEMA.to_string(),
        generated_at: isoformat_utc(now).unwrap_or_default(),
        host: display_host(hostname, options.show_hostname),
        roots: roots
            .iter()
            .map(|root| redact_path_with_home(root, options.absolute_paths, home))
            .collect(),
        items,
        warnings: collected
            .warnings
            .iter()
            .map(|warning| redact_command_with_home(warning, options.absolute_paths, home))
            .collect(),
        complete,
    }
}

fn hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}

fn temp_suffix(attempt: u64) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut value = std::collections::hash_map::RandomState::new().hash_one((
        attempt,
        nanos,
        std::process::id(),
    ));
    (0..8)
        .map(|_| {
            let index = (value % ALPHABET.len() as u64) as usize;
            value /= ALPHABET.len() as u64;
            ALPHABET[index] as char
        })
        .collect()
}

fn describe_io_error(error: &io::Error, path: &Path) -> String {
    match error.raw_os_error() {
        Some(code) => format!(
            "[Errno {code}] {}: {}",
            strerror(error),
            repr_str(&path.to_string_lossy())
        ),
        None => error.to_string(),
    }
}

fn describe_stream_error(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => format!("[Errno {code}] {}", strerror(error)),
        None => error.to_string(),
    }
}

/// Atomically publish `rendered` at `spelling` with mode 0600: write a
/// `mkstemp`-style sibling, fsync it, and rename it over the target.
pub fn write_private_report(spelling: &str, rendered: &str) -> Result<(), String> {
    let target = PathBuf::from(spelling);
    let parent = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&parent).map_err(|error| describe_io_error(&error, &parent))?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut created = None;
    for attempt in 0..10_000u64 {
        let candidate = parent.join(format!(".{name}.{}", temp_suffix(attempt)));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&candidate)
        {
            Ok(file) => {
                created = Some((file, candidate));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(describe_io_error(&error, &candidate)),
        }
    }
    let Some((mut file, temporary)) = created else {
        return Err("No usable temporary file name found".to_string());
    };
    let result = file
        .write_all(rendered.as_bytes())
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all())
        .map_err(|error| describe_io_error(&error, &temporary))
        .and_then(|()| {
            drop(file);
            fs::rename(&temporary, &target).map_err(|error| describe_io_error(&error, &target))
        });
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Run a scan with the given collectors and write the report. Returns the
/// exit status (0 complete, 1 incomplete or failed to write).
pub fn scan_with(
    options: &ScanOptions,
    exec: &dyn Exec,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let now = now_micros();
    let roots = match options.scan_roots() {
        Ok(roots) => roots,
        Err(error) => {
            let _ = writeln!(stderr, "{PROG}: error: {error}");
            return 1;
        }
    };
    let collected = collect_all(options, exec);
    let host = if options.show_hostname {
        hostname()
    } else {
        String::new()
    };
    let report = build_report(options, collected, &roots, now, &host, &paths::home());
    let rendered = format_report(&report, options.format);
    match &options.output {
        Some(path) => {
            if let Err(message) = write_private_report(path, &rendered) {
                let _ = writeln!(stderr, "{PROG}: error: {message}");
                return 1;
            }
        }
        None => {
            // Like the Python version, an undeliverable report (for example a
            // closed pipe) fails the run, but with one line instead of a
            // traceback.
            if let Err(error) = stdout
                .write_all(rendered.as_bytes())
                .and_then(|()| stdout.flush())
            {
                let _ = writeln!(stderr, "{PROG}: error: {}", describe_stream_error(&error));
                return 1;
            }
        }
    }
    if report.complete { 0 } else { 1 }
}

/// Entry point used by the binary; returns the process exit status.
pub fn run(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let parser = build_parser();
    let mut namespace = match parser.parse_args(args) {
        Ok(namespace) => namespace,
        Err(exit) => {
            let _ = stdout.write_all(exit.stdout.as_bytes());
            let _ = stderr.write_all(exit.stderr.as_bytes());
            return exit.code;
        }
    };
    if namespace.str("command").is_none() {
        match parser.parse_args(&["scan".to_string()]) {
            Ok(reparsed) => namespace = reparsed,
            Err(exit) => {
                let _ = stderr.write_all(exit.stderr.as_bytes());
                return exit.code;
            }
        }
    }
    let options = ScanOptions::from_namespace(&namespace);
    scan_with(&options, &System, stdout, stderr)
}

pub fn main(args: Vec<String>) -> i32 {
    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    run(&args, &mut out, &mut err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(args: &[&str]) -> ScanOptions {
        ScanOptions::parse(args).expect("valid scan arguments")
    }

    // tests/test_cli.py::test_cli_redacts_warnings (the collector is replaced
    // by injected warnings, exactly like the monkeypatched original)
    #[test]
    fn test_cli_redacts_warnings() {
        let home = "/home/tester";
        let opts = options(&[
            "--format",
            "json",
            "--systemd-scope",
            "off",
            "--no-cron",
            "--no-env",
            "--no-wrappers",
        ]);
        let collected = Collected {
            items: Vec::new(),
            warnings: vec![format!("git failed under {home}/private TOKEN=abc123")],
        };
        let report = build_report(&opts, collected, &[], 0, "host", home);
        assert!(!report.complete);
        let output = format_report(&report, opts.format);
        assert!(output.contains("\"complete\": false"));
        assert!(!output.contains("abc123"));
        assert!(!output.contains(home));
        assert!(output.contains("~/private"));
        assert!(output.contains("TOKEN=<redacted>"));
    }

    #[test]
    fn report_items_are_filtered_classified_redacted_and_sorted() {
        let home = "/home/tester";
        let opts = options(&[
            "--focus",
            "TOOL",
            "--kind",
            "Wrapper",
            "--retire-keyword",
            "old",
        ]);
        let mut first = InventoryItem::new("wrapper", "tool-b");
        first.path = format!("{home}/bin/tool-b");
        first.metadata.set("shebang", "#!/bin/sh TOKEN=x");
        first.metadata.set("source_path", format!("{home}/src"));
        let mut second = InventoryItem::new("wrapper", "old-tool");
        second.path = format!("{home}/bin/old-tool");
        let other = InventoryItem::new("repository", "tool-repo");
        let collected = Collected {
            items: vec![first, second, other],
            warnings: Vec::new(),
        };
        let report = build_report(&opts, collected, &[format!("{home}/bin")], 0, "box", home);
        assert!(report.complete);
        assert_eq!(report.host, "redacted");
        assert_eq!(report.roots, ["~/bin"]);
        let names: Vec<&str> = report.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["old-tool", "tool-b"]);
        assert_eq!(report.items[0].suggested_action, "retire_candidate");
        assert_eq!(report.items[1].path, "~/bin/tool-b");
        assert_eq!(
            report.items[1].metadata.get("shebang"),
            Some("#!/bin/sh TOKEN=<redacted>")
        );
        assert_eq!(report.items[1].metadata.get("source_path"), Some("~/src"));
    }

    #[test]
    fn scan_option_defaults_match_reference() {
        let opts = options(&[]);
        assert_eq!(opts.format, Format::Markdown);
        assert_eq!(opts.stale_days, 90);
        assert_eq!(opts.timeout, 5.0);
        assert_eq!(opts.repo_depth, 2);
        assert_eq!(opts.env_depth, 2);
        assert_eq!(opts.systemd_scope, "user");
        assert_eq!(opts.output, None);
        assert_eq!(
            opts.unit_dirs(),
            [
                "~/.config/systemd/user",
                "~/.local/share/systemd/user",
                "/etc/systemd/user"
            ]
        );
        let system = options(&[
            "--systemd-scope",
            "system",
            "--repo-root",
            "a//b/./",
            "--repo-root",
            "x",
        ]);
        assert_eq!(system.unit_dirs()[0], "/etc/systemd/system");
        assert_eq!(system.repo_root, ["a/b", "x"]);
    }

    #[test]
    fn scan_roots_dedupe_by_expanded_path() {
        let opts = options(&[
            "--systemd-scope",
            "off",
            "--repo-root",
            "work",
            "--env-root",
            "./work/",
            "--wrapper-dir",
            "bin",
            "--wrapper-dir",
            "bin",
        ]);
        assert_eq!(opts.scan_roots().unwrap(), ["work", "bin"]);
        let broken = options(&["--wrapper-dir", "~no-such-user-oremap/bin"]);
        assert_eq!(broken.scan_roots(), Err(HomeError));
    }
}
