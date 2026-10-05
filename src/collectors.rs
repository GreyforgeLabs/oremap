//! Read-only collectors for local workflow surfaces.
//!
//! Nothing here starts, stops, enables, or edits anything. Filesystem access
//! is limited to `stat`, directory listing, and bounded no-follow reads of
//! `.git` marker files and env files. External tools come only from a fixed
//! absolute-path allowlist and run with a minimal environment.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata as FsMetadata};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::formatters::csv_record;
use crate::models::{InventoryItem, Metadata};
use crate::paths;
use crate::privacy::{redact_command, sanitize_csv_cell};
use crate::pyfmt::{self, char_len, repr_float, repr_str, splitlines, strip, take_chars};
use crate::timefmt::{isoformat_utc, mtime_micros};

pub const SKIP_DIRS: [&str; 13] = [
    ".cache",
    ".git",
    ".hg",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".venv",
    "__pycache__",
    "dist",
    "node_modules",
    "target",
    "venv",
];

/// Reviewed absolute candidates for every external tool.
pub const APPROVED_EXECUTABLES: [(&str, &[&str]); 3] = [
    ("crontab", &["/usr/bin/crontab", "/bin/crontab"]),
    ("git", &["/usr/bin/git", "/bin/git"]),
    ("systemctl", &["/usr/bin/systemctl", "/bin/systemctl"]),
];
pub const SAFE_SUBPROCESS_ENV: [(&str, &str); 3] =
    [("LANG", "C"), ("LC_ALL", "C"), ("PATH", "/usr/bin:/bin")];
pub const USER_BUS_ENV: [&str; 2] = ["DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR"];
pub const MAX_ENV_BYTES: usize = 64 * 1024;
pub const MAX_ENV_LINES: usize = 1000;
const MAX_GIT_MARKER_BYTES: u64 = 4096;
const UNIT_SUFFIXES: [&str; 4] = [".service", ".timer", ".socket", ".path"];

/// Outcome of one external command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub executable: String,
    /// `ok`, `nonzero`, `timeout`, or `exec-error`.
    pub status: String,
}

impl CommandResult {
    pub fn new(code: i32, stdout: &str, stderr: &str, executable: &str) -> Self {
        Self {
            code,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            executable: executable.to_string(),
            status: if code == 0 { "ok" } else { "nonzero" }.to_string(),
        }
    }
}

/// External-tool boundary. The real implementation is [`System`]; tests
/// substitute deterministic fakes.
pub trait Exec: Sync {
    fn resolve(&self, name: &str) -> Option<String>;
    /// Arguments are `OsString`s so paths that are not valid UTF-8 reach
    /// the tool unchanged.
    fn run(&self, args: &[OsString], timeout: f64) -> CommandResult;
    fn probe(&self, name: &str, path: &str, timeout: f64) -> String {
        probe_with(self, name, path, timeout)
    }
}

/// The host system: approved executables and real subprocesses.
#[derive(Debug, Clone, Copy, Default)]
pub struct System;

impl Exec for System {
    fn resolve(&self, name: &str) -> Option<String> {
        resolve_approved_executable(name)
    }

    fn run(&self, args: &[OsString], timeout: f64) -> CommandResult {
        run_command(args, timeout)
    }
}

// ---------------------------------------------------------------------------
// Error text helpers
// ---------------------------------------------------------------------------

/// `OSError.strerror` for an I/O error (the C library message).
pub fn strerror(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => {
            let text = io::Error::from_raw_os_error(code).to_string();
            let suffix = format!(" (os error {code})");
            text.strip_suffix(&suffix).unwrap_or(&text).to_string()
        }
        None => error.kind().to_string(),
    }
}

fn record_inventory_error(warnings: &mut Vec<String>, path: &str, error: &io::Error) {
    let status = if error.kind() == io::ErrorKind::NotFound {
        "inventory-race"
    } else {
        "inventory-error"
    };
    warnings.push(format!("{status}: {path}: {}", strerror(error)));
}

fn same_file_identity(left: &FsMetadata, right: &FsMetadata) -> bool {
    (left.dev(), left.ino(), left.mode()) == (right.dev(), right.ino(), right.mode())
}

fn mtime_iso(path: &Path) -> String {
    // Python raised on errors other than FileNotFoundError; an empty value
    // keeps the scan going instead (documented in CHANGELOG 0.4.0).
    match fs::metadata(path) {
        Ok(metadata) => {
            isoformat_utc(mtime_micros(metadata.mtime(), metadata.mtime_nsec())).unwrap_or_default()
        }
        Err(_) => String::new(),
    }
}

fn open_no_follow(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

fn read_bounded(file: &mut File, limit: usize) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::with_capacity(limit.min(16 * 1024));
    file.take(limit as u64).read_to_end(&mut buffer)?;
    Ok(buffer)
}

fn is_executable(path: &Path) -> bool {
    rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok()
}

// ---------------------------------------------------------------------------
// Concurrency helpers
// ---------------------------------------------------------------------------

/// Work started by [`spawn_or_run`]: on its own thread, or already finished.
pub enum Deferred<'scope, T> {
    Thread(thread::ScopedJoinHandle<'scope, Option<T>>),
    Ready(Option<T>),
}

impl<T> Deferred<'_, T> {
    /// The result, or `None` if the worker thread did not finish normally.
    pub fn join(self) -> Option<T> {
        match self {
            Self::Thread(handle) => handle.join().ok().flatten(),
            Self::Ready(value) => value,
        }
    }
}

/// Run `work` on a scoped thread, or inline on the calling thread when the
/// system cannot start another thread, so resource limits slow the scan down
/// instead of aborting it.
pub fn spawn_or_run<'scope, T, F>(
    scope: &'scope thread::Scope<'scope, '_>,
    work: F,
) -> Deferred<'scope, T>
where
    T: Send + 'scope,
    F: FnOnce() -> T + Send + 'scope,
{
    let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(work)));
    let theirs = std::sync::Arc::clone(&slot);
    let spawned = thread::Builder::new().spawn_scoped(scope, move || {
        let work = theirs.lock().ok().and_then(|mut guard| guard.take());
        work.map(|work| work())
    });
    match spawned {
        Ok(handle) => Deferred::Thread(handle),
        // The rejected closure was dropped, so the work is still in the slot.
        Err(_) => Deferred::Ready(
            slot.lock()
                .ok()
                .and_then(|mut guard| guard.take())
                .map(|work| work()),
        ),
    }
}

// ---------------------------------------------------------------------------
// Subprocesses
// ---------------------------------------------------------------------------

/// The environment handed to one external command.
pub fn subprocess_env(
    args: &[OsString],
    getenv: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let mut environment: Vec<(String, String)> = SAFE_SUBPROCESS_ENV
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    // `Path(executable).name`
    let name = args
        .first()
        .and_then(|executable| Path::new(executable).file_name())
        .unwrap_or_default();
    if name == "systemctl" && args.iter().skip(1).any(|arg| arg.as_os_str() == "--user") {
        for key in USER_BUS_ENV {
            if let Some(value) = getenv(key).filter(|value| !value.is_empty()) {
                environment.push((key.to_string(), value));
            }
        }
    }
    if name == "git" {
        // Keep `git status` from opportunistically rewriting .git/index.
        environment.push(("GIT_OPTIONAL_LOCKS".to_string(), "0".to_string()));
    }
    environment
}

fn timeout_message(args: &[OsString], timeout: f64) -> String {
    let rendered: Vec<String> = args
        .iter()
        .map(|arg| repr_str(&arg.to_string_lossy()))
        .collect();
    format!(
        "Command '[{}]' timed out after {} seconds",
        rendered.join(", "),
        repr_float(timeout)
    )
}

/// Run one approved command with a minimal environment, a deadline, and
/// distinct `ok`/`nonzero`/`timeout`/`exec-error` outcomes.
pub fn run_command(args: &[OsString], timeout: f64) -> CommandResult {
    run_command_with_env(args, timeout, &|name| std::env::var(name).ok())
}

/// [`run_command`] reading the parent environment through `getenv`.
pub fn run_command_with_env(
    args: &[OsString],
    timeout: f64,
    getenv: &dyn Fn(&str) -> Option<String>,
) -> CommandResult {
    let executable = args
        .first()
        .map(|arg| arg.to_string_lossy().into_owned())
        .unwrap_or_default();
    let failure = |code: i32, stderr: String, status: &str| CommandResult {
        code,
        stdout: String::new(),
        stderr,
        executable: executable.clone(),
        status: status.to_string(),
    };
    if args.is_empty() {
        return failure(127, "empty command".to_string(), "exec-error");
    }
    let mut command = Command::new(&args[0]);
    command
        .args(&args[1..])
        .env_clear()
        .envs(subprocess_env(args, getenv))
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let code = match error.raw_os_error() {
                Some(13) | Some(1) => 126,
                _ => 127,
            };
            let message = match error.raw_os_error() {
                Some(errno) => format!(
                    "[Errno {errno}] {}: {}",
                    strerror(&error),
                    repr_str(&executable)
                ),
                None => error.to_string(),
            };
            return failure(code, message, "exec-error");
        }
    };

    // Python's subprocess.run treats a non-finite timeout as an error; here
    // it means "no deadline" (documented in CHANGELOG 0.4.0).
    let deadline = if timeout.is_nan() {
        None
    } else {
        Duration::try_from_secs_f64(timeout.max(0.0))
            .ok()
            .and_then(|limit| Instant::now().checked_add(limit))
    };
    let timed_out = |child: &mut std::process::Child| {
        let _ = child.kill();
        let _ = child.wait();
        failure(124, timeout_message(args, timeout), "timeout")
    };
    if timeout.is_finite() && timeout <= 0.0 {
        return timed_out(&mut child);
    }

    let (sender, receiver) = mpsc::channel::<(u8, Vec<u8>)>();
    let mut pending = 0;
    let pipes: [(u8, Option<Box<dyn Read + Send>>); 2] = [
        (
            1,
            child
                .stdout
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        ),
        (
            2,
            child
                .stderr
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        ),
    ];
    for (stream, pipe) in pipes {
        let Some(mut pipe) = pipe else { continue };
        let sender = sender.clone();
        let reader = thread::Builder::new().spawn(move || {
            let mut data = Vec::new();
            let _ = pipe.read_to_end(&mut data);
            let _ = sender.send((stream, data));
        });
        match reader {
            Ok(_) => pending += 1,
            Err(error) => {
                // Like a failed fork in Python: report an exec error.
                let _ = child.kill();
                let _ = child.wait();
                let message = match error.raw_os_error() {
                    Some(errno) => format!("[Errno {errno}] {}", strerror(&error)),
                    None => error.to_string(),
                };
                return failure(127, message, "exec-error");
            }
        }
    }
    drop(sender);

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while pending > 0 {
        let message = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match receiver.recv_timeout(remaining) {
                    Ok(message) => message,
                    Err(mpsc::RecvTimeoutError::Timeout) => return timed_out(&mut child),
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match receiver.recv() {
                Ok(message) => message,
                Err(_) => break,
            },
        };
        pending -= 1;
        match message {
            (1, data) => stdout = data,
            (_, data) => stderr = data,
        }
    }

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => return timed_out(&mut child),
        }
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return timed_out(&mut child);
            }
            thread::sleep(Duration::from_micros(200));
        } else {
            match child.wait() {
                Ok(status) => break status,
                Err(_) => return failure(127, String::new(), "exec-error"),
            }
        }
    };
    let code = status
        .code()
        .unwrap_or_else(|| -status.signal().unwrap_or(0));
    CommandResult {
        code,
        stdout: pyfmt::decode_process_text(&stdout),
        stderr: pyfmt::decode_process_text(&stderr),
        executable,
        status: if code == 0 { "ok" } else { "nonzero" }.to_string(),
    }
}

/// Resolve the first existing executable from explicit absolute candidates.
pub fn resolve_from_candidates(candidates: &[&str]) -> Option<String> {
    for candidate in candidates {
        let Ok(path) = fs::canonicalize(candidate) else {
            continue;
        };
        let is_file = fs::metadata(&path).is_ok_and(|metadata| metadata.is_file());
        if is_file && is_executable(&path) {
            return Some(paths::display(&path));
        }
    }
    None
}

/// Resolve an approved tool. `PATH` is never consulted.
pub fn resolve_approved_executable(name: &str) -> Option<String> {
    APPROVED_EXECUTABLES
        .iter()
        .find(|(tool, _)| *tool == name)
        .and_then(|(_, candidates)| resolve_from_candidates(candidates))
}

fn clean_version_line(line: &str) -> String {
    let replaced: String = line
        .chars()
        .map(|c| {
            if (c as u32) < 0x20 || c as u32 == 0x7f {
                ' '
            } else {
                c
            }
        })
        .collect();
    pyfmt::split(&replaced).join(" ")
}

/// Bounded version diagnostic for an approved executable.
pub fn probe_with<E: Exec + ?Sized>(exec: &E, _name: &str, path: &str, timeout: f64) -> String {
    let result = exec.run(&[path.into(), "--version".into()], timeout);
    if result.status != "ok" {
        return format!("unavailable:{}:{}", result.status, result.code);
    }
    for line in splitlines(&result.stdout)
        .into_iter()
        .chain(splitlines(&result.stderr))
    {
        let cleaned = clean_version_line(line);
        if !cleaned.is_empty() {
            return take_chars(&cleaned, 160).to_string();
        }
    }
    "unavailable:empty".to_string()
}

/// [`probe_with`] using the real system.
pub fn probe_executable_version(name: &str, path: &str, timeout: f64) -> String {
    probe_with(&System, name, path, timeout)
}

// ---------------------------------------------------------------------------
// systemd
// ---------------------------------------------------------------------------

/// Insertion-ordered map keyed by unit name with dict assignment semantics.
#[derive(Default)]
struct UnitMap {
    order: Vec<String>,
    items: std::collections::HashMap<String, InventoryItem>,
}

impl UnitMap {
    fn insert(&mut self, item: InventoryItem) {
        if !self.items.contains_key(&item.name) {
            self.order.push(item.name.clone());
        }
        self.items.insert(item.name.clone(), item);
    }

    fn into_ordered(mut self) -> Vec<InventoryItem> {
        self.order
            .iter()
            .filter_map(|name| self.items.remove(name))
            .collect()
    }
}

fn ordered_pairs(pairs: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (key, value) in pairs {
        match index.get(&key) {
            Some(position) => out[*position].1 = value,
            None => {
                index.insert(key.clone(), out.len());
                out.push((key, value));
            }
        }
    }
    out
}

/// Parse `systemctl list-unit-files --no-legend` output into name -> state.
pub fn parse_systemctl_unit_files(output: &str) -> Vec<(String, String)> {
    ordered_pairs(splitlines(output).into_iter().filter_map(|line| {
        let parts = pyfmt::split(line);
        (parts.len() >= 2 && parts[0].contains('.'))
            .then(|| (parts[0].to_string(), parts[1].to_string()))
    }))
}

/// Parse `systemctl list-units --all --no-legend` output into name -> active.
pub fn parse_systemctl_units(output: &str) -> Vec<(String, String)> {
    ordered_pairs(splitlines(output).into_iter().filter_map(|line| {
        let parts = pyfmt::split(line);
        (parts.len() >= 4 && parts[0].contains('.'))
            .then(|| (parts[0].to_string(), parts[2].to_string()))
    }))
}

fn unit_suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(index) if index > 0 && index < name.len() - 1 => &name[index..],
        _ => "",
    }
}

fn unit_files(dir_spelling: &str, scope: &str) -> Vec<InventoryItem> {
    let Ok(expanded) = paths::expanduser(dir_spelling) else {
        return Vec::new();
    };
    let directory = paths::to_path(&expanded);
    if fs::metadata(&directory).is_err() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut names: Vec<(String, OsString)> = entries
        .filter_map(Result::ok)
        .map(|entry| {
            let raw = entry.file_name();
            (raw.to_string_lossy().into_owned(), raw)
        })
        .filter(|(name, _)| name.contains('.') && UNIT_SUFFIXES.contains(&unit_suffix(name)))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|(name, raw)| {
            let path = paths::join(&expanded, &name);
            let mut item = InventoryItem::new("systemd_unit", name);
            // Stat the real name; the display spelling may contain U+FFFD.
            item.last_activity = mtime_iso(&directory.join(&raw));
            item.path = path;
            item.metadata = Metadata::from_pairs([("scope", scope), ("source", "unit_file")]);
            item
        })
        .collect()
}

fn merge_systemctl(
    items: &mut UnitMap,
    states: Vec<(String, String)>,
    result: &CommandResult,
    scope: &str,
    include_discovered: bool,
    executable_version: &str,
    apply: fn(&mut InventoryItem, String),
) {
    for (name, state) in states {
        let mut item = match items.items.remove(&name) {
            Some(item) => item,
            None => {
                if !include_discovered {
                    continue;
                }
                let mut item = InventoryItem::new("systemd_unit", name.clone());
                item.metadata = Metadata::from_pairs([
                    ("scope", scope),
                    ("source", "systemctl"),
                    ("executable", result.executable.as_str()),
                    ("executable_version", executable_version),
                ]);
                items.order.push(name.clone());
                item
            }
        };
        item.metadata.set("executable", result.executable.clone());
        item.metadata.set("executable_version", executable_version);
        apply(&mut item, state);
        items.items.insert(name, item);
    }
}

/// Collect systemd unit files and `systemctl` state without mutating anything.
pub fn collect_systemd(
    exec: &dyn Exec,
    scope: &str,
    unit_dirs: &[String],
    include_discovered: bool,
    timeout: f64,
) -> (Vec<InventoryItem>, Vec<String>) {
    let mut items = UnitMap::default();
    let mut warnings = Vec::new();
    for unit_dir in unit_dirs {
        for item in unit_files(unit_dir, scope) {
            items.insert(item);
        }
    }

    let systemctl = exec.resolve("systemctl");
    let systemctl = match systemctl {
        Some(path) if scope != "off" => path,
        other => {
            if scope != "off" && other.is_none() {
                warnings.push("systemctl approved executable not found".to_string());
            }
            return (items.into_ordered(), warnings);
        }
    };

    let mut unit_file_args: Vec<OsString> = vec![systemctl.clone().into()];
    let mut unit_args: Vec<OsString> = vec![systemctl.clone().into()];
    if scope == "user" {
        unit_file_args.push("--user".into());
        unit_args.push("--user".into());
    }
    unit_file_args.extend(["list-unit-files", "--no-legend", "--no-pager"].map(OsString::from));
    unit_args.extend(["list-units", "--all", "--no-legend", "--no-pager"].map(OsString::from));

    // The three read-only queries are independent; run them concurrently and
    // merge in the original order so output is unchanged.
    let (executable_version, unit_file_result, unit_result) = thread::scope(|s| {
        let version = spawn_or_run(s, || exec.probe("systemctl", &systemctl, timeout));
        let files = spawn_or_run(s, || exec.run(&unit_file_args, timeout));
        let units = exec.run(&unit_args, timeout);
        (
            version.join().unwrap_or_default(),
            files
                .join()
                .unwrap_or_else(|| CommandResult::new(1, "", "", &systemctl)),
            units,
        )
    });

    for name in items.order.clone() {
        if let Some(item) = items.items.get_mut(&name) {
            item.metadata.set("executable", systemctl.clone());
            item.metadata
                .set("executable_version", executable_version.clone());
        }
    }

    if unit_file_result.code == 0 {
        let states = parse_systemctl_unit_files(&unit_file_result.stdout);
        merge_systemctl(
            &mut items,
            states,
            &unit_file_result,
            scope,
            include_discovered,
            &executable_version,
            |item, state| item.enabled = state,
        );
    } else if !strip(&unit_file_result.stderr).is_empty() {
        warnings.push(format!(
            "systemctl list-unit-files failed: {}",
            strip(&unit_file_result.stderr)
        ));
    }

    if unit_result.code == 0 {
        let states = parse_systemctl_units(&unit_result.stdout);
        merge_systemctl(
            &mut items,
            states,
            &unit_result,
            scope,
            include_discovered,
            &executable_version,
            |item, state| item.active = state,
        );
    } else if !strip(&unit_result.stderr).is_empty() {
        warnings.push(format!(
            "systemctl list-units failed: {}",
            strip(&unit_result.stderr)
        ));
    }

    let mut ordered = items.into_ordered();
    ordered.sort_by(|a, b| {
        (a.kind.as_str(), a.name.as_str()).cmp(&(b.kind.as_str(), b.name.as_str()))
    });
    (ordered, warnings)
}

// ---------------------------------------------------------------------------
// cron
// ---------------------------------------------------------------------------

fn looks_like_assignment(line: &str) -> bool {
    let mut chars = line.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    let rest: &str = chars.as_str();
    let after_name = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
    after_name
        .trim_start_matches(pyfmt::is_space)
        .starts_with('=')
}

fn split_cron_line(line: &str) -> (String, String) {
    let parts = pyfmt::split(line);
    if parts.is_empty() {
        return (String::new(), String::new());
    }
    if parts[0].starts_with('@') {
        return (parts[0].to_string(), parts[1..].join(" "));
    }
    if parts.len() >= 6 {
        return (parts[..5].join(" "), parts[5..].join(" "));
    }
    ("unknown".to_string(), line.to_string())
}

/// Collect the current user's crontab entries.
pub fn collect_cron(
    exec: &dyn Exec,
    timeout: f64,
    absolute_paths: bool,
) -> (Vec<InventoryItem>, Vec<String>) {
    let Some(crontab) = exec.resolve("crontab") else {
        return (
            Vec::new(),
            vec!["crontab approved executable not found".to_string()],
        );
    };
    let list_args: [OsString; 2] = [crontab.clone().into(), "-l".into()];
    let (executable_version, result) = thread::scope(|s| {
        let version = spawn_or_run(s, || exec.probe("crontab", &crontab, timeout));
        let result = exec.run(&list_args, timeout);
        (version.join().unwrap_or_default(), result)
    });
    if result.code != 0 {
        let stderr = strip(&result.stderr).to_lowercase();
        if stderr.contains("no crontab") {
            return (Vec::new(), Vec::new());
        }
        return (
            Vec::new(),
            vec![format!("crontab -l failed: {}", strip(&result.stderr))],
        );
    }

    let mut items = Vec::new();
    for (index, line) in splitlines(&result.stdout).into_iter().enumerate() {
        let stripped = strip(line);
        if stripped.is_empty() || stripped.starts_with('#') || looks_like_assignment(stripped) {
            continue;
        }
        let (schedule, command) = split_cron_line(stripped);
        let mut preview = redact_command(&command, absolute_paths);
        if char_len(&preview) > 160 {
            preview = format!("{}...", take_chars(&preview, 157));
        }
        let mut item = InventoryItem::new("cron_job", format!("user-crontab:{}", index + 1));
        item.status = "present".into();
        item.metadata = Metadata::from_pairs([
            ("schedule", schedule),
            ("command_preview", preview),
            ("executable", result.executable.clone()),
            ("executable_version", executable_version.clone()),
        ]);
        items.push(item);
    }
    (items, Vec::new())
}

// ---------------------------------------------------------------------------
// wrappers
// ---------------------------------------------------------------------------

fn read_shebang(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let mut line = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return String::new(),
        };
        if let Some(end) = chunk[..read]
            .iter()
            .position(|b| *b == b'\n' || *b == b'\r')
        {
            line.extend_from_slice(&chunk[..end]);
            break;
        }
        line.extend_from_slice(&chunk[..read]);
    }
    let decoded = pyfmt::decode_ignore(&line);
    let first = strip(&decoded);
    if first.starts_with("#!") {
        first.to_string()
    } else {
        String::new()
    }
}

/// Collect executable files from explicit wrapper directories.
///
/// A directory that exists but cannot be listed is reported as an
/// `inventory-error` warning (Python 0.3.0 aborted with a traceback).
pub fn collect_wrappers(wrapper_dirs: &[String], warnings: &mut Vec<String>) -> Vec<InventoryItem> {
    let mut items = Vec::new();
    for wrapper_dir in wrapper_dirs {
        let Ok(expanded) = paths::expanduser(wrapper_dir) else {
            continue;
        };
        let directory = paths::to_path(&expanded);
        if !fs::metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
            continue;
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                record_inventory_error(warnings, &expanded, &error);
                continue;
            }
        };
        let mut names: Vec<OsString> = Vec::new();
        let mut failed = false;
        for entry in entries {
            match entry {
                Ok(entry) => names.push(entry.file_name()),
                Err(error) => {
                    record_inventory_error(warnings, &expanded, &error);
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            continue;
        }
        let mut names: Vec<(String, OsString)> = names
            .into_iter()
            .map(|name| (name.to_string_lossy().into_owned(), name))
            .collect();
        names.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for (display_name, raw_name) in names {
            let path = directory.join(&raw_name);
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() || !is_executable(&path) {
                continue;
            }
            let mut item = InventoryItem::new("wrapper", display_name.clone());
            item.metadata.set("size_bytes", metadata.len().to_string());
            let shebang = read_shebang(&path);
            if !shebang.is_empty() {
                item.metadata.set("shebang", shebang);
            }
            item.path = paths::join(&expanded, &display_name);
            item.status = "executable".into();
            item.last_activity = mtime_iso(&path);
            items.push(item);
        }
    }
    items
}

// ---------------------------------------------------------------------------
// Directory walking (os.walk, topdown, followlinks=False)
// ---------------------------------------------------------------------------

/// Walk callback: directory, depth, prunable sub-directory names, other names.
type Visit<'a> = dyn FnMut(&Path, usize, &mut Vec<OsString>, &[OsString]) + 'a;

fn walk(top: &Path, on_error: &mut dyn FnMut(&Path, &io::Error), visit: &mut Visit<'_>) {
    let mut stack: Vec<(PathBuf, usize)> = vec![(top.to_path_buf(), 0)];
    while let Some((directory, depth)) = stack.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                on_error(&directory, &error);
                continue;
            }
        };
        let mut dirs = Vec::new();
        let mut nondirs = Vec::new();
        let mut failed = false;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    on_error(&directory, &error);
                    failed = true;
                    break;
                }
            };
            let is_dir = match entry.file_type() {
                Ok(kind) if kind.is_symlink() => {
                    fs::metadata(entry.path()).is_ok_and(|metadata| metadata.is_dir())
                }
                Ok(kind) => kind.is_dir(),
                Err(_) => false,
            };
            if is_dir {
                dirs.push(entry.file_name());
            } else {
                nondirs.push(entry.file_name());
            }
        }
        if failed {
            continue;
        }
        visit(&directory, depth, &mut dirs, &nondirs);
        for name in dirs.iter().rev() {
            let child = directory.join(name);
            let is_link = fs::symlink_metadata(&child).is_ok_and(|m| m.file_type().is_symlink());
            if !is_link {
                stack.push((child, depth + 1));
            }
        }
    }
}

fn resolve_root(root: &str, warnings: &mut Vec<String>) -> Option<PathBuf> {
    let expanded = paths::expanduser(root).ok()?;
    match fs::canonicalize(&expanded) {
        Ok(path) => Some(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            record_inventory_error(warnings, root, &error);
            None
        }
    }
}

fn is_skipped_dir(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    SKIP_DIRS.iter().any(|skip| skip.as_bytes() == bytes)
}

// ---------------------------------------------------------------------------
// repositories
// ---------------------------------------------------------------------------

fn lstat_matches(
    path: &Path,
    predicate: fn(&FsMetadata) -> bool,
    warnings: &mut Vec<String>,
) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) => predicate(&metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            record_inventory_error(warnings, &paths::display(path), &error);
            false
        }
    }
}

fn has_safe_git_marker(path: &Path, warnings: &mut Vec<String>) -> bool {
    let marker = path.join(".git");
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return false,
        Err(error) => {
            record_inventory_error(warnings, &paths::display(&marker), &error);
            return false;
        }
    };
    let kind = metadata.file_type();
    if kind.is_symlink() {
        return false;
    }
    if kind.is_dir() {
        return true;
    }
    if !kind.is_file() || metadata.len() > MAX_GIT_MARKER_BYTES {
        return false;
    }
    let result = (|| -> io::Result<bool> {
        let mut file = open_no_follow(&marker)?;
        let opened = file.metadata()?;
        if !opened.is_file() || !same_file_identity(&metadata, &opened) {
            return Ok(false);
        }
        let content = read_bounded(&mut file, MAX_GIT_MARKER_BYTES as usize)?;
        let start = content
            .iter()
            .position(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c'))
            .unwrap_or(content.len());
        Ok(content[start..].starts_with(b"gitdir:"))
    })();
    match result {
        Ok(found) => found,
        Err(error) => {
            record_inventory_error(warnings, &paths::display(&marker), &error);
            false
        }
    }
}

fn looks_like_bare_git_repo(path: &Path, warnings: &mut Vec<String>) -> bool {
    lstat_matches(&path.join("HEAD"), |m| m.file_type().is_file(), warnings)
        && lstat_matches(&path.join("objects"), |m| m.file_type().is_dir(), warnings)
        && (lstat_matches(&path.join("refs"), |m| m.file_type().is_dir(), warnings)
            || lstat_matches(
                &path.join("packed-refs"),
                |m| m.file_type().is_file(),
                warnings,
            ))
}

/// Find Git work trees, linked worktrees, and bare repositories without
/// following `.git` marker symlinks.
pub fn find_git_repositories(
    repo_roots: &[String],
    max_depth: i64,
    warnings: &mut Vec<String>,
) -> Vec<PathBuf> {
    let mut repos = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for root in repo_roots {
        let Some(expanded) = resolve_root(root, warnings) else {
            continue;
        };
        let warnings_cell = std::cell::RefCell::new(std::mem::take(warnings));
        walk(
            &expanded,
            &mut |path, error| {
                record_inventory_error(
                    &mut warnings_cell.borrow_mut(),
                    &paths::display(path),
                    error,
                )
            },
            &mut |current, depth, dirnames, _files| {
                let mut sink = warnings_cell.borrow_mut();
                if has_safe_git_marker(current, &mut sink)
                    || looks_like_bare_git_repo(current, &mut sink)
                {
                    match fs::canonicalize(current) {
                        Ok(resolved) => {
                            if seen.insert(resolved) {
                                repos.push(current.to_path_buf());
                            }
                        }
                        Err(error) => {
                            record_inventory_error(&mut sink, &paths::display(current), &error)
                        }
                    }
                    dirnames.clear();
                    return;
                }
                dirnames.retain(|name| {
                    !is_skipped_dir(name) && !name.as_bytes().starts_with(b".cache")
                });
                if depth as i64 >= max_depth {
                    dirnames.clear();
                }
            },
        );
        *warnings = warnings_cell.into_inner();
    }
    repos
}

struct RepoProbe {
    branch: CommandResult,
    status: CommandResult,
    last_commit: CommandResult,
}

fn worker_count(jobs: usize) -> usize {
    let available = thread::available_parallelism().map_or(4, |n| n.get());
    jobs.min(available.clamp(2, 8)).max(1)
}

/// Find Git repositories under the requested roots and collect lightweight
/// state with read-only `git rev-parse`, `git status --porcelain`, and
/// `git log -1`.
pub fn collect_repositories(
    exec: &dyn Exec,
    repo_roots: &[String],
    max_depth: i64,
    timeout: f64,
) -> (Vec<InventoryItem>, Vec<String>) {
    let mut warnings = Vec::new();
    let Some(git) = exec.resolve("git") else {
        return (
            Vec::new(),
            vec!["git approved executable not found".to_string()],
        );
    };

    let (executable_version, repos) = thread::scope(|s| {
        let version = spawn_or_run(s, || exec.probe("git", &git, timeout));
        let repos = find_git_repositories(repo_roots, max_depth, &mut warnings);
        (version.join().unwrap_or_default(), repos)
    });

    let displays: Vec<String> = repos.iter().map(|repo| paths::display(repo)).collect();
    let next = AtomicUsize::new(0);
    let mut probes: Vec<Option<RepoProbe>> = (0..repos.len()).map(|_| None).collect();
    let results = std::sync::Mutex::new(&mut probes);
    thread::scope(|s| {
        for _ in 0..worker_count(repos.len()) {
            // A worker that cannot get its own thread drains the queue inline.
            spawn_or_run(s, || {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(repo) = repos.get(index) else {
                        break;
                    };
                    let run = |extra: &[&str]| {
                        let mut args: Vec<OsString> = vec![
                            git.clone().into(),
                            "-C".into(),
                            repo.clone().into_os_string(),
                        ];
                        args.extend(extra.iter().map(OsString::from));
                        exec.run(&args, timeout)
                    };
                    let probe = RepoProbe {
                        branch: run(&["rev-parse", "--abbrev-ref", "HEAD"]),
                        status: run(&["status", "--porcelain"]),
                        last_commit: run(&["log", "-1", "--format=%cI"]),
                    };
                    if let Ok(mut guard) = results.lock() {
                        guard[index] = Some(probe);
                    }
                }
            });
        }
    });

    let mut items = Vec::new();
    for ((repo, display), probe) in repos.iter().zip(&displays).zip(probes) {
        let Some(probe) = probe else { continue };
        let mut metadata = Metadata::from_pairs([
            ("executable", git.as_str()),
            ("executable_version", executable_version.as_str()),
        ]);
        if probe.branch.code == 0 {
            metadata.set("branch", strip(&probe.branch.stdout));
        }
        let mut repo_status = "unknown";
        if probe.status.code == 0 {
            let dirty = splitlines(&probe.status.stdout)
                .into_iter()
                .filter(|line| !strip(line).is_empty())
                .count();
            repo_status = if dirty > 0 { "dirty" } else { "clean" };
            metadata.set("dirty_count", dirty.to_string());
        } else {
            warnings.push(format!(
                "git status failed for {display}: {}",
                strip(&probe.status.stderr)
            ));
        }
        let last_activity = if probe.last_commit.code == 0 {
            strip(&probe.last_commit.stdout).to_string()
        } else {
            mtime_iso(repo)
        };
        let name = repo
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut item = InventoryItem::new("repository", name);
        item.path = display.clone();
        item.status = repo_status.to_string();
        item.last_activity = last_activity;
        item.metadata = metadata;
        items.push(item);
    }
    items.sort_by(|a, b| a.path.cmp(&b.path));
    (items, warnings)
}

// ---------------------------------------------------------------------------
// env files
// ---------------------------------------------------------------------------

fn is_env_file(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    bytes == b".env"
        || bytes == b".envrc"
        || bytes.starts_with(b".env.")
        || bytes.ends_with(b".env")
}

/// Find env-like regular files under each root, rejecting symlinks and
/// anything that resolves outside the root.
pub fn find_env_files(
    env_roots: &[String],
    max_depth: i64,
    warnings: &mut Vec<String>,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for root in env_roots {
        let Some(expanded) = resolve_root(root, warnings) else {
            continue;
        };
        let warnings_cell = std::cell::RefCell::new(std::mem::take(warnings));
        walk(
            &expanded,
            &mut |path, error| {
                record_inventory_error(
                    &mut warnings_cell.borrow_mut(),
                    &paths::display(path),
                    error,
                )
            },
            &mut |current, depth, dirnames, filenames| {
                let mut sink = warnings_cell.borrow_mut();
                dirnames.retain(|name| !is_skipped_dir(name));
                for filename in filenames {
                    if !is_env_file(filename) {
                        continue;
                    }
                    let candidate = current.join(filename);
                    let metadata = match fs::symlink_metadata(&candidate) {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            record_inventory_error(&mut sink, &paths::display(&candidate), &error);
                            continue;
                        }
                    };
                    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                        continue;
                    }
                    let resolved = match fs::canonicalize(&candidate) {
                        Ok(resolved) => resolved,
                        Err(error) => {
                            record_inventory_error(&mut sink, &paths::display(&candidate), &error);
                            continue;
                        }
                    };
                    if !resolved.starts_with(&expanded) {
                        continue;
                    }
                    if seen.insert(resolved) {
                        files.push(candidate);
                    }
                }
                if depth as i64 >= max_depth {
                    dirnames.clear();
                }
            },
        );
        *warnings = warnings_cell.into_inner();
    }
    files
}

fn env_name(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches(pyfmt::is_space);
    let mut chars = trimmed.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return None,
    }
    let end = trimmed
        .char_indices()
        .skip(1)
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map_or(trimmed.len(), |(index, _)| index);
    let after = trimmed[end..].trim_start_matches(pyfmt::is_space);
    after.starts_with('=').then(|| &trimmed[..end])
}

/// Variable names declared in an env file. Values are never kept.
pub fn env_names(path: &Path, warnings: &mut Vec<String>) -> Vec<String> {
    let display = paths::display(path);
    let initial = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            record_inventory_error(warnings, &display, &error);
            return Vec::new();
        }
    };
    if initial.file_type().is_symlink() || !initial.file_type().is_file() {
        return Vec::new();
    }
    let raw = (|| -> io::Result<Option<Vec<u8>>> {
        let mut file = open_no_follow(path)?;
        let opened = file.metadata()?;
        if !opened.is_file() || !same_file_identity(&initial, &opened) {
            return Ok(None);
        }
        read_bounded(&mut file, MAX_ENV_BYTES + 1).map(Some)
    })();
    let raw = match raw {
        Ok(Some(raw)) => raw,
        Ok(None) => return Vec::new(),
        Err(error) => {
            record_inventory_error(warnings, &display, &error);
            return Vec::new();
        }
    };
    if raw.len() > MAX_ENV_BYTES {
        warnings.push(format!(
            "env-file-truncated: {display}: exceeds {MAX_ENV_BYTES} bytes"
        ));
    }
    let text = pyfmt::decode_ignore(&raw[..raw.len().min(MAX_ENV_BYTES)]);
    let mut names: Vec<String> = splitlines(&text)
        .into_iter()
        .take(MAX_ENV_LINES)
        .filter_map(env_name)
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Find env-like files and record metadata without outputting values.
pub fn collect_env_files(
    env_roots: &[String],
    max_depth: i64,
    include_names: bool,
    warnings: &mut Vec<String>,
) -> Vec<InventoryItem> {
    collect_env_files_with(
        env_roots,
        max_depth,
        include_names,
        warnings,
        &mut env_names,
    )
}

/// [`collect_env_files`] with an injectable name reader (used by tests to
/// simulate files that disappear mid-scan).
pub fn collect_env_files_with(
    env_roots: &[String],
    max_depth: i64,
    include_names: bool,
    warnings: &mut Vec<String>,
    read_names: &mut dyn FnMut(&Path, &mut Vec<String>) -> Vec<String>,
) -> Vec<InventoryItem> {
    let mut items = Vec::new();
    for path in find_env_files(env_roots, max_depth, warnings) {
        let names = read_names(&path, warnings);
        let mut item = InventoryItem::new(
            "env_file",
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        item.metadata.set("var_count", names.len().to_string());
        if include_names && !names.is_empty() {
            let shown: Vec<&str> = names.iter().take(40).map(String::as_str).collect();
            item.metadata.set("var_names", shown.join(","));
        }
        item.path = paths::display(&path);
        item.status = "present".into();
        item.last_activity = mtime_iso(&path);
        items.push(item);
    }
    items.sort_by(|a, b| a.path.cmp(&b.path));
    items
}

// ---------------------------------------------------------------------------
// CSV helper
// ---------------------------------------------------------------------------

/// Write rows (ordered key/value pairs) as spreadsheet-safe CSV. The header
/// comes from the first row's keys, or `kind,name` when there are no rows.
pub fn write_csv(rows: &[Vec<(String, String)>], path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let fieldnames: Vec<String> = match rows.first() {
        Some(first) => first.iter().map(|(key, _)| key.clone()).collect(),
        None => vec!["kind".to_string(), "name".to_string()],
    };
    let mut out = String::new();
    csv_record(&mut out, &fieldnames);
    for row in rows {
        if let Some((key, _)) = row.iter().find(|(key, _)| !fieldnames.contains(key)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("dict contains fields not in fieldnames: {}", repr_str(key)),
            ));
        }
        let values: Vec<String> = fieldnames
            .iter()
            .map(|field| {
                let value = row
                    .iter()
                    .find(|(key, _)| key == field)
                    .map_or("", |(_, value)| value.as_str());
                sanitize_csv_cell(value)
            })
            .collect();
        csv_record(&mut out, &values);
    }
    fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn spelling(path: &Path) -> String {
        paths::display(path)
    }

    fn write(path: &Path, content: &[u8]) {
        fs::write(path, content).expect("write fixture");
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
    }

    fn running_as_root() -> bool {
        rustix::process::geteuid().is_root()
    }

    type RunFn = dyn Fn(&[String]) -> CommandResult + Sync;

    fn lossy(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    struct Fake {
        executable: Option<String>,
        version: String,
        run: Box<RunFn>,
    }

    impl Exec for Fake {
        fn resolve(&self, _name: &str) -> Option<String> {
            self.executable.clone()
        }
        fn run(&self, args: &[OsString], _timeout: f64) -> CommandResult {
            (self.run)(&lossy(args))
        }
        fn probe(&self, _name: &str, _path: &str, _timeout: f64) -> String {
            self.version.clone()
        }
    }

    fn fake(run: impl Fn(&[String]) -> CommandResult + Sync + 'static) -> Fake {
        Fake {
            executable: Some("/usr/bin/tool".into()),
            version: "tool 1.2.3".into(),
            run: Box::new(run),
        }
    }

    // tests/test_collectors.py::test_parse_systemctl_unit_files
    #[test]
    fn test_parse_systemctl_unit_files() {
        let output = "alpha.service enabled enabled\nbeta.timer disabled disabled\n";
        assert_eq!(
            parse_systemctl_unit_files(output),
            vec![
                ("alpha.service".to_string(), "enabled".to_string()),
                ("beta.timer".to_string(), "disabled".to_string()),
            ]
        );
    }

    // tests/test_collectors.py::test_parse_systemctl_units
    #[test]
    fn test_parse_systemctl_units() {
        let output = "alpha.service loaded active running Alpha Service\nbeta.timer loaded inactive dead Beta\n";
        assert_eq!(
            parse_systemctl_units(output),
            vec![
                ("alpha.service".to_string(), "active".to_string()),
                ("beta.timer".to_string(), "inactive".to_string()),
            ]
        );
    }

    #[test]
    fn systemctl_parsers_skip_markers_and_keep_first_position() {
        let output = "● bad.service loaded failed failed Bad\nshort.service\nx.service a b c\nx.service d e f\n";
        assert_eq!(
            parse_systemctl_units(output),
            vec![("x.service".to_string(), "e".to_string())]
        );
        assert_eq!(
            parse_systemctl_unit_files("noext enabled\nz.timer static -\n"),
            vec![("z.timer".to_string(), "static".to_string())]
        );
    }

    // tests/test_collectors.py::test_live_user_systemd_bus_when_available
    #[test]
    fn test_live_user_systemd_bus_when_available() {
        let Some(systemctl) = resolve_approved_executable("systemctl") else {
            eprintln!("skipped: approved systemctl unavailable");
            return;
        };
        let baseline = Command::new(&systemctl)
            .args(["--user", "list-units", "--all", "--no-legend", "--no-pager"])
            .output();
        let readable = baseline.as_ref().is_ok_and(|output| {
            output.status.success()
                && !parse_systemctl_units(&String::from_utf8_lossy(&output.stdout)).is_empty()
        });
        if !readable {
            eprintln!("skipped: live user systemd bus has no readable units");
            return;
        }
        let (items, warnings) = collect_systemd(&System, "user", &[], true, 10.0);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(items.iter().any(|item| !item.active.is_empty()));
    }

    // tests/test_collectors.py::test_find_git_repositories_detects_root_repo
    #[test]
    fn test_find_git_repositories_detects_root_repo() {
        let dir = tmp();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let mut warnings = Vec::new();
        let found = find_git_repositories(&[spelling(&repo)], 1, &mut warnings);
        assert_eq!(found, vec![fs::canonicalize(&repo).unwrap()]);
    }

    // tests/test_collectors.py::test_find_git_repositories_supports_worktree_and_bare_layouts
    #[test]
    fn test_find_git_repositories_supports_worktree_and_bare_layouts() {
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        let worktree = root.join("worktree");
        fs::create_dir(&worktree).unwrap();
        write(&worktree.join(".git"), b"gitdir: /outside/not-read\n");
        let bare = root.join("bare.git");
        fs::create_dir_all(bare.join("objects")).unwrap();
        fs::create_dir(bare.join("refs")).unwrap();
        write(&bare.join("HEAD"), b"ref: refs/heads/main\n");

        let mut warnings = Vec::new();
        let found = find_git_repositories(&[spelling(&root)], 2, &mut warnings);

        assert!(found.contains(&worktree));
        assert!(found.contains(&bare));
    }

    // tests/test_collectors.py::test_find_git_repositories_rejects_symlink_marker
    #[test]
    fn test_find_git_repositories_rejects_symlink_marker() {
        let dir = tmp();
        let external = dir.path().join("external.git");
        fs::create_dir(&external).unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        symlink(&external, repo.join(".git")).unwrap();
        let mut warnings = Vec::new();
        assert!(find_git_repositories(&[spelling(&repo)], 1, &mut warnings).is_empty());
    }

    #[test]
    fn git_discovery_respects_depth_skip_dirs_and_marker_rules() {
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        for nested in ["node_modules/pkg", ".cachex/r", "deep/a/b", "shallow"] {
            fs::create_dir_all(root.join(nested).join(".git")).unwrap();
        }
        fs::create_dir(root.join("big")).unwrap();
        write(&root.join("big/.git"), &[b' '; 5000]);
        fs::create_dir(root.join("spaced")).unwrap();
        write(&root.join("spaced/.git"), b" \n\tgitdir: x\n");
        let mut warnings = Vec::new();
        let found = find_git_repositories(&[spelling(&root)], 2, &mut warnings);
        assert!(found.contains(&root.join("shallow")));
        assert!(found.contains(&root.join("spaced")));
        assert!(!found.contains(&root.join("big")));
        assert!(
            !found
                .iter()
                .any(|p| p.starts_with(root.join("node_modules")))
        );
        assert!(!found.iter().any(|p| p.starts_with(root.join(".cachex"))));
        assert!(!found.contains(&root.join("deep/a/b")));
        let deeper = find_git_repositories(&[spelling(&root)], 3, &mut warnings);
        assert!(deeper.contains(&root.join("deep/a/b")));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn unreadable_directories_are_recorded_not_fatal() {
        if running_as_root() {
            eprintln!("skipped: permission checks do not apply to root");
            return;
        }
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("denied/inner")).unwrap();
        fs::create_dir_all(root.join("ok/.git")).unwrap();
        chmod(&root.join("denied"), 0o000);
        let mut warnings = Vec::new();
        let found = find_git_repositories(&[spelling(&root)], 2, &mut warnings);
        chmod(&root.join("denied"), 0o755);
        assert_eq!(found, vec![root.join("ok")]);
        assert_eq!(
            warnings,
            vec![format!(
                "inventory-error: {}: Permission denied",
                root.join("denied").display()
            )]
        );
    }

    // tests/test_collectors.py::test_collect_env_files_does_not_emit_values
    #[test]
    fn test_collect_env_files_does_not_emit_values() {
        let dir = tmp();
        write(
            &dir.path().join(".env"),
            b"API_TOKEN=secret-value\nPUBLIC_NAME=demo\n",
        );
        let mut warnings = Vec::new();
        let items = collect_env_files(&[spelling(dir.path())], 1, true, &mut warnings);
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.metadata.get("var_count"), Some("2"));
        assert!(
            item.metadata
                .get("var_names")
                .unwrap()
                .contains("API_TOKEN")
        );
        assert!(!format!("{item:?}").contains("secret-value"));
    }

    // tests/test_collectors.py::test_find_env_files_rejects_external_and_internal_symlinks
    #[test]
    fn test_find_env_files_rejects_external_and_internal_symlinks() {
        let parent = tmp();
        let root = fs::canonicalize(parent.path()).unwrap().join("root");
        fs::create_dir(&root).unwrap();
        let external = root.parent().unwrap().join("root-external.env");
        write(&external, b"TOKEN=outside\n");
        let internal = root.join("real.env");
        write(&internal, b"TOKEN=inside\n");
        symlink(&external, root.join(".env.external")).unwrap();
        symlink(&internal, root.join(".env.internal")).unwrap();

        let mut warnings = Vec::new();
        let found = find_env_files(&[spelling(&root)], 1, &mut warnings);

        assert_eq!(found, vec![internal]);
    }

    // tests/test_collectors.py::test_env_read_is_bounded_and_disappearance_is_recorded
    #[test]
    fn test_env_read_is_bounded_and_disappearance_is_recorded() {
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        let mut large = b"FIRST=ok\n".to_vec();
        large.extend(std::iter::repeat_n(b'X', MAX_ENV_BYTES + 10));
        write(&root.join(".env.large"), &large);
        let gone = root.join(".env.gone");
        write(&gone, b"GONE=yes\n");
        write(&root.join(".env.healthy"), b"HEALTHY=yes\n");

        let mut warnings = Vec::new();
        let mut disappear_then_read = |path: &Path, warnings: &mut Vec<String>| {
            if path == gone && path.exists() {
                fs::remove_file(path).unwrap();
            }
            env_names(path, warnings)
        };
        let items = collect_env_files_with(
            &[spelling(&root)],
            1,
            true,
            &mut warnings,
            &mut disappear_then_read,
        );

        let by_name = |name: &str| items.iter().find(|item| item.name == name).unwrap();
        assert!(
            by_name(".env.healthy")
                .metadata
                .get("var_names")
                .unwrap()
                .contains("HEALTHY")
        );
        assert!(
            by_name(".env.large")
                .metadata
                .get("var_names")
                .unwrap()
                .contains("FIRST")
        );
        assert_eq!(by_name(".env.gone").metadata.get("var_count"), Some("0"));
        assert!(warnings.iter().any(|w| w.contains("env-file-truncated")));
        assert!(warnings.iter().any(|w| w.contains("inventory-race")));
    }

    // tests/test_collectors.py::test_env_permission_error_does_not_abort_other_inventory
    #[test]
    fn test_env_permission_error_does_not_abort_other_inventory() {
        if running_as_root() {
            eprintln!("skipped: permission checks do not apply to root");
            return;
        }
        let dir = tmp();
        let denied = dir.path().join(".env.denied");
        write(&denied, b"DENIED=yes\n");
        chmod(&denied, 0o000);
        write(&dir.path().join(".env.healthy"), b"HEALTHY=yes\n");

        let mut warnings = Vec::new();
        let items = collect_env_files(&[spelling(dir.path())], 1, true, &mut warnings);

        assert!(items.iter().any(|item| item.name == ".env.healthy"));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("inventory-error") && w.contains(".env.denied"))
        );
    }

    #[test]
    fn env_names_follow_reference_parsing_rules() {
        let dir = tmp();
        let path = dir.path().join(".envrc");
        write(
            &path,
            b"export A=1\n B = 2\n#C=3\n\xe2\x80\x83D=4\nE\x1f=5\n1F=6\n_G=7\nH=8\r\nI=9\x0bJ=10\n",
        );
        let mut warnings = Vec::new();
        assert_eq!(
            env_names(&path, &mut warnings),
            ["B", "D", "E", "H", "I", "J", "_G"]
        );
        let many = dir.path().join("many.env");
        let content: String = (0..1100).map(|i| format!("V{i}=1\n")).collect();
        write(&many, content.as_bytes());
        assert_eq!(env_names(&many, &mut warnings).len(), MAX_ENV_LINES);
        assert!(warnings.is_empty());
    }

    // tests/test_collectors.py::test_collect_wrappers_records_executables
    #[test]
    fn test_collect_wrappers_records_executables() {
        let dir = tmp();
        let wrapper = dir.path().join("my-tool");
        write(&wrapper, b"#!/usr/bin/env bash\nexit 0\n");
        chmod(&wrapper, 0o755);
        write(&dir.path().join("notes.txt"), b"not executable\n");

        let mut warnings = Vec::new();
        let items = collect_wrappers(&[spelling(dir.path())], &mut warnings);

        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, ["my-tool"]);
        assert_eq!(
            items[0].metadata.get("shebang"),
            Some("#!/usr/bin/env bash")
        );
        assert_eq!(items[0].metadata.get("size_bytes"), Some("27"));
    }

    #[test]
    fn shebang_reading_matches_text_mode_readline() {
        let dir = tmp();
        let cases: [(&str, &[u8], &str); 5] = [
            ("spaced", b"   #!/bin/sh -e  \r\nrest\n", "#!/bin/sh -e"),
            ("bom", b"\xef\xbb\xbf#!/bin/sh\n", ""),
            ("invalid", b"\xff#!/bin/inv\xe2\x82\n", "#!/bin/inv"),
            ("plain", b"no shebang\n", ""),
            ("empty", b"", ""),
        ];
        for (name, content, expected) in cases {
            let path = dir.path().join(name);
            write(&path, content);
            assert_eq!(read_shebang(&path), expected, "{name}");
        }
    }

    // tests/test_collectors.py::test_resolver_ignores_path_spoofing[systemctl|crontab|git]
    #[test]
    fn test_resolver_ignores_path_spoofing() {
        for name in ["systemctl", "crontab", "git"] {
            let dir = tmp();
            let approved_dir = dir.path().join("approved");
            let attacker_dir = dir.path().join("attacker");
            fs::create_dir(&approved_dir).unwrap();
            fs::create_dir(&attacker_dir).unwrap();
            let approved = approved_dir.join(name);
            let attacker = attacker_dir.join(name);
            for (path, marker) in [(&approved, "approved"), (&attacker, "attacker")] {
                write(path, format!("#!/bin/sh\necho {marker}\n").as_bytes());
                chmod(path, 0o755);
            }
            // The resolver never consults PATH; only explicit candidates count.
            let selected = resolve_from_candidates(&[approved.to_str().unwrap()]);
            assert_eq!(
                selected,
                Some(spelling(&fs::canonicalize(&approved).unwrap()))
            );
            assert_ne!(selected, Some(spelling(&attacker)));
        }
    }

    // tests/test_collectors.py::test_resolver_skips_when_approved_executable_is_absent
    #[test]
    fn test_resolver_skips_when_approved_executable_is_absent() {
        assert_eq!(
            resolve_from_candidates(&["/definitely/missing/oremap/git"]),
            None
        );
    }

    #[test]
    fn resolver_rejects_non_executable_candidates() {
        let dir = tmp();
        let plain = dir.path().join("git");
        write(&plain, b"#!/bin/sh\n");
        chmod(&plain, 0o644);
        assert_eq!(resolve_from_candidates(&[plain.to_str().unwrap()]), None);
        assert_eq!(
            resolve_from_candidates(&[dir.path().to_str().unwrap()]),
            None
        );
    }

    // tests/test_collectors.py::test_run_command_uses_minimal_environment_and_distinct_statuses
    #[test]
    fn test_run_command_uses_minimal_environment_and_distinct_statuses() {
        let leaky =
            |name: &str| (name == "PRIVATE_SCANNER_VALUE").then(|| "must-not-leak".to_string());
        let environment = run_command_with_env(
            &[
                "/bin/sh".into(),
                "-c".into(),
                "echo ${PRIVATE_SCANNER_VALUE:-clean}; env | sort".into(),
            ],
            2.0,
            &leaky,
        );
        let nonzero = run_command(&["/bin/sh".into(), "-c".into(), "exit 7".into()], 2.0);
        let timed_out = run_command(&["/bin/sh".into(), "-c".into(), "sleep 2".into()], 0.05);
        let dir = tmp();
        let denied = dir.path().join("denied");
        write(&denied, b"no shebang\n");
        chmod(&denied, 0o600);
        let permission = run_command(&[denied.clone().into_os_string()], 2.0);

        let lines: Vec<&str> = environment.stdout.lines().collect();
        assert_eq!(lines[0], "clean");
        assert!(!environment.stdout.contains("must-not-leak"));
        for line in &lines[1..] {
            let key = line.split('=').next().unwrap();
            assert!(
                ["LANG", "LC_ALL", "PATH", "PWD", "SHLVL", "_"].contains(&key),
                "unexpected variable {line}"
            );
        }
        assert_eq!((nonzero.status.as_str(), nonzero.code), ("nonzero", 7));
        assert_eq!(
            (timed_out.status.as_str(), timed_out.code),
            ("timeout", 124)
        );
        assert_eq!(
            timed_out.stderr,
            "Command '['/bin/sh', '-c', 'sleep 2']' timed out after 0.05 seconds"
        );
        assert_eq!(
            (permission.status.as_str(), permission.code),
            ("exec-error", 126)
        );
        assert_eq!(
            permission.stderr,
            format!("[Errno 13] Permission denied: '{}'", denied.display())
        );
        let missing = run_command(&["/definitely/missing/tool".into()], 2.0);
        assert_eq!((missing.status.as_str(), missing.code), ("exec-error", 127));
        assert_eq!(
            missing.stderr,
            "[Errno 2] No such file or directory: '/definitely/missing/tool'"
        );
    }

    #[test]
    fn subprocess_environment_is_minimal() {
        let none = |_: &str| -> Option<String> { Some("x".into()) };
        let env = subprocess_env(&["/usr/bin/crontab".into(), "-l".into()], &none);
        assert_eq!(
            env,
            SAFE_SUBPROCESS_ENV
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        );
        // System scope never receives the user bus.
        let env = subprocess_env(&["/usr/bin/systemctl".into(), "list-units".into()], &none);
        assert_eq!(env.len(), 3);
        let env = subprocess_env(&["/usr/bin/git".into(), "status".into()], &none);
        assert_eq!(
            env.last(),
            Some(&("GIT_OPTIONAL_LOCKS".to_string(), "0".to_string()))
        );
    }

    // tests/test_collectors.py::test_user_systemctl_receives_only_required_bus_environment
    #[test]
    fn test_user_systemctl_receives_only_required_bus_environment() {
        let dir = tmp();
        let systemctl = dir.path().join("systemctl");
        write(
            &systemctl,
            b"#!/bin/sh\nprintf '%s|%s|%s\\n' \"${DBUS_SESSION_BUS_ADDRESS-None}\" \"${XDG_RUNTIME_DIR-None}\" \"${PRIVATE_SCANNER_VALUE-None}\"\n",
        );
        chmod(&systemctl, 0o755);
        let parent_env = |name: &str| match name {
            "DBUS_SESSION_BUS_ADDRESS" => Some("unix:path=/run/user/1000/bus".to_string()),
            "XDG_RUNTIME_DIR" => Some("/run/user/1000".to_string()),
            "PRIVATE_SCANNER_VALUE" => Some("must-not-leak".to_string()),
            _ => None,
        };
        let result = run_command_with_env(
            &[
                systemctl.clone().into_os_string(),
                "--user".into(),
                "list-units".into(),
            ],
            2.0,
            &parent_env,
        );
        assert_eq!(result.code, 0);
        assert_eq!(
            result.stdout,
            "unix:path=/run/user/1000/bus|/run/user/1000|None\n"
        );
    }

    // tests/test_collectors.py::test_external_collectors_record_executable_version
    #[test]
    fn test_external_collectors_record_executable_version() {
        let dir = tmp();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let git = fake(|args| {
            if args.iter().any(|a| a == "status") {
                CommandResult::new(0, "", "", &args[0])
            } else if args.iter().any(|a| a == "log") {
                CommandResult::new(0, "2026-01-01T00:00:00Z\n", "", &args[0])
            } else {
                CommandResult::new(0, "main\n", "", &args[0])
            }
        });

        let (items, warnings) = collect_repositories(&git, &[spelling(&repo)], 1, 1.0);
        assert!(warnings.is_empty());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].metadata.get("executable"), Some("/usr/bin/tool"));
        assert_eq!(
            items[0].metadata.get("executable_version"),
            Some("tool 1.2.3")
        );
        assert_eq!(items[0].metadata.get("branch"), Some("main"));
        assert_eq!(items[0].metadata.get("dirty_count"), Some("0"));
        assert_eq!(items[0].status, "clean");
        assert_eq!(items[0].last_activity, "2026-01-01T00:00:00Z");

        let unit_dir = dir.path().join("units");
        fs::create_dir(&unit_dir).unwrap();
        write(&unit_dir.join("demo.service"), b"[Service]\n");
        let (systemd_items, systemd_warnings) =
            collect_systemd(&git, "user", &[spelling(&unit_dir)], false, 1.0);
        assert!(systemd_warnings.is_empty());
        assert_eq!(
            systemd_items[0].metadata.get("executable_version"),
            Some("tool 1.2.3")
        );

        let cron = fake(|args| CommandResult::new(0, "* * * * * echo ok\n", "", &args[0]));
        let (cron_items, cron_warnings) = collect_cron(&cron, 1.0, false);
        assert!(cron_warnings.is_empty());
        assert_eq!(
            cron_items[0].metadata.get("executable_version"),
            Some("tool 1.2.3")
        );
    }

    // tests/test_collectors.py::test_probe_executable_version_is_bounded
    #[test]
    fn test_probe_executable_version_is_bounded() {
        let dir = tmp();
        let executable = dir.path().join("tool");
        write(&executable, b"#!/bin/sh\nprintf 'tool 1.2.3\\n'\n");
        chmod(&executable, 0o755);
        assert_eq!(
            probe_executable_version("tool", &spelling(&executable), 1.0),
            "tool 1.2.3"
        );
    }

    #[test]
    fn probe_cleans_truncates_and_reports_failures() {
        let noisy = fake(|args| {
            CommandResult::new(
                0,
                &format!("\n \x01 v\t1  {}\n", "x".repeat(300)),
                "",
                &args[0],
            )
        });
        let probed = probe_with(&noisy, "t", "/t", 1.0);
        assert!(probed.starts_with("v 1 xxx"));
        assert_eq!(char_len(&probed), 160);
        let stderr_only = fake(|args| CommandResult::new(0, "", "tool 9\n", &args[0]));
        assert_eq!(probe_with(&stderr_only, "t", "/t", 1.0), "tool 9");
        let empty = fake(|args| CommandResult::new(0, "  \n", "", &args[0]));
        assert_eq!(probe_with(&empty, "t", "/t", 1.0), "unavailable:empty");
        let failing = fake(|args| CommandResult::new(3, "", "", &args[0]));
        assert_eq!(
            probe_with(&failing, "t", "/t", 1.0),
            "unavailable:nonzero:3"
        );
    }

    #[test]
    fn cron_lines_are_parsed_and_redacted() {
        let crontab = "# comment\nSHELL=/bin/sh\n  \n@reboot /home/x/run --token abc\n*/5 * * * * echo TOKEN=zzz\nbroken line\n0 0 * * * "
            .to_string()
            + &"y".repeat(200)
            + "\n";
        let exec = fake(move |args| CommandResult::new(0, &crontab, "", &args[0]));
        let (items, warnings) = collect_cron(&exec, 1.0, true);
        assert!(warnings.is_empty());
        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "user-crontab:4",
                "user-crontab:5",
                "user-crontab:6",
                "user-crontab:7"
            ]
        );
        assert_eq!(items[0].metadata.get("schedule"), Some("@reboot"));
        assert_eq!(
            items[0].metadata.get("command_preview"),
            Some("/home/x/run --token <redacted>")
        );
        assert_eq!(items[1].metadata.get("schedule"), Some("*/5 * * * *"));
        assert_eq!(
            items[1].metadata.get("command_preview"),
            Some("echo TOKEN=<redacted>")
        );
        assert_eq!(items[2].metadata.get("schedule"), Some("unknown"));
        assert_eq!(
            items[2].metadata.get("command_preview"),
            Some("broken line")
        );
        let preview = items[3].metadata.get("command_preview").unwrap();
        assert_eq!(char_len(preview), 160);
        assert!(preview.ends_with("..."));
        let keys: Vec<&str> = items[0].metadata.iter().map(|(k, _)| k).collect();
        assert_eq!(
            keys,
            [
                "schedule",
                "command_preview",
                "executable",
                "executable_version"
            ]
        );
    }

    #[test]
    fn cron_failures_and_missing_crontab() {
        let none = fake(|args| CommandResult::new(1, "", "no crontab for user\n", &args[0]));
        assert_eq!(collect_cron(&none, 1.0, false), (Vec::new(), Vec::new()));
        let broken = fake(|args| CommandResult::new(1, "", " boom \n", &args[0]));
        assert_eq!(
            collect_cron(&broken, 1.0, false).1,
            vec!["crontab -l failed: boom".to_string()]
        );
        let mut missing = fake(|args| CommandResult::new(0, "", "", &args[0]));
        missing.executable = None;
        assert_eq!(
            collect_cron(&missing, 1.0, false).1,
            vec!["crontab approved executable not found".to_string()]
        );
    }

    #[test]
    fn systemd_merges_listing_state_in_reference_order() {
        let dir = tmp();
        let units = dir.path().join("units");
        fs::create_dir(&units).unwrap();
        for name in [
            "b.service",
            "a.timer",
            ".hidden.service",
            "noext",
            "x.txt",
            ".service",
        ] {
            write(&units.join(name), b"");
        }
        let exec = fake(|args| {
            if args.iter().any(|a| a == "list-unit-files") {
                CommandResult::new(
                    0,
                    "a.timer enabled enabled\nz.service static -\n",
                    "",
                    &args[0],
                )
            } else {
                CommandResult::new(1, "", "bus unavailable\n", &args[0])
            }
        });
        let (items, warnings) = collect_systemd(&exec, "user", &[spelling(&units)], true, 1.0);
        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(
            names,
            [".hidden.service", "a.timer", "b.service", "z.service"]
        );
        assert_eq!(items[1].enabled, "enabled");
        assert_eq!(items[3].metadata.get("source"), Some("systemctl"));
        assert_eq!(items[3].path, "");
        assert_eq!(
            warnings,
            vec!["systemctl list-units failed: bus unavailable".to_string()]
        );
        let (only_files, _) = collect_systemd(&exec, "user", &[spelling(&units)], false, 1.0);
        assert_eq!(only_files.len(), 3);

        let mut missing = fake(|args| CommandResult::new(0, "", "", &args[0]));
        missing.executable = None;
        let (items, warnings) = collect_systemd(&missing, "user", &[spelling(&units)], true, 1.0);
        assert_eq!(items.len(), 3);
        assert_eq!(
            warnings,
            vec!["systemctl approved executable not found".to_string()]
        );
    }

    #[test]
    fn git_receives_non_utf8_repository_paths_unchanged() {
        use std::os::unix::ffi::OsStringExt;
        use std::sync::Mutex;

        struct Recorder(Mutex<Vec<Vec<OsString>>>);
        impl Exec for Recorder {
            fn resolve(&self, _name: &str) -> Option<String> {
                Some("/usr/bin/git".into())
            }
            fn run(&self, args: &[OsString], _timeout: f64) -> CommandResult {
                if let Ok(mut calls) = self.0.lock() {
                    calls.push(args.to_vec());
                }
                CommandResult::new(0, "", "", "/usr/bin/git")
            }
            fn probe(&self, _name: &str, _path: &str, _timeout: f64) -> String {
                "git version 2".into()
            }
        }

        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        let repo = root.join(OsString::from_vec(b"r\xffepo".to_vec()));
        fs::create_dir_all(repo.join(".git")).unwrap();
        let recorder = Recorder(Mutex::new(Vec::new()));
        let (items, warnings) = collect_repositories(&recorder, &[spelling(&root)], 1, 1.0);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "r\u{fffd}epo");
        let calls = recorder.0.into_inner().unwrap();
        assert_eq!(calls.len(), 3);
        for call in calls {
            assert_eq!(call[2], repo.clone().into_os_string());
        }
    }

    #[test]
    fn repository_status_failures_are_warnings() {
        let dir = tmp();
        let repo = fs::canonicalize(dir.path()).unwrap().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let exec = fake(|args| {
            if args.iter().any(|a| a == "status") {
                CommandResult::new(128, "", "fatal: nope\n", &args[0])
            } else if args.iter().any(|a| a == "log") {
                CommandResult::new(128, "", "", &args[0])
            } else {
                CommandResult::new(0, " M a\n\n?? b\n", "", &args[0])
            }
        });
        let (items, warnings) = collect_repositories(&exec, &[spelling(&repo)], 1, 1.0);
        assert_eq!(items[0].status, "unknown");
        assert!(!items[0].last_activity.is_empty());
        assert_eq!(
            warnings,
            vec![format!(
                "git status failed for {}: fatal: nope",
                repo.display()
            )]
        );
    }

    // tests/test_formatters.py::test_low_level_csv_writer_uses_the_same_policy
    #[test]
    fn test_low_level_csv_writer_uses_the_same_policy() {
        let dir = tmp();
        let destination = dir.path().join("report.csv");
        write_csv(
            &[vec![
                ("kind".to_string(), "=CMD()".to_string()),
                ("name".to_string(), "ordinary".to_string()),
            ]],
            &destination,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "kind,name\r\n'=CMD(),ordinary\r\n"
        );
        let empty = dir.path().join("nested/empty.csv");
        write_csv(&[], &empty).unwrap();
        assert_eq!(fs::read_to_string(&empty).unwrap(), "kind,name\r\n");
    }
}
