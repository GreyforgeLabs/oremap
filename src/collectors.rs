//! Read-only collectors for local workflow surfaces.
//!
//! Nothing here starts, stops, enables, or edits anything. Filesystem access
//! is limited to `stat`, directory listing, and bounded no-follow reads of
//! `.git` marker files, Git configuration and index files, and env files. External tools come only from a fixed
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

fn metadata_mtime_iso(metadata: &FsMetadata) -> String {
    isoformat_utc(mtime_micros(metadata.mtime(), metadata.mtime_nsec())).unwrap_or_default()
}

fn mtime_iso(path: &Path) -> String {
    // Python raised on errors other than FileNotFoundError; an empty value
    // keeps the scan going instead (documented in CHANGELOG 0.4.0).
    fs::metadata(path)
        .map(|metadata| metadata_mtime_iso(&metadata))
        .unwrap_or_default()
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
        // A partial clone fetches missing objects on demand, which runs the
        // repository's configured transport (`core.sshCommand`, a remote
        // helper) and contacts the network. Git 2.44 and later honour this;
        // older versions ignore it.
        environment.push(("GIT_NO_LAZY_FETCH".to_string(), "1".to_string()));
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
        .stdin(Stdio::null())
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
///
/// `systemctl` puts a status marker (`●`, or `*` in the C locale) in front
/// of failed, activating, and not-found units; it is skipped so those units
/// keep their state.
pub fn parse_systemctl_units(output: &str) -> Vec<(String, String)> {
    ordered_pairs(splitlines(output).into_iter().filter_map(|line| {
        let mut parts = pyfmt::split(line);
        if parts
            .first()
            .is_some_and(|first| first.chars().all(|c| !c.is_alphanumeric() && c != '.'))
        {
            parts.remove(0);
        }
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

/// One explicitly requested directory (a unit or wrapper directory) and its
/// entry names, sorted by display spelling and then by raw bytes.
struct ListedDir {
    spelling: String,
    path: PathBuf,
    names: Vec<(String, OsString)>,
}

/// List an explicit directory. A missing path or a non-directory is skipped;
/// a directory that exists but cannot be listed is recorded as an
/// `inventory-error` warning, so the report is marked incomplete.
fn list_explicit_dir(spelling: &str, warnings: &mut Vec<String>) -> Option<ListedDir> {
    let expanded = paths::expanduser(spelling).ok()?;
    let path = paths::to_path(&expanded);
    if !fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
        return None;
    }
    let listed = fs::read_dir(&path).and_then(|entries| {
        entries
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<OsString>>>()
    });
    let names = match listed {
        Ok(names) => names,
        Err(error) => {
            record_inventory_error(warnings, &expanded, &error);
            return None;
        }
    };
    let mut names: Vec<(String, OsString)> = names
        .into_iter()
        .map(|name| (name.to_string_lossy().into_owned(), name))
        .collect();
    names.sort();
    Some(ListedDir {
        spelling: expanded,
        path,
        names,
    })
}

fn unit_files(dir_spelling: &str, scope: &str, warnings: &mut Vec<String>) -> Vec<InventoryItem> {
    let Some(listed) = list_explicit_dir(dir_spelling, warnings) else {
        return Vec::new();
    };
    listed
        .names
        .into_iter()
        .filter(|(name, _)| name.contains('.') && UNIT_SUFFIXES.contains(&unit_suffix(name)))
        .map(|(name, raw)| {
            let mut item = InventoryItem::new("systemd_unit", name);
            item.path = paths::join(&listed.spelling, &item.name);
            // Stat the real name; the display spelling may contain U+FFFD.
            item.last_activity = mtime_iso(&listed.path.join(&raw));
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
        for item in unit_files(unit_dir, scope, &mut warnings) {
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

/// The command text cron runs. Cron turns the first `%` that is not
/// escaped as `\\%` into a newline and feeds the rest to the command's
/// standard input, which often carries message bodies or credentials, so
/// that part is replaced with `%<redacted>`.
fn cron_command_text(command: &str) -> String {
    let stdin_start = command
        .char_indices()
        .find(|(index, c)| *c == '%' && !command[..*index].ends_with('\\'))
        .map(|(index, _)| index);
    match stdin_start {
        Some(index) => format!("{}%<redacted>", &command[..index]),
        None => command.to_string(),
    }
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
        let mut preview = redact_command(&cron_command_text(&command), absolute_paths);
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

/// `#!` lines longer than this are cut (the kernel reads only 256 bytes).
const MAX_SHEBANG_BYTES: u64 = 64 * 1024;

/// False once the bytes read so far can no longer begin with `#!` after
/// invalid UTF-8 is dropped and leading whitespace is stripped.
fn could_start_shebang(prefix: &[u8]) -> bool {
    let decoded = pyfmt::decode_ignore(prefix);
    let mut chars = decoded.trim_start_matches(pyfmt::is_space).chars();
    matches!(
        (chars.next(), chars.next()),
        (None, _) | (Some('#'), None) | (Some('#'), Some('!'))
    )
}

/// First line of a wrapper when it is a `#!` line. Reading stops at the
/// first line break, as soon as the file cannot start with `#!` (binaries
/// stop after one block), or after [`MAX_SHEBANG_BYTES`].
fn read_shebang(path: &Path) -> String {
    let Ok(file) = File::open(path) else {
        return String::new();
    };
    let mut file = file.take(MAX_SHEBANG_BYTES);
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
        if !could_start_shebang(&line) {
            return String::new();
        }
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
        let Some(listed) = list_explicit_dir(wrapper_dir, warnings) else {
            continue;
        };
        for (display_name, raw_name) in listed.names {
            let path = listed.path.join(&raw_name);
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() || !is_executable(&path) {
                continue;
            }
            let mut item = InventoryItem::new("wrapper", display_name);
            item.metadata.set("size_bytes", metadata.len().to_string());
            let shebang = read_shebang(&path);
            if !shebang.is_empty() {
                item.metadata.set("shebang", shebang);
            }
            item.path = paths::join(&listed.spelling, &item.name);
            item.status = "executable".into();
            item.last_activity = metadata_mtime_iso(&metadata);
            items.push(item);
        }
    }
    items
}

// ---------------------------------------------------------------------------
// Directory walking (os.walk, topdown, followlinks=False)
// ---------------------------------------------------------------------------

/// Walk callback: directory, depth, prunable sub-directory names, other names
/// (files, symlinks, and everything else).
type Visit<'a> = dyn FnMut(&Path, usize, &mut Vec<OsString>, &[OsString]) + 'a;

/// Depth-first, top-down walk that never follows symlinks (`os.walk` with
/// `followlinks=False`). Entry types come from the directory listing, so a
/// directory costs one listing and no per-entry `stat`.
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
            // A symlink is never entered, so it is listed with the other
            // names without following it; both visitors reject symlinks.
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
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
            stack.push((directory.join(name), depth + 1));
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
    discover_git_repositories(repo_roots, max_depth, warnings)
        .into_iter()
        .map(|(path, _bare)| path)
        .collect()
}

/// [`find_git_repositories`] that also says whether each repository is bare.
fn discover_git_repositories(
    repo_roots: &[String],
    max_depth: i64,
    warnings: &mut Vec<String>,
) -> Vec<(PathBuf, bool)> {
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
            &mut |current, depth, dirnames, files| {
                let mut sink = warnings_cell.borrow_mut();
                // The listing says which marker names exist, so only those
                // are inspected; a directory without them costs no syscalls.
                let listed = |name: &[u8], among: &[OsString]| {
                    among.iter().any(|entry| entry.as_bytes() == name)
                };
                let subdirs: &[OsString] = dirnames;
                let worktree = (listed(b".git", subdirs) || listed(b".git", files))
                    && has_safe_git_marker(current, &mut sink);
                let bare = !worktree
                    && listed(b"HEAD", files)
                    && listed(b"objects", subdirs)
                    && looks_like_bare_git_repo(current, &mut sink);
                if worktree || bare {
                    match fs::canonicalize(current) {
                        Ok(resolved) => {
                            if seen.insert(resolved) {
                                repos.push((current.to_path_buf(), bare));
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

/// Results of the read-only `git` queries for one repository.
struct RepoProbe {
    /// `None` when the branch could not be determined (unborn or failed).
    branch: Option<String>,
    /// `git status`; `None` for a bare repository, which has no work tree.
    status: Option<CommandResult>,
    last_commit: CommandResult,
}

/// Every `git` call disables `core.fsmonitor`: a repository that enables
/// it would otherwise make `git status` start a background
/// `fsmonitor--daemon` and write its state under `.git`. The empty value
/// means "off" in every Git version.
const GIT_READ_ONLY_CONFIG: [&str; 2] = ["-c", "core.fsmonitor="];

/// Git's system configuration file for the approved `/usr/bin/git` builds.
const SYSTEM_GIT_CONFIG: &str = "/etc/gitconfig";
/// A configuration file larger than this is assumed to define a filter.
const MAX_CONFIG_SCAN_BYTES: usize = 1024 * 1024;
/// Lists the clean and process filter drivers configured for a repository.
/// It reads configuration files only and never runs a driver.
const FILTER_DRIVER_QUERY: [&str; 5] = [
    "config",
    "--null",
    "--name-only",
    "--get-regexp",
    r"^filter\..*\.(clean|process)$",
];

/// Whether a Git configuration file mentions one of `words` (ASCII
/// lower case, matched in any letter case). A missing file does not; a
/// symlink, or a file that cannot be read in full, is assumed to.
fn config_mentions(path: &Path, words: &[&[u8]]) -> bool {
    let mut file = match open_no_follow(path) {
        Ok(file) => file,
        Err(error) => return error.kind() != io::ErrorKind::NotFound,
    };
    match read_bounded(&mut file, MAX_CONFIG_SCAN_BYTES + 1) {
        Ok(content) if content.len() <= MAX_CONFIG_SCAN_BYTES => {
            let content = content.to_ascii_lowercase();
            words
                .iter()
                .any(|word| content.windows(word.len()).any(|window| window == *word))
        }
        _ => true,
    }
}

/// Whether a Git configuration file could define a filter driver: it
/// mentions `filter` (a `[filter "name"]` section, in any letter case) or
/// `include` (which can read another file). A missing file cannot; a
/// symlink, or a file that cannot be read in full, is assumed to.
fn config_may_define_filters(path: &Path) -> bool {
    config_mentions(path, &[b"filter", b"include"])
}

/// Whether a Git configuration file could move its work tree with
/// `core.worktree`: it mentions `worktree` or `include`.
fn config_may_move_work_tree(path: &Path) -> bool {
    config_mentions(path, &[b"worktree", b"include"])
}

/// The Git directories behind one work tree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GitDirs {
    /// The work tree's own directory, with its `index`,
    /// `config.worktree`, and `modules/`.
    git_dir: PathBuf,
    /// The common directory, with the repository `config`.
    common_dir: PathBuf,
}

impl GitDirs {
    /// The repository configuration files Git reads for the work tree:
    /// `config` in the common directory and `config.worktree` in its own.
    fn config_files(&self) -> [PathBuf; 2] {
        [
            self.common_dir.join("config"),
            self.git_dir.join("config.worktree"),
        ]
    }
}

/// `bytes` without trailing line breaks, as Git trims `.git` files and
/// `commondir`.
fn trim_line_breaks(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|b| !matches!(b, b'\n' | b'\r'))
        .map_or(0, |last| last + 1);
    &bytes[..end]
}

/// The Git directories of the work tree at `work_tree`, found from its
/// `.git` the way Git reads it: a directory, or a file holding
/// `gitdir: <path>` (a linked worktree or a submodule checkout, relative
/// to `work_tree`), plus an optional `commondir`. `None` when that cannot
/// be worked out.
fn resolve_git_dirs(work_tree: &Path) -> Option<GitDirs> {
    let marker = work_tree.join(".git");
    let kind = fs::symlink_metadata(&marker).ok()?.file_type();
    let git_dir = if kind.is_dir() {
        marker
    } else if kind.is_file() {
        let mut file = open_no_follow(&marker).ok()?;
        let content = read_bounded(&mut file, MAX_GIT_MARKER_BYTES as usize).ok()?;
        let target = content.strip_prefix(b"gitdir: ")?;
        work_tree.join(OsStr::from_bytes(trim_line_breaks(target)))
    } else {
        return None;
    };
    let common_dir = match open_no_follow(&git_dir.join("commondir")) {
        Ok(mut file) => {
            let content = read_bounded(&mut file, MAX_GIT_MARKER_BYTES as usize).ok()?;
            git_dir.join(OsStr::from_bytes(trim_line_breaks(&content)))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => git_dir.clone(),
        Err(_) => return None,
    };
    Some(GitDirs {
        git_dir,
        common_dir,
    })
}

/// Whether the configuration Git reads for a work tree could define a
/// filter driver. Unknown Git directories (`None`) are assumed to.
fn repository_may_define_filters(dirs: Option<&GitDirs>, system_config_may: bool) -> bool {
    system_config_may
        || dirs.is_none_or(|dirs| {
            dirs.config_files()
                .iter()
                .any(|path| config_may_define_filters(path))
        })
}

/// An index file larger than this is assumed to list a gitlink.
const MAX_INDEX_SCAN_BYTES: usize = 64 * 1024 * 1024;

/// Whether `mode` is a gitlink the way Git tests it (`S_ISGITLINK`): only
/// the file-type bits count, so `0o160755` is one too.
fn is_gitlink_mode(mode: u32) -> bool {
    mode & 0o170000 == 0o160000
}

/// Walks an index file laid out for object names of `hash_len` bytes, the
/// way Git reads it: `Some((entries, split))` when the layout fits, where
/// `entries` says an entry is a gitlink or the index is sparse (an `sdir`
/// extension folds directories into tree entries), and `split` that it is
/// a split index (`link` extension).
fn walk_index(content: &[u8], hash_len: usize) -> Option<(bool, bool)> {
    let be32 = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(
            content.get(at..at.checked_add(4)?)?.try_into().ok()?,
        ))
    };
    if content.get(..4)? != b"DIRC" {
        return None;
    }
    let version = be32(4)?;
    if !(2..=4).contains(&version) {
        return None;
    }
    let count = be32(8)?;
    let end = content.len().checked_sub(hash_len)?;
    let nul_after = |at: usize| content.get(at..end)?.iter().position(|b| *b == 0);
    let (mut entries, mut split) = (false, false);
    let mut at = 12;
    for _ in 0..count {
        // ctime, mtime, dev, ino, then the mode; flags follow the name.
        entries |= is_gitlink_mode(be32(at + 24)?);
        let flags_at = at + 40 + hash_len;
        let flags = u16::from_be_bytes(content.get(flags_at..flags_at + 2)?.try_into().ok()?);
        let mut name_at = flags_at + 2;
        if flags & 0x4000 != 0 {
            if version < 3 {
                return None;
            }
            name_at += 2;
        }
        at = if version == 4 {
            // A varint (the prefix length taken from the previous name),
            // then the rest of the name, NUL-terminated.
            while content.get(name_at)? & 0x80 != 0 {
                name_at += 1;
            }
            name_at + 1 + nul_after(name_at + 1)? + 1
        } else {
            let length = match usize::from(flags & 0x0fff) {
                0x0fff => nul_after(name_at)?,
                length => length,
            };
            // Padded with 1 to 8 NULs to a multiple of 8 bytes.
            at + ((name_at - at + length + 8) & !7)
        };
        if at > end {
            return None;
        }
    }
    // Extensions: a 4-byte signature and a 4-byte size, while 8 bytes fit.
    while end - at >= 8 {
        match content.get(at..at + 4)? {
            b"link" => split = true,
            b"sdir" => entries = true,
            _ => {}
        }
        at = at.checked_add(8 + usize::try_from(be32(at + 4)?).ok()?)?;
        if at > end {
            return None;
        }
    }
    Some((entries, split))
}

/// What one index file shows: `Some((entries, split))` as in
/// [`walk_index`], for SHA-1 or SHA-256 object names (both, when both
/// layouts fit); `None` when it cannot be read in full or fits neither
/// layout. A missing file lists nothing.
fn index_file_hints(path: &Path) -> Option<(bool, bool)> {
    let mut file = match open_no_follow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Some((false, false)),
        Err(_) => return None,
    };
    let content = read_bounded(&mut file, MAX_INDEX_SCAN_BYTES + 1).ok()?;
    if content.len() > MAX_INDEX_SCAN_BYTES {
        return None;
    }
    [20, 32]
        .into_iter()
        .filter_map(|hash_len| walk_index(&content, hash_len))
        .reduce(|(entries, split), (more, also)| (entries || more, split || also))
}

/// Whether the index in `git_dir` could list a gitlink (a submodule
/// entry); see [`index_file_hints`]. A split index keeps most entries in
/// a `sharedindex.<hash>` file beside it, which is checked too; Git
/// cannot read a split index whose shared file is missing. A symlinked,
/// unreadable, or oversized file is assumed to list one.
fn index_may_list_gitlinks(git_dir: &Path) -> bool {
    let Some((entries, split)) = index_file_hints(&git_dir.join("index")) else {
        return true;
    };
    if entries || !split {
        return entries;
    }
    let Ok(listing) = fs::read_dir(git_dir) else {
        return true;
    };
    for entry in listing {
        let Ok(entry) = entry else { return true };
        if entry.file_name().as_bytes().starts_with(b"sharedindex.")
            && index_file_hints(&entry.path()).is_none_or(|(entries, _)| entries)
        {
            return true;
        }
    }
    false
}

/// Driver names from [`FILTER_DRIVER_QUERY`] output (NUL-terminated keys
/// such as `filter.lfs.clean`), or the reason one cannot be disabled
/// with `-c`: a name containing `=` would be split there, a name that
/// was not valid UTF-8 cannot be passed back unchanged, and a line break
/// may stand for a carriage return that output decoding replaced.
fn filter_driver_names(output: &str) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = Vec::new();
    for key in output.split('\0').filter(|key| !key.is_empty()) {
        let Some(name) = key.strip_prefix("filter.").and_then(|rest| {
            rest.strip_suffix(".clean")
                .or(rest.strip_suffix(".process"))
        }) else {
            continue;
        };
        if name.contains(['=', '\n', '\u{fffd}']) {
            return Err(format!(
                "filter driver {} cannot be disabled",
                repr_str(name)
            ));
        }
        if !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

/// Why a repository's `git status` is not run; reported as its failure.
#[derive(Debug)]
struct NotRun {
    code: i32,
    reason: String,
}

impl NotRun {
    fn new(reason: impl Into<String>) -> Self {
        NotRun {
            code: 1,
            reason: reason.into(),
        }
    }

    /// A failed query's exit code (never 0) and reason.
    fn from_result(result: &CommandResult, what: &str) -> Self {
        NotRun {
            code: if result.code == 0 { 1 } else { result.code },
            reason: format!("{what} failed: {}", strip(&result.stderr)),
        }
    }

    /// The same failure, attributed to the submodule at `shown`.
    fn in_submodule(self, shown: &str) -> Self {
        NotRun {
            code: self.code,
            reason: format!("submodule {}: {}", repr_str(shown), self.reason),
        }
    }
}

/// `git` arguments for a query in `dir`. Inside a submodule checkout the
/// query also passes `--git-dir=.git`, which is how `git status` starts
/// its own check there (`GIT_DIR=.git`), so configuration, conditional
/// includes, and `core.worktree` resolve exactly as they do for that
/// check.
fn git_query_args(git: &str, dir: &Path, submodule: bool, query: &[&str]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![git.into()];
    args.extend(GIT_READ_ONLY_CONFIG.iter().map(OsString::from));
    args.extend(["-C".into(), dir.as_os_str().to_os_string()]);
    if submodule {
        args.push("--git-dir=.git".into());
    }
    args.extend(query.iter().map(OsString::from));
    args
}

/// Add the filter drivers configured for the work tree in `dir` to
/// `names`. The query reads configuration files only and never runs a
/// driver.
fn query_filter_drivers(
    exec: &dyn Exec,
    git: &str,
    dir: &Path,
    submodule: bool,
    timeout: f64,
    names: &mut Vec<String>,
) -> Result<(), NotRun> {
    let result = exec.run(
        &git_query_args(git, dir, submodule, &FILTER_DRIVER_QUERY),
        timeout,
    );
    // `git config --get-regexp` exits 1 when nothing matches.
    let found = match result.code {
        0 => filter_driver_names(&result.stdout).map_err(NotRun::new)?,
        1 if strip(&result.stderr).is_empty() => Vec::new(),
        _ => return Err(NotRun::from_result(&result, "filter driver check")),
    };
    for name in found {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(())
}

/// Lists every index entry, including those outside the current
/// directory, with its mode; gitlinks have mode `160000`.
const GITLINK_QUERY: [&str; 6] = ["ls-files", "--stage", "-z", "--full-name", "--", ":/"];

/// Paths of the gitlinks (submodule entries) in the index of the work
/// tree in `dir`, relative to its top level, whether or not `.gitmodules`
/// mentions them: `git status` checks every populated one.
fn gitlink_paths(
    exec: &dyn Exec,
    git: &str,
    dir: &Path,
    submodule: bool,
    timeout: f64,
) -> Result<Vec<String>, NotRun> {
    let result = exec.run(
        &git_query_args(git, dir, submodule, &GITLINK_QUERY),
        timeout,
    );
    if result.code != 0 {
        return Err(NotRun::from_result(&result, "submodule check"));
    }
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for entry in result.stdout.split('\0').filter(|entry| !entry.is_empty()) {
        // `<mode> <object> <stage>\t<path>`
        let parsed = entry.split_once('\t').and_then(|(info, path)| {
            let mode = u32::from_str_radix(info.split(' ').next()?, 8).ok()?;
            Some((mode, path))
        });
        let Some((mode, path)) = parsed else {
            return Err(NotRun::new(
                "submodule check failed: unexpected git ls-files output",
            ));
        };
        // Unmerged entries list one path up to three times.
        if is_gitlink_mode(mode) && seen.insert(path) {
            paths.push(path.to_string());
        }
    }
    Ok(paths)
}

/// `path` as a relative path inside a work tree, or `None` when it could
/// not name the directory Git would check: it is absolute, has `.` or
/// `..` components, or lost bytes in output decoding (not valid UTF-8,
/// or a carriage return turned into a line break).
fn gitlink_relative_path(path: &str) -> Option<&Path> {
    let relative = Path::new(path);
    let plain = !path.is_empty()
        && !path.contains(['\n', '\u{fffd}'])
        && relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)));
    plain.then_some(relative)
}

/// The work tree Git uses for `dir`: `dir` itself, unless the
/// configuration could set `core.worktree`, in which case Git is asked.
fn effective_work_tree(
    exec: &dyn Exec,
    git: &str,
    dir: &Path,
    submodule: bool,
    dirs: &GitDirs,
    timeout: f64,
) -> Result<PathBuf, NotRun> {
    if !dirs
        .config_files()
        .iter()
        .any(|path| config_may_move_work_tree(path))
    {
        return Ok(dir.to_path_buf());
    }
    let query = ["rev-parse", "--show-toplevel"];
    let result = exec.run(&git_query_args(git, dir, submodule, &query), timeout);
    let top = result.stdout.strip_suffix('\n').unwrap_or(&result.stdout);
    if result.code != 0 {
        return Err(NotRun::from_result(&result, "work tree check"));
    }
    if !top.starts_with('/') || top.contains(['\n', '\u{fffd}']) {
        return Err(NotRun::new("work tree path cannot be checked"));
    }
    Ok(PathBuf::from(top))
}

/// Deepest submodule nesting that a scan follows.
const MAX_SUBMODULE_DEPTH: usize = 8;
/// Most populated submodules, at every depth, that a scan follows for one
/// repository.
const MAX_SUBMODULES: usize = 512;

/// `path` resolved, when it lies inside one of `roots` (resolved too).
fn resolved_inside(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let resolved = fs::canonicalize(path).ok()?;
    roots
        .iter()
        .any(|root| resolved.starts_with(root))
        .then_some(resolved)
}

/// Add the filter drivers configured in every populated submodule that
/// `git status` in `repo` would check, at every depth, to `names`.
///
/// `git status` checks a populated submodule by starting `git status`
/// inside it, which reads the submodule's own configuration and checks
/// its own submodules in turn. Those processes inherit the `-c`
/// overrides, so disabling every driver found here keeps them from
/// running any. Gitlinks come from the index, not `.gitmodules`. Each
/// submodule's `.git` must resolve, without a symlink, to a Git directory
/// inside the scanned repository's work tree or Git directories, and the
/// walk stops at [`MAX_SUBMODULE_DEPTH`] and [`MAX_SUBMODULES`]; anything
/// else is refused, so `git status` is not run.
fn submodule_filter_drivers(
    exec: &dyn Exec,
    git: &str,
    repo: &Path,
    dirs: &GitDirs,
    system_config_may: bool,
    timeout: f64,
    names: &mut Vec<String>,
) -> Result<(), NotRun> {
    // Where Git runs for a work tree, its Git directories, the path shown
    // in warnings (empty for the scanned repository), and its depth.
    let mut pending = vec![(repo.to_path_buf(), dirs.clone(), String::new(), 0)];
    // The scanned repository's work tree and Git directories, resolved;
    // set once its index lists a gitlink.
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    let mut populated = 0;
    while let Some((dir, dirs, shown, depth)) = pending.pop() {
        let submodule = depth > 0;
        let attribute = |failure: NotRun| {
            if submodule {
                failure.in_submodule(&shown)
            } else {
                failure
            }
        };
        let paths = gitlink_paths(exec, git, &dir, submodule, timeout).map_err(attribute)?;
        if paths.is_empty() {
            continue;
        }
        let tree =
            effective_work_tree(exec, git, &dir, submodule, &dirs, timeout).map_err(attribute)?;
        let tree = if submodule {
            resolved_inside(&tree, &roots).ok_or_else(|| {
                NotRun::new("work tree is outside the repository").in_submodule(&shown)
            })?
        } else {
            roots = [&tree, &dirs.git_dir, &dirs.common_dir]
                .iter()
                .map(fs::canonicalize)
                .collect::<Result<_, _>>()
                .map_err(|error| {
                    NotRun::new(format!(
                        "work tree or Git directory cannot be resolved: {}",
                        strerror(&error)
                    ))
                })?;
            tree
        };
        for path in paths {
            let shown = if submodule {
                format!("{shown}/{path}")
            } else {
                path.clone()
            };
            let refuse = |reason: &str| NotRun::new(reason).in_submodule(&shown);
            let Some(relative) = gitlink_relative_path(&path) else {
                return Err(refuse("path cannot be checked"));
            };
            let checkout = tree.join(relative);
            // Git checks a submodule only when its `.git` exists.
            match fs::symlink_metadata(checkout.join(".git")) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(refuse(".git is a symbolic link"));
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(refuse(&strerror(&error))),
            }
            populated += 1;
            if depth + 1 > MAX_SUBMODULE_DEPTH || populated > MAX_SUBMODULES {
                return Err(refuse("too many submodules to check"));
            }
            let Some(sub_dirs) = resolve_git_dirs(&checkout) else {
                return Err(refuse("git directory cannot be resolved"));
            };
            let resolved = [&checkout, &sub_dirs.git_dir, &sub_dirs.common_dir]
                .map(|path| resolved_inside(path, &roots));
            let [Some(checkout_id), Some(git_dir_id), Some(_)] = resolved else {
                return Err(refuse("git directory is outside the repository"));
            };
            if !seen.insert((checkout_id, git_dir_id)) {
                continue;
            }
            if repository_may_define_filters(Some(&sub_dirs), system_config_may) {
                query_filter_drivers(exec, git, &checkout, true, timeout, names)
                    .map_err(|failure| failure.in_submodule(&shown))?;
            }
            if index_may_list_gitlinks(&sub_dirs.git_dir) {
                pending.push((checkout, sub_dirs, shown, depth + 1));
            }
        }
    }
    Ok(())
}

/// `-c` arguments that switch off every filter driver configured for a
/// work tree and for each populated submodule `git status` would check in
/// it, so `git status` never runs a repository-defined command when it
/// re-reads a file whose cached stat data is stale. `Err` holds a failed
/// result to report instead of running `git status` at all.
fn filter_driver_overrides(
    exec: &dyn Exec,
    git: &str,
    repo: &Path,
    system_config_may: bool,
    timeout: f64,
) -> Result<Vec<OsString>, CommandResult> {
    let dirs = resolve_git_dirs(repo);
    let mut names = Vec::new();
    let collected = (|| {
        if repository_may_define_filters(dirs.as_ref(), system_config_may) {
            query_filter_drivers(exec, git, repo, false, timeout, &mut names)?;
        }
        // Fast path: an index without gitlinks costs no query.
        if dirs
            .as_ref()
            .is_none_or(|dirs| index_may_list_gitlinks(&dirs.git_dir))
        {
            let Some(dirs) = &dirs else {
                return Err(NotRun::new("git directory cannot be resolved"));
            };
            submodule_filter_drivers(
                exec,
                git,
                repo,
                dirs,
                system_config_may,
                timeout,
                &mut names,
            )?;
        }
        Ok(())
    })();
    match collected {
        Ok(()) => Ok(names
            .iter()
            .flat_map(|name| {
                ["clean=", "process=", "required=false"]
                    .map(|setting| ["-c".into(), format!("filter.{name}.{setting}").into()])
            })
            .flatten()
            .collect()),
        Err(failure) => Err(CommandResult {
            code: failure.code,
            stdout: String::new(),
            stderr: format!("not run: {}", failure.reason),
            executable: git.to_string(),
            status: "nonzero".to_string(),
        }),
    }
}

/// Branch from `git status --porcelain=v2 --branch` headers, spelled the
/// way `git rev-parse --abbrev-ref HEAD` prints it: `HEAD` when detached,
/// nothing for an unborn branch.
fn porcelain_v2_branch(output: &str) -> Option<String> {
    let mut oid = None;
    let mut head = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("# branch.oid ") {
            oid = Some(value);
        } else if let Some(value) = line.strip_prefix("# branch.head ") {
            head = Some(value);
        }
    }
    match (oid, head) {
        (Some("(initial)"), _) | (_, None) => None,
        (_, Some("(detached)")) => Some("HEAD".to_string()),
        (_, Some(name)) => Some(name.to_string()),
    }
}

fn probe_repository(
    exec: &dyn Exec,
    git: &str,
    repo: &Path,
    bare: bool,
    system_config_may: bool,
    timeout: f64,
) -> RepoProbe {
    let run_with = |config: &[OsString], extra: &[&str]| {
        let mut args: Vec<OsString> = vec![git.into()];
        args.extend(GIT_READ_ONLY_CONFIG.iter().map(OsString::from));
        args.extend(config.iter().cloned());
        args.extend(["-C".into(), repo.as_os_str().to_os_string()]);
        args.extend(extra.iter().map(OsString::from));
        exec.run(&args, timeout)
    };
    let run = |extra: &[&str]| run_with(&[], extra);
    let rev_parse_branch = || {
        let result = run(&["rev-parse", "--abbrev-ref", "HEAD"]);
        (result.code == 0).then(|| strip(&result.stdout).to_string())
    };
    // One `git status` reports both the branch and the changes; the
    // separate `rev-parse` runs only when there is no work tree or the
    // status query failed. Only `git status` can run filter drivers.
    let (branch, status) = if bare {
        (rev_parse_branch(), None)
    } else {
        let status = match filter_driver_overrides(exec, git, repo, system_config_may, timeout) {
            Ok(overrides) => run_with(&overrides, &["status", "--porcelain=v2", "--branch"]),
            Err(failed) => failed,
        };
        let branch = if status.code == 0 {
            porcelain_v2_branch(&status.stdout)
        } else {
            rev_parse_branch()
        };
        (branch, Some(status))
    };
    RepoProbe {
        branch,
        status,
        // `log.showSignature` in a repository's configuration would make
        // `git log` run its configured `gpg.program` on a signed commit.
        last_commit: run(&["log", "-1", "--no-show-signature", "--format=%cI"]),
    }
}

fn worker_count(jobs: usize) -> usize {
    let available = thread::available_parallelism().map_or(4, |n| n.get());
    jobs.min(available.clamp(2, 8)).max(1)
}

/// Find Git repositories under the requested roots and collect lightweight
/// state with read-only `git status --porcelain=v2 --branch` and
/// `git log -1` (plus `git rev-parse` for bare repositories). Filter
/// drivers configured for a work tree, and for every populated submodule
/// its `git status` checks, are switched off for that `git status`. That
/// adds one `git config` query for a repository whose configuration
/// mentions `filter` or `include`, and a `git ls-files` query (plus the
/// queries for each submodule) for one whose index could list a gitlink.
pub fn collect_repositories(
    exec: &dyn Exec,
    repo_roots: &[String],
    max_depth: i64,
    timeout: f64,
) -> (Vec<InventoryItem>, Vec<String>) {
    collect_repositories_with(
        exec,
        repo_roots,
        max_depth,
        timeout,
        Path::new(SYSTEM_GIT_CONFIG),
    )
}

/// [`collect_repositories`] with an injectable system configuration path.
fn collect_repositories_with(
    exec: &dyn Exec,
    repo_roots: &[String],
    max_depth: i64,
    timeout: f64,
    system_config: &Path,
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
        let repos = discover_git_repositories(repo_roots, max_depth, &mut warnings);
        (version.join().unwrap_or_default(), repos)
    });

    let system_config_may = !repos.is_empty() && config_may_define_filters(system_config);
    let next = AtomicUsize::new(0);
    let mut probes: Vec<Option<RepoProbe>> = (0..repos.len()).map(|_| None).collect();
    let results = std::sync::Mutex::new(&mut probes);
    thread::scope(|s| {
        for _ in 0..worker_count(repos.len()) {
            // A worker that cannot get its own thread drains the queue inline.
            spawn_or_run(s, || {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some((repo, bare)) = repos.get(index) else {
                        break;
                    };
                    let probe =
                        probe_repository(exec, &git, repo, *bare, system_config_may, timeout);
                    if let Ok(mut guard) = results.lock() {
                        guard[index] = Some(probe);
                    }
                }
            });
        }
    });

    let mut items = Vec::new();
    for ((repo, _), probe) in repos.iter().zip(probes) {
        let Some(probe) = probe else { continue };
        let display = paths::display(repo);
        let mut metadata = Metadata::from_pairs([
            ("executable", git.as_str()),
            ("executable_version", executable_version.as_str()),
        ]);
        if let Some(branch) = probe.branch {
            metadata.set("branch", branch);
        }
        let repo_status = match &probe.status {
            None => "bare",
            Some(status) if status.code == 0 => {
                let dirty = splitlines(&status.stdout)
                    .into_iter()
                    .filter(|line| !line.starts_with('#') && !strip(line).is_empty())
                    .count();
                metadata.set("dirty_count", dirty.to_string());
                if dirty > 0 { "dirty" } else { "clean" }
            }
            Some(status) => {
                warnings.push(format!(
                    "git status failed for {display}: {}",
                    strip(&status.stderr)
                ));
                "unknown"
            }
        };
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
        item.path = display;
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

/// The name and value text of one env-file assignment: `NAME=value`,
/// `NAME = value`, or the shell form `export NAME=value` used by `.envrc`.
fn env_assignment(line: &str) -> Option<(&str, &str)> {
    let mut trimmed = line.trim_start_matches(pyfmt::is_space);
    if let Some(rest) = trimmed
        .strip_prefix("export")
        .filter(|rest| rest.starts_with(pyfmt::is_space))
    {
        let rest = rest.trim_start_matches(pyfmt::is_space);
        if !rest.starts_with('=') {
            trimmed = rest;
        }
    }
    match trimmed.chars().next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return None,
    }
    let end = trimmed
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(trimmed.len());
    let value = trimmed[end..]
        .trim_start_matches(pyfmt::is_space)
        .strip_prefix('=')?;
    Some((&trimmed[..end], value))
}

/// Byte offset just past the quote that closes a value opened with
/// `quote`; with `escapes` (double quotes and `$'...'`), a backslash escapes
/// the next character.
fn closing_quote(text: &str, quote: char, escapes: bool) -> Option<usize> {
    let mut escaped = false;
    for (index, c) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' && escapes {
            escaped = true;
        } else if c == quote {
            return Some(index + c.len_utf8());
        }
    }
    None
}

/// Whether a line ends in an unescaped backslash, which continues it
/// (shell syntax, as used by `.envrc`).
fn continues_line(line: &str) -> bool {
    line.bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 1
}

/// Names assigned in env-file text. Lines that belong to a value (inside a
/// quoted value that spans several lines, or after a backslash line
/// continuation) are never read as names: a PEM block, for example, has
/// lines that look like `BASE64=`.
fn env_names_in(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut open_quote: Option<(char, bool)> = None;
    let mut continued = false;
    for line in splitlines(text).into_iter().take(MAX_ENV_LINES) {
        if continued {
            continued = continues_line(line);
            continue;
        }
        if let Some((quote, escapes)) = open_quote {
            if closing_quote(line, quote, escapes).is_some() {
                open_quote = None;
            }
            continue;
        }
        let Some((name, value)) = env_assignment(line) else {
            continue;
        };
        names.push(name.to_string());
        let value = value.trim_start_matches([' ', '\t']);
        let quoted = if let Some(rest) = value.strip_prefix("$'") {
            Some(('\'', true, rest))
        } else {
            match value.chars().next() {
                Some('"') => Some(('"', true, &value[1..])),
                Some('\'') => Some(('\'', false, &value[1..])),
                _ => None,
            }
        };
        match quoted {
            Some((quote, escapes, rest)) => {
                if closing_quote(rest, quote, escapes).is_none() {
                    open_quote = Some((quote, escapes));
                }
            }
            None => continued = continues_line(value),
        }
    }
    names.sort();
    names.dedup();
    names
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
    env_names_in(&pyfmt::decode_ignore(&raw[..raw.len().min(MAX_ENV_BYTES)]))
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
    fn systemctl_parsers_strip_status_markers_and_keep_first_position() {
        let output = "● bad.service loaded failed failed Bad\nshort.service\nx.service a b c\nx.service d e f\n* gone.service not-found inactive dead gone\n● short.service a b\n";
        assert_eq!(
            parse_systemctl_units(output),
            vec![
                ("bad.service".to_string(), "failed".to_string()),
                ("x.service".to_string(), "e".to_string()),
                ("gone.service".to_string(), "inactive".to_string()),
            ]
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
    fn unreadable_unit_directories_mark_the_scan_incomplete() {
        if running_as_root() {
            eprintln!("skipped: permission checks do not apply to root");
            return;
        }
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        let denied = root.join("denied");
        let readable = root.join("units");
        fs::create_dir_all(&denied).unwrap();
        fs::create_dir_all(&readable).unwrap();
        write(&denied.join("hidden.service"), b"");
        write(&readable.join("seen.timer"), b"");
        chmod(&denied, 0o000);
        let off = fake(|args| CommandResult::new(0, "", "", &args[0]));
        let dirs = [
            spelling(&denied),
            spelling(&readable),
            spelling(&root.join("missing")),
            spelling(&readable.join("seen.timer")),
        ];
        let (items, warnings) = collect_systemd(&off, "off", &dirs, true, 1.0);
        chmod(&denied, 0o755);
        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, ["seen.timer"]);
        assert_eq!(
            warnings,
            vec![format!(
                "inventory-error: {}: Permission denied",
                denied.display()
            )]
        );
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

    #[test]
    fn walks_never_enter_symlinked_directories_or_loops() {
        let parent = tmp();
        let base = fs::canonicalize(parent.path()).unwrap();
        let outside = base.join("outside");
        fs::create_dir_all(outside.join("repo/.git")).unwrap();
        write(&outside.join(".env"), b"OUTSIDE=1\n");
        let root = base.join("root");
        fs::create_dir_all(root.join("real/.git")).unwrap();
        write(&root.join("real/.env"), b"INSIDE=1\n");
        symlink(&outside, root.join("linked")).unwrap();
        symlink(&root, root.join("loop")).unwrap();
        symlink("self", root.join("self")).unwrap();
        symlink(&outside, root.join("dir.env")).unwrap();
        symlink(&outside, root.join(".git")).unwrap();

        let mut warnings = Vec::new();
        let repos = find_git_repositories(&[spelling(&root)], 5, &mut warnings);
        assert_eq!(repos, vec![root.join("real")]);
        let envs = find_env_files(&[spelling(&root)], 5, &mut warnings);
        assert_eq!(envs, vec![root.join("real/.env")]);
        assert!(warnings.is_empty(), "{warnings:?}");
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
            b"export A=1\n B = 2\n#C=3\n\xe2\x80\x83D=4\nE\x1f=5\n1F=6\n_G=7\nH=8\r\nI=9\x0bJ=10\nexport=11\nexport  K = 12\nexportL=13\n",
        );
        let mut warnings = Vec::new();
        assert_eq!(
            env_names(&path, &mut warnings),
            [
                "A", "B", "D", "E", "H", "I", "J", "K", "_G", "export", "exportL"
            ]
        );
        let many = dir.path().join("many.env");
        let content: String = (0..1100).map(|i| format!("V{i}=1\n")).collect();
        write(&many, content.as_bytes());
        assert_eq!(env_names(&many, &mut warnings).len(), MAX_ENV_LINES);
        assert!(warnings.is_empty());
    }

    #[test]
    fn multi_line_quoted_values_are_never_read_as_names() {
        let text = concat!(
            "KEY=\"-----BEGIN PRIVATE KEY-----\n",
            "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj==\n",
            "SECRETPART=abc\n",
            "-----END PRIVATE KEY-----\"\n",
            "AFTER=1\n",
            "ESC=\"a \\\" still open\nINSIDE=x\n\"\n",
            "SINGLE='one\nIN_SINGLE=y\n'\n",
            "SAME_LINE=\"closed\" ok\n",
            "EMPTY=\"\"\n",
            "LAST=2\n",
            "CONT=part one \\\n",
            "IN_CONT=x \\\n",
            "STILL_IN_CONT=y\n",
            "AFTER_CONT=3\n",
            "TWO_BACKSLASHES=a\\\\\n",
            "ANSI=$'line \\' one\n",
            "IN_ANSI=z'\n",
            "END=4\n",
        );
        assert_eq!(
            env_names_in(text),
            [
                "AFTER",
                "AFTER_CONT",
                "ANSI",
                "CONT",
                "EMPTY",
                "END",
                "ESC",
                "KEY",
                "LAST",
                "SAME_LINE",
                "SINGLE",
                "TWO_BACKSLASHES"
            ]
        );
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

    #[test]
    fn shebang_reading_is_bounded_across_blocks() {
        let dir = tmp();
        let long_line = format!("#!/bin/{}", "x".repeat(20_000));
        let mut straddle = vec![b' '; 8191];
        straddle.extend_from_slice("\u{3000}#!/bin/x\n".as_bytes());
        let cases: Vec<(&str, Vec<u8>, String)> = vec![
            (
                "long",
                format!("{long_line}\nrest\n").into_bytes(),
                long_line.clone(),
            ),
            (
                "spaces",
                [vec![b' '; 9000], b"#!/bin/sh\n".to_vec()].concat(),
                "#!/bin/sh".into(),
            ),
            ("straddle", straddle, "#!/bin/x".into()),
            ("binary", vec![b'A'; 1 << 20], String::new()),
            (
                "huge",
                [b"#!".to_vec(), vec![b'y'; 100_000]].concat(),
                format!("#!{}", "y".repeat(MAX_SHEBANG_BYTES as usize - 2)),
            ),
        ];
        for (name, content, expected) in cases {
            let path = dir.path().join(name);
            write(&path, &content);
            assert_eq!(read_shebang(&path), expected, "{name}");
        }
        // An endless file without line breaks no longer reads forever.
        assert_eq!(read_shebang(Path::new("/dev/zero")), "");
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
    fn external_commands_read_stdin_from_dev_null() {
        let result = run_command(
            &[
                "/bin/sh".into(),
                "-c".into(),
                "readlink /proc/self/fd/0".into(),
            ],
            5.0,
        );
        assert_eq!(
            (result.status.as_str(), result.stdout.trim()),
            ("ok", "/dev/null")
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
            env[SAFE_SUBPROCESS_ENV.len()..],
            [
                ("GIT_OPTIONAL_LOCKS".to_string(), "0".to_string()),
                ("GIT_NO_LAZY_FETCH".to_string(), "1".to_string()),
            ]
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
                CommandResult::new(0, "# branch.oid 0abc\n# branch.head main\n", "", &args[0])
            } else if args.iter().any(|a| a == "log") {
                CommandResult::new(0, "2026-01-01T00:00:00Z\n", "", &args[0])
            } else {
                CommandResult::new(0, "unexpected\n", "", &args[0])
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
    fn cron_standard_input_after_percent_is_redacted() {
        let crontab = concat!(
            "0 9 * * * mail -s report ops%Dear ops,%password is hunter2%\n",
            "@daily date +\\%Y-\\%m-\\%d >> /tmp/log\n",
            "@hourly printf a\\%b%second line\n",
            "@weekly %all stdin\n",
        );
        let exec = fake(move |args| CommandResult::new(0, crontab, "", &args[0]));
        let (items, warnings) = collect_cron(&exec, 1.0, true);
        assert!(warnings.is_empty());
        let previews: Vec<&str> = items
            .iter()
            .map(|item| item.metadata.get("command_preview").unwrap())
            .collect();
        assert_eq!(
            previews,
            [
                "mail -s report ops%<redacted>",
                "date +\\%Y-\\%m-\\%d >> /tmp/log",
                "printf a\\%b%<redacted>",
                "%<redacted>",
            ]
        );
        assert!(!format!("{items:?}").contains("hunter2"));
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
        // A host `/etc/gitconfig` that mentions `filter` would add a query.
        let missing = root.join("no-system-config");
        let (items, warnings) =
            collect_repositories_with(&recorder, &[spelling(&root)], 1, 1.0, &missing);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "r\u{fffd}epo");
        let calls = recorder.0.into_inner().unwrap();
        assert_eq!(calls.len(), 2);
        let expected: Vec<OsString> = ["-c", "core.fsmonitor=", "-C"]
            .map(OsString::from)
            .into_iter()
            .chain([repo.clone().into_os_string()])
            .collect();
        for call in calls {
            assert_eq!(call[1..5], expected[..]);
        }
    }

    /// Records every `git` call and answers like a repository on `main`
    /// with one modified file.
    struct GitRecorder(std::sync::Mutex<Vec<Vec<String>>>);

    impl Exec for GitRecorder {
        fn resolve(&self, _name: &str) -> Option<String> {
            Some("/usr/bin/git".into())
        }
        fn run(&self, args: &[OsString], _timeout: f64) -> CommandResult {
            let args = lossy(args);
            let stdout = match args.iter().map(String::as_str).nth(5) {
                Some("status") => "# branch.oid 0abc\n# branch.head main\n1 .M N... 1 1 1 a a f\n",
                Some("rev-parse") => "trunk\n",
                Some("log") => "2026-01-02T00:00:00+00:00\n",
                _ => "",
            };
            if let Ok(mut calls) = self.0.lock() {
                calls.push(args);
            }
            CommandResult::new(0, stdout, "", "/usr/bin/git")
        }
        fn probe(&self, _name: &str, _path: &str, _timeout: f64) -> String {
            "git version 2".into()
        }
    }

    #[test]
    fn repositories_use_two_git_queries_and_bare_ones_skip_status() {
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("work/.git")).unwrap();
        let bare = root.join("mirror.git");
        fs::create_dir_all(bare.join("objects")).unwrap();
        fs::create_dir_all(bare.join("refs")).unwrap();
        write(&bare.join("HEAD"), b"ref: refs/heads/trunk\n");
        let recorder = GitRecorder(std::sync::Mutex::new(Vec::new()));
        let missing = root.join("no-system-config");
        let (items, warnings) =
            collect_repositories_with(&recorder, &[spelling(&root)], 1, 1.0, &missing);
        assert!(warnings.is_empty(), "{warnings:?}");
        let summary: Vec<(&str, &str, Option<&str>, Option<&str>)> = items
            .iter()
            .map(|item| {
                (
                    item.name.as_str(),
                    item.status.as_str(),
                    item.metadata.get("branch"),
                    item.metadata.get("dirty_count"),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("mirror.git", "bare", Some("trunk"), None),
                ("work", "dirty", Some("main"), Some("1")),
            ]
        );
        let mut commands: Vec<String> = recorder
            .0
            .into_inner()
            .unwrap()
            .into_iter()
            .map(|call| {
                let repo = Path::new(&call[4])
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                format!("{repo}: {}", call[5..].join(" "))
            })
            .collect();
        commands.sort();
        assert_eq!(
            commands,
            [
                "mirror.git: log -1 --no-show-signature --format=%cI",
                "mirror.git: rev-parse --abbrev-ref HEAD",
                "work: log -1 --no-show-signature --format=%cI",
                "work: status --porcelain=v2 --branch",
            ]
        );
    }

    #[test]
    fn porcelain_v2_branch_matches_rev_parse_spelling() {
        let branch = |text: &str| porcelain_v2_branch(text);
        assert_eq!(
            branch("# branch.oid 1a\n# branch.head main\n"),
            Some("main".into())
        );
        assert_eq!(
            branch("# branch.oid 1a\n# branch.head (detached)\n"),
            Some("HEAD".into())
        );
        assert_eq!(branch("# branch.oid (initial)\n# branch.head main\n"), None);
        assert_eq!(
            branch(
                "# branch.oid 1a\n# branch.head feat/x\n# branch.upstream origin/feat/x\n# branch.ab +1 -0\n"
            ),
            Some("feat/x".into())
        );
        assert_eq!(branch(""), None);
    }

    fn real_git() -> Option<String> {
        resolve_approved_executable("git")
    }

    fn git_in(git: &str, repo: &Path, args: &[&str]) {
        let status = Command::new(git)
            .arg("-C")
            .arg(repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", repo)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn real_git_branch_and_dirty_count_match_the_reference_queries() {
        let Some(git) = real_git() else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        let make = |name: &str| {
            let repo = root.join(name);
            fs::create_dir(&repo).unwrap();
            git_in(&git, &repo, &["init", "-q", "-b", "main"]);
            repo
        };
        let unborn = make("unborn");
        write(&unborn.join("new"), b"x");
        let clean = make("clean");
        git_in(
            &git,
            &clean,
            &["commit", "-q", "--allow-empty", "-m", "one"],
        );
        let dirty = make("dirty");
        write(&dirty.join("a"), b"1");
        write(&dirty.join("b"), b"1");
        git_in(&git, &dirty, &["add", "a", "b"]);
        git_in(&git, &dirty, &["commit", "-q", "-m", "one"]);
        write(&dirty.join("a"), b"2");
        git_in(&git, &dirty, &["mv", "b", "c"]);
        write(&dirty.join("untracked"), b"u");
        let detached = make("detached");
        git_in(
            &git,
            &detached,
            &["commit", "-q", "--allow-empty", "-m", "one"],
        );
        git_in(&git, &detached, &["checkout", "-q", "--detach"]);

        let (items, warnings) = collect_repositories(&System, &[spelling(&root)], 1, 30.0);
        assert!(warnings.is_empty(), "{warnings:?}");
        let summary: Vec<(&str, &str, Option<&str>, Option<&str>)> = items
            .iter()
            .map(|item| {
                (
                    item.name.as_str(),
                    item.status.as_str(),
                    item.metadata.get("branch"),
                    item.metadata.get("dirty_count"),
                )
            })
            .collect();
        // `rev-parse --abbrev-ref HEAD` fails on an unborn branch and
        // prints `HEAD` when detached; `status --porcelain` printed one line
        // per change (modified, renamed, untracked).
        assert_eq!(
            summary,
            [
                ("clean", "clean", Some("main"), Some("0")),
                ("detached", "clean", Some("HEAD"), Some("0")),
                ("dirty", "dirty", Some("main"), Some("3")),
                ("unborn", "dirty", None, Some("1")),
            ]
        );
    }

    #[test]
    fn scanning_a_repository_never_starts_the_fsmonitor_daemon() {
        let Some(git) = real_git() else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let dir = tmp();
        let repo = fs::canonicalize(dir.path()).unwrap().join("repo");
        fs::create_dir(&repo).unwrap();
        git_in(&git, &repo, &["init", "-q", "-b", "main"]);
        git_in(&git, &repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git_in(&git, &repo, &["config", "core.fsmonitor", "true"]);
        let (items, warnings) = collect_repositories(&System, &[spelling(&repo)], 0, 30.0);
        let daemon_state = repo.join(".git/fsmonitor--daemon");
        let started = daemon_state.exists();
        if started {
            // Never leave a daemon behind, even when the assertion fails.
            git_in(&git, &repo, &["fsmonitor--daemon", "stop"]);
        }
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(items[0].status, "clean");
        assert!(!started, "git status started fsmonitor--daemon");
    }

    #[test]
    fn scanning_a_repository_never_runs_its_filter_drivers() {
        let Some(git) = real_git() else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        git_in(&git, &repo, &["init", "-q", "-b", "main"]);
        write(&repo.join("clean-me"), b"one\n");
        write(&repo.join("process-me"), b"two\n");
        git_in(&git, &repo, &["add", "clean-me", "process-me"]);
        git_in(&git, &repo, &["commit", "-q", "-m", "one"]);
        // A linked worktree reads the same repository configuration.
        let linked = root.join("linked");
        git_in(
            &git,
            &repo,
            &["worktree", "add", "-q", linked.to_str().unwrap()],
        );
        // One driver comes from an included file, so the cheap check must
        // fall through to the `git config` query; the other is direct and
        // required, so a failing driver would also fail `git status`.
        let ran_clean = root.join("clean-ran");
        let ran_process = root.join("process-ran");
        let included = root.join("drivers.cfg");
        write(
            &included,
            format!(
                "[filter \"Evil.Clean\"]\n\tclean = sh -c 'touch {}; cat'\n",
                ran_clean.display()
            )
            .as_bytes(),
        );
        git_in(
            &git,
            &repo,
            &["config", "include.path", included.to_str().unwrap()],
        );
        git_in(
            &git,
            &repo,
            &[
                "config",
                "filter.evil-process.process",
                &format!("sh -c 'touch {}; exit 1'", ran_process.display()),
            ],
        );
        git_in(
            &git,
            &repo,
            &["config", "filter.evil-process.required", "true"],
        );
        write(
            &repo.join(".git/info/attributes"),
            b"clean-me filter=Evil.Clean\nprocess-me filter=evil-process\n",
        );
        // Stale stat data makes `git status` re-read both files.
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        for name in ["clean-me", "process-me"] {
            File::options()
                .write(true)
                .open(repo.join(name))
                .and_then(|file| file.set_modified(old))
                .unwrap();
        }
        for name in ["clean-me", "process-me"] {
            File::options()
                .write(true)
                .open(linked.join(name))
                .and_then(|file| file.set_modified(old))
                .unwrap();
        }
        let roots = [spelling(&repo), spelling(&linked)];
        let (items, warnings) = collect_repositories(&System, &roots, 0, 30.0);
        assert!(!ran_clean.exists(), "git status ran the clean filter");
        assert!(!ran_process.exists(), "git status ran the process filter");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(items.len(), 2);
        for item in &items {
            assert_eq!(item.status, "clean", "{item:?}");
            assert_eq!(item.metadata.get("dirty_count"), Some("0"));
        }
    }

    /// Records every command and runs it on the host, like [`System`].
    struct RecordingSystem(std::sync::Mutex<Vec<Vec<String>>>);

    impl Exec for RecordingSystem {
        fn resolve(&self, name: &str) -> Option<String> {
            System.resolve(name)
        }
        fn run(&self, args: &[OsString], timeout: f64) -> CommandResult {
            if let Ok(mut calls) = self.0.lock() {
                calls.push(lossy(args));
            }
            System.run(args, timeout)
        }
    }

    #[test]
    fn scanning_never_runs_filter_drivers_configured_in_submodules() {
        let Some(git) = real_git() else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        // Defines `filter.x.clean` in a submodule's own configuration and
        // assigns it to `file`, whose stale stat data makes `git status`
        // re-read it through the filter. The driver leaves `marker` and is
        // named after it, so disabling one driver never covers another. A
        // conditional driver comes from a file included only for that
        // Git directory (`includeIf "gitdir:..."`), which a query run
        // anywhere else would not see.
        let arm_with = |git_dir: &Path, file: &Path, marker: &Path, conditional: bool| {
            let name = marker.file_name().unwrap().to_str().unwrap();
            let section = format!(
                "[filter \"{name}\"]\n\tclean = \"sh -c 'touch {}; cat'\"\n",
                marker.display()
            );
            let config = git_dir.join("config");
            let mut text = fs::read(&config).unwrap();
            if conditional {
                let included = marker.with_extension("cfg");
                write(&included, section.as_bytes());
                text.extend_from_slice(
                    format!(
                        "[includeIf \"gitdir:{}\"]\n\tpath = {}\n",
                        git_dir.display(),
                        included.display()
                    )
                    .as_bytes(),
                );
            } else {
                text.extend_from_slice(section.as_bytes());
            }
            write(&config, &text);
            fs::create_dir_all(git_dir.join("info")).unwrap();
            write(
                &git_dir.join("info/attributes"),
                format!("* filter={name}\n").as_bytes(),
            );
            File::options()
                .write(true)
                .open(file)
                .and_then(|file| file.set_modified(old))
                .unwrap();
        };
        let arm =
            |git_dir: &Path, file: &Path, marker: &Path| arm_with(git_dir, file, marker, false);
        let make = |path: &Path| {
            fs::create_dir_all(path).unwrap();
            git_in(&git, path, &["init", "-q", "-b", "main"]);
            write(&path.join("f"), b"content\n");
            git_in(&git, path, &["add", "f"]);
            git_in(&git, path, &["commit", "-q", "-m", "one"]);
        };
        let allow = ["-c", "protocol.file.allow=always"];
        let submodule = |repo: &Path, args: &[&str]| {
            let all: Vec<&str> = allow.iter().chain(args).copied().collect();
            git_in(&git, repo, &all);
        };

        // Sources: `outer` has a nested submodule `inner`.
        let sources = root.join("sources");
        make(&sources.join("inner"));
        make(&sources.join("outer"));
        let inner_url = spelling(&sources.join("inner"));
        let outer_url = spelling(&sources.join("outer"));
        submodule(
            &sources.join("outer"),
            &["submodule", "add", "-q", &inner_url, "inner"],
        );
        git_in(
            &git,
            &sources.join("outer"),
            &["commit", "-q", "-m", "inner"],
        );

        let scanned = root.join("scanned");
        // 1. A registered submodule with a nested one, both armed.
        let registered = scanned.join("registered");
        make(&registered);
        submodule(
            &registered,
            &["submodule", "add", "-q", &outer_url, "outer"],
        );
        submodule(
            &registered,
            &["submodule", "update", "-q", "--init", "--recursive"],
        );
        git_in(&git, &registered, &["commit", "-q", "-m", "outer"]);
        let modules = registered.join(".git/modules/outer");
        let ran_outer = root.join("ran-outer");
        let ran_inner = root.join("ran-inner");
        arm(&modules, &registered.join("outer/f"), &ran_outer);
        arm_with(
            &modules.join("modules/inner"),
            &registered.join("outer/inner/f"),
            &ran_inner,
            true,
        );

        // 2. A bare gitlink: an embedded repository added without any
        //    `.gitmodules` entry.
        let unregistered = scanned.join("unregistered");
        make(&unregistered);
        make(&unregistered.join("embedded"));
        git_in(&git, &unregistered, &["add", "embedded"]);
        git_in(&git, &unregistered, &["commit", "-q", "-m", "gitlink"]);
        assert!(!unregistered.join(".gitmodules").exists());
        let ran_embedded = root.join("ran-embedded");
        arm(
            &unregistered.join("embedded/.git"),
            &unregistered.join("embedded/f"),
            &ran_embedded,
        );

        // 3. A submodule whose `.git` file points outside the repository.
        let escaping = scanned.join("escaping");
        make(&escaping);
        make(&escaping.join("esc"));
        git_in(&git, &escaping, &["add", "esc"]);
        git_in(&git, &escaping, &["commit", "-q", "-m", "gitlink"]);
        let elsewhere = root.join("elsewhere.git");
        fs::rename(escaping.join("esc/.git"), &elsewhere).unwrap();
        write(
            &escaping.join("esc/.git"),
            format!("gitdir: {}\n", elsewhere.display()).as_bytes(),
        );
        let ran_escaping = root.join("ran-escaping");
        arm(&elsewhere, &escaping.join("esc/f"), &ran_escaping);

        // 4. A gitlink kept in the shared file of a split index.
        let split = scanned.join("split");
        make(&split);
        make(&split.join("embedded"));
        git_in(&git, &split, &["add", "embedded"]);
        git_in(&git, &split, &["commit", "-q", "-m", "gitlink"]);
        git_in(&git, &split, &["update-index", "--split-index"]);
        let ran_split = root.join("ran-split");
        arm(
            &split.join("embedded/.git"),
            &split.join("embedded/f"),
            &ran_split,
        );

        // 5. A gitlink whose mode has permission bits, which Git still
        //    treats as a gitlink. The index trailer is zeroed, as
        //    `index.skipHash` writes it, so Git accepts the edit.
        let odd = scanned.join("odd-mode");
        fs::create_dir_all(&odd).unwrap();
        git_in(&git, &odd, &["init", "-q", "-b", "main"]);
        make(&odd.join("embedded"));
        git_in(&git, &odd, &["add", "embedded"]);
        git_in(&git, &odd, &["commit", "-q", "-m", "gitlink"]);
        git_in(&git, &odd, &["config", "index.skipHash", "true"]);
        let index_path = odd.join(".git/index");
        let mut index = fs::read(&index_path).unwrap();
        // The only entry starts after the 12-byte header; its mode is at 24.
        assert_eq!(index[36..40], 0o160000u32.to_be_bytes());
        index[36..40].copy_from_slice(&0o160755u32.to_be_bytes());
        let trailer = index.len() - 20;
        index[trailer..].fill(0);
        write(&index_path, &index);
        let ran_odd = root.join("ran-odd");
        arm(
            &odd.join("embedded/.git"),
            &odd.join("embedded/f"),
            &ran_odd,
        );

        // 6. A repository without submodules.
        let plain = scanned.join("plain");
        make(&plain);

        // 7. A repository whose `core.worktree` puts its work tree, and
        //    so its submodule checkouts, outside the scanned directory.
        let tree = root.join("relocated-tree");
        make(&tree);
        submodule(&tree, &["submodule", "add", "-q", &outer_url, "sub"]);
        git_in(&git, &tree, &["commit", "-q", "-m", "sub"]);
        let relocated = scanned.join("relocated");
        fs::create_dir(&relocated).unwrap();
        fs::rename(tree.join(".git"), relocated.join(".git")).unwrap();
        let sub_git_dir = relocated.join(".git/modules/sub");
        write(
            &tree.join("sub/.git"),
            format!("gitdir: {}\n", sub_git_dir.display()).as_bytes(),
        );
        for (config, work_tree) in [
            (relocated.join(".git/config"), tree.clone()),
            (sub_git_dir.join("config"), tree.join("sub")),
        ] {
            git_in(
                &git,
                &root,
                &[
                    "config",
                    "--file",
                    config.to_str().unwrap(),
                    "core.worktree",
                    work_tree.to_str().unwrap(),
                ],
            );
        }
        let ran_relocated = root.join("ran-relocated");
        arm(&sub_git_dir, &tree.join("sub/f"), &ran_relocated);

        let exec = RecordingSystem(std::sync::Mutex::new(Vec::new()));
        let missing = root.join("no-system-config");
        let (items, warnings) =
            collect_repositories_with(&exec, &[spelling(&scanned)], 1, 30.0, &missing);
        let ran: Vec<&str> = [
            (&ran_outer, "a registered submodule"),
            (&ran_inner, "a nested submodule"),
            (&ran_embedded, "an unregistered gitlink"),
            (&ran_escaping, "a submodule with an outside git directory"),
            (&ran_relocated, "a submodule of a relocated work tree"),
            (&ran_split, "a gitlink in a shared index"),
            (&ran_odd, "a gitlink with permission bits in its mode"),
        ]
        .into_iter()
        .filter(|(marker, _)| marker.exists())
        .map(|(_, what)| what)
        .collect();
        assert!(ran.is_empty(), "git status ran the filters of {ran:?}");
        let summary: Vec<(&str, &str, Option<&str>)> = items
            .iter()
            .map(|item| {
                (
                    item.name.as_str(),
                    item.status.as_str(),
                    item.metadata.get("dirty_count"),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("escaping", "unknown", None),
                ("odd-mode", "clean", Some("0")),
                ("plain", "clean", Some("0")),
                ("registered", "clean", Some("0")),
                ("relocated", "clean", Some("0")),
                ("split", "clean", Some("0")),
                ("unregistered", "clean", Some("0")),
            ]
        );
        assert_eq!(
            warnings,
            [format!(
                "git status failed for {}: not run: submodule 'esc': git directory is \
                 outside the repository",
                escaping.display()
            )]
        );
        // A repository whose index lists no gitlink costs no extra query.
        let calls = exec.0.into_inner().unwrap();
        let plain_calls: Vec<&Vec<String>> = calls
            .iter()
            .filter(|call| call.iter().any(|arg| Path::new(arg) == plain))
            .collect();
        assert_eq!(plain_calls.len(), 2, "{plain_calls:?}");
    }

    #[test]
    fn scanning_never_verifies_signatures_or_fetches_missing_objects() {
        use std::io::Write;
        let Some(git) = real_git() else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        let git_out = |repo: &Path, args: &[&str], input: &[u8]| -> String {
            let mut child = Command::new(&git)
                .arg("-C")
                .arg(repo)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", repo)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("git runs");
            child.stdin.take().unwrap().write_all(input).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "git {args:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        let make = |name: &str| {
            let repo = root.join(name);
            fs::create_dir(&repo).unwrap();
            git_in(&git, &repo, &["init", "-q", "-b", "main"]);
            write(&repo.join("f"), b"one\n");
            git_in(&git, &repo, &["add", "f"]);
            git_in(&git, &repo, &["commit", "-q", "-m", "one"]);
            repo
        };
        let marker_script = |name: &str| {
            let marker = root.join(format!("{name}-ran"));
            let script = root.join(format!("{name}.sh"));
            write(
                &script,
                format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()).as_bytes(),
            );
            chmod(&script, 0o755);
            (script, marker)
        };

        // `log.showSignature` would make `git log` run `gpg.program` on a
        // signed HEAD commit.
        let signed = make("signed");
        let tree = git_out(&signed, &["rev-parse", "HEAD^{tree}"], b"");
        let commit = format!(
            "tree {tree}\nauthor t <t@t> 1 +0000\ncommitter t <t@t> 1 +0000\n\
             gpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\
             \nsigned\n"
        );
        let oid = git_out(
            &signed,
            &["hash-object", "-t", "commit", "-w", "--stdin"],
            commit.as_bytes(),
        );
        git_in(&git, &signed, &["update-ref", "refs/heads/main", &oid]);
        let (gpg, gpg_ran) = marker_script("gpg");
        git_in(&git, &signed, &["config", "log.showSignature", "true"]);
        git_in(
            &git,
            &signed,
            &["config", "gpg.program", gpg.to_str().unwrap()],
        );

        // A partial clone missing HEAD's tree would fetch it through the
        // promisor remote, running the configured `core.sshCommand`.
        let partial = make("partial");
        let tree = git_out(&partial, &["rev-parse", "HEAD^{tree}"], b"");
        fs::remove_file(
            partial
                .join(".git/objects")
                .join(&tree[..2])
                .join(&tree[2..]),
        )
        .unwrap();
        let (ssh, ssh_ran) = marker_script("ssh");
        for (key, value) in [
            ("core.repositoryformatversion", "1"),
            ("extensions.partialClone", "origin"),
            ("remote.origin.url", "ssh://example.invalid/repo"),
            ("remote.origin.promisor", "true"),
            ("core.sshCommand", ssh.to_str().unwrap()),
        ] {
            git_in(&git, &partial, &["config", key, value]);
        }

        let roots = [spelling(&signed), spelling(&partial)];
        let (items, warnings) = collect_repositories(&System, &roots, 0, 30.0);
        assert!(!gpg_ran.exists(), "git log ran gpg.program");
        assert!(!ssh_ran.exists(), "git fetched a missing object");
        let signed_item = items.iter().find(|item| item.name == "signed").unwrap();
        assert_eq!(signed_item.status, "clean", "{signed_item:?}");
        assert_eq!(signed_item.last_activity, "1970-01-01T00:00:01Z");
        // The missing tree is reported instead of fetched.
        let partial_item = items.iter().find(|item| item.name == "partial").unwrap();
        assert_eq!(partial_item.status, "unknown", "{partial_item:?}");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with(&format!("git status failed for {}", partial.display())),
            "{warnings:?}"
        );
    }

    #[test]
    fn filter_drivers_are_disabled_only_when_configuration_could_define_them() {
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        for name in ["plain", "lfs", "worktree"] {
            fs::create_dir_all(root.join(name).join(".git")).unwrap();
        }
        write(&root.join("plain/.git/config"), b"[core]\n\tbare = false\n");
        write(
            &root.join("lfs/.git/config"),
            b"[Filter \"lfs\"]\n\tclean = git-lfs clean -- %f\n",
        );
        write(&root.join("worktree/.git/config"), b"[core]\n");
        write(
            &root.join("worktree/.git/config.worktree"),
            b"[includeIf \"gitdir:/x/\"]\n\tpath = more\n",
        );
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let seen = std::sync::Arc::clone(&log);
        let exec = fake(move |args| {
            seen.lock().unwrap().push(args.to_vec());
            let repo = args
                .iter()
                .position(|a| a == "-C")
                .map(|i| args[i + 1].clone());
            let stdout = if args.iter().any(|a| a == "config") {
                if repo.as_deref().is_some_and(|r| r.ends_with("/lfs")) {
                    "filter.lfs.clean\0filter.lfs.process\0"
                } else {
                    return CommandResult::new(1, "", "", &args[0]);
                }
            } else if args.iter().any(|a| a == "status") {
                "# branch.oid 0abc\n# branch.head main\n"
            } else {
                ""
            };
            CommandResult::new(0, stdout, "", &args[0])
        });
        let missing = root.join("no-system-config");
        let (items, warnings) =
            collect_repositories_with(&exec, &[spelling(&root)], 1, 1.0, &missing);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(items.iter().all(|item| item.status == "clean"), "{items:?}");
        let of = |repo: &str, sub: &str| -> Vec<Vec<String>> {
            log.lock()
                .unwrap()
                .iter()
                .filter(|call| call.iter().any(|a| a.ends_with(&format!("/{repo}"))))
                .filter(|call| call.iter().any(|a| a == sub))
                .cloned()
                .collect()
        };
        // A configuration that cannot define a driver costs no query.
        assert!(of("plain", "config").is_empty());
        assert_eq!(of("worktree", "config").len(), 1);
        assert_eq!(of("lfs", "config").len(), 1);
        let status = |repo: &str| of(repo, "status").remove(0);
        assert_eq!(
            status("lfs")[..10],
            [
                "/usr/bin/tool",
                "-c",
                "core.fsmonitor=",
                "-c",
                "filter.lfs.clean=",
                "-c",
                "filter.lfs.process=",
                "-c",
                "filter.lfs.required=false",
                "-C",
            ]
        );
        assert_eq!(status("plain")[3], "-C");
        assert_eq!(status("worktree")[3], "-C");

        // A system configuration that could define drivers sends every
        // work tree through the query.
        write(&missing, b"[filter \"lfs\"]\n");
        log.lock().unwrap().clear();
        collect_repositories_with(&exec, &[spelling(&root)], 1, 1.0, &missing);
        for repo in ["plain", "lfs", "worktree"] {
            assert_eq!(of(repo, "config").len(), 1, "{repo}");
        }
    }

    #[test]
    fn filter_driver_names_refuse_names_that_cannot_be_disabled() {
        assert_eq!(
            filter_driver_names("filter.lfs.clean\0filter.lfs.process\0filter.a.b.clean\0"),
            Ok(vec!["lfs".to_string(), "a.b".to_string()])
        );
        assert_eq!(filter_driver_names(""), Ok(Vec::new()));
        assert_eq!(
            filter_driver_names("filter.x=y.clean\0"),
            Err("filter driver 'x=y' cannot be disabled".to_string())
        );
        assert!(filter_driver_names("filter.\u{fffd}.process\0").is_err());

        // A name that cannot be disabled, or a failed query, means
        // `git status` is not run and the repository is reported.
        let dir = tmp();
        let repo = fs::canonicalize(dir.path()).unwrap().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        write(&repo.join(".git/config"), b"[include]\n\tpath = x\n");
        for (code, stdout, stderr, reason) in [
            (
                0,
                "filter.x=y.clean\0",
                "",
                "not run: filter driver 'x=y' cannot be disabled",
            ),
            (
                3,
                "",
                "fatal: bad config line 1\n",
                "not run: filter driver check failed: fatal: bad config line 1",
            ),
        ] {
            let exec = fake(move |args| {
                assert!(!args.iter().any(|a| a == "status"), "status ran: {args:?}");
                if args.iter().any(|a| a == "config") {
                    CommandResult::new(code, stdout, stderr, &args[0])
                } else {
                    CommandResult::new(0, "main\n", "", &args[0])
                }
            });
            let missing = repo.join("no-system-config");
            let (items, warnings) =
                collect_repositories_with(&exec, &[spelling(&repo)], 0, 1.0, &missing);
            assert_eq!(items[0].status, "unknown");
            assert_eq!(
                warnings,
                vec![format!(
                    "git status failed for {}: {reason}",
                    repo.display()
                )]
            );
        }
    }

    #[test]
    fn configuration_files_that_could_define_filter_drivers() {
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        let file = |name: &str, content: &[u8]| {
            let path = root.join(name);
            write(&path, content);
            path
        };
        assert!(!config_may_define_filters(&root.join("missing")));
        assert!(!config_may_define_filters(&file(
            "plain",
            b"[core]\n\trepositoryformatversion = 0\n"
        )));
        assert!(config_may_define_filters(&file(
            "upper",
            b"[FILTER \"x\"]\n"
        )));
        assert!(config_may_define_filters(&file("old", b"[filter.x]\n")));
        assert!(config_may_define_filters(&file(
            "include",
            b"[IncludeIf \"onbranch:main\"]\n"
        )));
        let large = vec![b'#'; MAX_CONFIG_SCAN_BYTES + 1];
        assert!(config_may_define_filters(&file("large", &large)));
        let link = root.join("link");
        symlink(root.join("plain"), &link).unwrap();
        assert!(config_may_define_filters(&link));

        let may = |repo: &Path, system_config_may: bool| {
            repository_may_define_filters(resolve_git_dirs(repo).as_ref(), system_config_may)
        };
        let plain = root.join("plain-repo");
        fs::create_dir_all(plain.join(".git")).unwrap();
        assert!(!may(&plain, false));
        assert!(may(&plain, true));

        // A linked worktree's `.git` file is followed to the common
        // `config` and its own `config.worktree`, as Git reads them.
        let main = root.join("main");
        let admin = main.join(".git/worktrees/linked");
        fs::create_dir_all(&admin).unwrap();
        write(&admin.join("commondir"), b"../..\n");
        let linked = root.join("linked");
        fs::create_dir(&linked).unwrap();
        write(
            &linked.join(".git"),
            b"gitdir: ../main/.git/worktrees/linked\n",
        );
        assert_eq!(
            resolve_git_dirs(&linked).map(|dirs| dirs.config_files()),
            Some([
                linked.join("../main/.git/worktrees/linked/../../config"),
                linked.join("../main/.git/worktrees/linked/config.worktree"),
            ])
        );
        assert!(!may(&linked, false));
        write(&main.join(".git/config"), b"[filter \"lfs\"]\n");
        assert!(may(&linked, false));
        fs::remove_file(main.join(".git/config")).unwrap();
        write(&admin.join("config.worktree"), b"[include]\n");
        assert!(may(&linked, false));
        // A `.git` file Git itself would reject gets the full query.
        write(
            &linked.join(".git"),
            b"gitdir:../main/.git/worktrees/linked\n",
        );
        assert_eq!(
            resolve_git_dirs(&linked).map(|dirs| dirs.config_files()),
            None
        );
        assert!(may(&linked, false));
    }

    /// An index file in format `version` for `hash_len`-byte object names
    /// with `entries` (mode, name), empty `extensions`, and a zero trailer
    /// (`index.skipHash`). Version 3 entries carry extended flags.
    fn index_bytes(
        version: u32,
        hash_len: usize,
        entries: &[(u32, &str)],
        extensions: &[&[u8; 4]],
    ) -> Vec<u8> {
        let mut out = b"DIRC".to_vec();
        out.extend(version.to_be_bytes());
        out.extend(u32::try_from(entries.len()).unwrap().to_be_bytes());
        for (mode, name) in entries {
            let start = out.len();
            out.extend([0; 24]);
            out.extend(mode.to_be_bytes());
            out.extend(vec![0; 12 + hash_len]);
            let extended = if version == 3 { 0x4000 } else { 0 };
            out.extend((name.len().min(0x0fff) as u16 | extended).to_be_bytes());
            if version == 3 {
                out.extend([0, 0]);
            }
            if version == 4 {
                out.push(0);
            }
            out.extend(name.as_bytes());
            if version == 4 {
                out.push(0);
            } else {
                let length = out.len() - start;
                out.extend(vec![0; 8 - length % 8]);
            }
        }
        for signature in extensions {
            out.extend(*signature);
            out.extend(0u32.to_be_bytes());
        }
        out.extend(vec![0; hash_len]);
        out
    }

    /// A truncated, flipped, or random index never panics or hangs the
    /// reader, and a well-formed index that holds a gitlink is never read
    /// as holding none.
    #[test]
    fn index_reader_survives_malformed_input_and_never_hides_a_gitlink() {
        // A deterministic SplitMix64, so a failure reproduces.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };

        // Real index layouts, gitlink and plain, across versions and hash
        // lengths, with varied names (including a long one).
        let long = "d/".repeat(600) + "f";
        let mut bases: Vec<Vec<u8>> = Vec::new();
        for (version, hash_len) in [(2, 20), (2, 32), (3, 20), (3, 32), (4, 20), (4, 32)] {
            for entries in [
                vec![(0o100644u32, "f"), (0o160000, "sub")],
                vec![(0o100644, "a"), (0o040000, "d"), (0o160755, long.as_str())],
                vec![(0o100644, "only")],
            ] {
                bases.push(index_bytes(version, hash_len, &entries, &[b"TREE"]));
            }
        }

        // No mutation makes the reader panic, over-read, or hang. A result
        // of `None` means "assume gitlinks", which is always safe.
        for _ in 0..6000 {
            let mut bytes = bases[(next() as usize) % bases.len()].clone();
            match next() % 4 {
                // Truncate anywhere, including to nothing.
                0 => bytes.truncate((next() as usize) % (bytes.len() + 1)),
                // Flip one bit.
                1 if !bytes.is_empty() => {
                    let at = (next() as usize) % bytes.len();
                    bytes[at] ^= 1 << (next() % 8);
                }
                // Overwrite a short run with random bytes.
                2 if !bytes.is_empty() => {
                    let at = (next() as usize) % bytes.len();
                    for byte in bytes[at..].iter_mut().take(1 + (next() as usize) % 16) {
                        *byte = next() as u8;
                    }
                }
                // A buffer of random length and random bytes.
                _ => {
                    bytes = (0..(next() as usize) % 256).map(|_| next() as u8).collect();
                }
            }
            // Must simply return for either hash length.
            let _ = walk_index(&bytes, 20);
            let _ = walk_index(&bytes, 32);
        }

        // The fast path never clears a well-formed index that lists a
        // gitlink, whatever else it holds and however many entries precede
        // it, across versions, hash lengths, and extensions.
        let names = ["a", "b", "zz", long.as_str(), "x/y", "a-really/long/one"];
        let extensions: [&[u8; 4]; 4] = [b"TREE", b"REUC", b"UNTR", b"sdir"];
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        for case in 0..2000u64 {
            let (version, hash_len) =
                [(2, 20), (2, 32), (3, 20), (3, 32), (4, 20), (4, 32)][(next() as usize) % 6];
            let count = (next() as usize) % 6;
            let mut entries: Vec<(u32, &str)> = (0..count)
                .map(|_| {
                    let mode = [0o100644u32, 0o100755, 0o120000, 0o040000][(next() as usize) % 4];
                    (mode, names[(next() as usize) % names.len()])
                })
                .collect();
            // Insert at least one gitlink at a random position; any mode
            // whose type bits are a gitlink, as Git tests it.
            let gitlink_mode = 0o160000u32 | (next() as u32 & 0o777);
            let at = (next() as usize) % (entries.len() + 1);
            entries.insert(at, (gitlink_mode, names[(next() as usize) % names.len()]));
            // A sparse index needs no tree entries; keep names unique enough
            // that Git's own writer would accept the order is not required
            // here, since only the fast path reads these bytes.
            let exts: Vec<&[u8; 4]> = if next() % 2 == 0 {
                vec![extensions[(next() as usize) % extensions.len()]]
            } else {
                Vec::new()
            };
            let bytes = index_bytes(version, hash_len, &entries, &exts);
            // The reader for the matching hash length must see the gitlink.
            let seen = walk_index(&bytes, hash_len);
            assert!(
                matches!(seen, Some((true, _)) | None),
                "case {case}: v{version} {hash_len}B {entries:?} read as {seen:?}"
            );
            // And the on-disk path (OR over both hash lengths) must too.
            let git_dir = root.join(format!("case-{case}"));
            fs::create_dir(&git_dir).unwrap();
            write(&git_dir.join("index"), &bytes);
            assert!(
                index_may_list_gitlinks(&git_dir),
                "case {case}: v{version} {hash_len}B {entries:?} cleared on disk"
            );
        }
    }

    #[test]
    fn indexes_that_could_list_gitlinks() {
        let plain = index_bytes(2, 20, &[(0o100644, "f"), (0o120000, "link")], &[b"TREE"]);
        let gitlink = index_bytes(2, 20, &[(0o100644, "f"), (0o160000, "sub")], &[]);
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        // A Git directory holding `files`.
        let git_dir = |name: &str, files: &[(&str, &[u8])]| {
            let path = root.join(name);
            fs::create_dir(&path).unwrap();
            for (file, content) in files {
                write(&path.join(file), content);
            }
            path
        };
        let lists =
            |name: &str, index: &[u8]| index_may_list_gitlinks(&git_dir(name, &[("index", index)]));
        assert!(!index_may_list_gitlinks(&git_dir("missing", &[])));
        assert!(!lists("plain", &plain));
        assert!(lists("gitlink", &gitlink));
        // Git tests only the file-type bits of a mode.
        assert!(lists(
            "odd-mode",
            &index_bytes(2, 20, &[(0o160755, "sub")], &[])
        ));
        let long = "d/".repeat(3000) + "f";
        for (version, hash_len) in [(2, 32), (3, 20), (3, 32), (4, 20), (4, 32)] {
            let what = format!("version {version}, {hash_len}-byte names");
            let entries = [(0o100644, "f"), (0o100755, long.as_str())];
            let index = index_bytes(version, hash_len, &entries, &[b"TREE"]);
            assert!(
                !lists(&format!("plain-{version}-{hash_len}"), &index),
                "{what}"
            );
            let entries = [(0o100644, long.as_str()), (0o160000, "sub")];
            let index = index_bytes(version, hash_len, &entries, &[]);
            assert!(
                lists(&format!("gitlink-{version}-{hash_len}"), &index),
                "{what}"
            );
        }
        // A sparse index folds directories into tree entries.
        assert!(lists(
            "sparse",
            &index_bytes(2, 20, &[(0o100644, "f")], &[b"sdir"])
        ));
        // A split index is judged by its shared index files.
        let split = index_bytes(2, 20, &[(0o100644, "f")], &[b"link"]);
        assert!(!lists("split-alone", &split));
        assert!(!index_may_list_gitlinks(&git_dir(
            "split-plain",
            &[("index", &split), ("sharedindex.0abc", &plain)]
        )));
        assert!(index_may_list_gitlinks(&git_dir(
            "split-gitlink",
            &[
                ("index", &split),
                ("sharedindex.0abc", &plain),
                ("sharedindex.0def", &gitlink)
            ]
        )));
        // An index that does not fit the format is assumed to list one.
        assert!(lists("truncated", &plain[..plain.len() - 30]));
        assert!(lists("not-an-index", b"not an index at all"));
        let large = vec![b'#'; MAX_INDEX_SCAN_BYTES + 1];
        assert!(lists("large", &large));
        let linked = git_dir("symlink", &[]);
        symlink(root.join("plain/index"), linked.join("index")).unwrap();
        assert!(index_may_list_gitlinks(&linked));
    }

    #[test]
    fn submodules_are_followed_or_the_repository_is_reported() {
        let gitlink_index = index_bytes(2, 20, &[(0o160000, "sub")], &[]);
        let dir = tmp();
        let root = fs::canonicalize(dir.path()).unwrap();
        // A work tree, optionally with a Git directory configuration and
        // an index that could list gitlinks.
        let work_tree = |path: &Path, config: &[u8], gitlinks: bool| {
            fs::create_dir_all(path.join(".git")).unwrap();
            write(&path.join(".git/config"), config);
            if gitlinks {
                write(&path.join(".git/index"), &gitlink_index);
            }
        };
        let gitlink = |path: &str| format!("100644 0abc 0\tf\0160000 0def 0\t{path}\0");
        // `git ls-files` answers per directory.
        let listings: std::collections::HashMap<PathBuf, (i32, String, String)> = [
            ("unpopulated", gitlink("missing")),
            ("dotdot", gitlink("../dotdot")),
            ("symlinked", gitlink("sub")),
            ("outside", gitlink("sub")),
            ("nested", gitlink("a")),
            ("nested/a", gitlink("b")),
            ("moved", gitlink("sub")),
            ("moved/sub", gitlink("x")),
            (
                "wide",
                (0..=MAX_SUBMODULES)
                    .map(|n| gitlink(&format!("s{n}")))
                    .collect(),
            ),
        ]
        .into_iter()
        .map(|(path, listing)| (root.join(path), (0, listing, String::new())))
        .chain((0..=MAX_SUBMODULE_DEPTH).map(|depth| {
            let path = root.join("deep").join("d/".repeat(depth));
            (
                path.components().collect(),
                (0, gitlink("d"), String::new()),
            )
        }))
        .chain([(
            root.join("failing"),
            (
                128,
                String::new(),
                "fatal: index file corrupt\n".to_string(),
            ),
        )])
        .collect();
        for name in ["unpopulated", "dotdot", "symlinked", "outside", "failing"] {
            work_tree(&root.join(name), b"[core]\n", true);
        }
        fs::create_dir_all(root.join("symlinked/sub")).unwrap();
        symlink(root.join("elsewhere"), root.join("symlinked/sub/.git")).unwrap();
        work_tree(&root.join("elsewhere"), b"[core]\n", false);
        fs::create_dir_all(root.join("outside/sub")).unwrap();
        write(
            &root.join("outside/sub/.git"),
            format!("gitdir: {}\n", root.join("elsewhere/.git").display()).as_bytes(),
        );
        // A submodule whose `core.worktree` leads outside.
        work_tree(&root.join("moved"), b"[core]\n", true);
        work_tree(&root.join("moved/sub"), b"[core]\n\tworktree = /\n", true);
        work_tree(&root.join("nested"), b"[filter \"top\"]\n", true);
        work_tree(&root.join("nested/a"), b"[filter \"a\"]\n", true);
        work_tree(&root.join("nested/a/b"), b"[include]\n", false);
        let mut deep = root.join("deep");
        for _ in 0..=MAX_SUBMODULE_DEPTH + 1 {
            work_tree(&deep, b"[core]\n", true);
            deep.push("d");
        }
        work_tree(&root.join("wide"), b"[core]\n", true);
        for n in 0..=MAX_SUBMODULES {
            fs::create_dir_all(root.join(format!("wide/s{n}/.git"))).unwrap();
        }

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let seen = std::sync::Arc::clone(&log);
        let root_for_exec = root.clone();
        let exec = fake(move |args| {
            seen.lock().unwrap().push(args.to_vec());
            let dir = args
                .iter()
                .position(|a| a == "-C")
                .map(|i| PathBuf::from(&args[i + 1]))
                .unwrap();
            if args.iter().any(|a| a == "ls-files") {
                let (code, stdout, stderr) = listings.get(&dir).cloned().unwrap_or_default();
                return CommandResult::new(code, &stdout, &stderr, &args[0]);
            }
            if args.iter().any(|a| a == "config") {
                let name = dir.file_name().unwrap().to_string_lossy();
                let name = if name == "nested" { "top".into() } else { name };
                return CommandResult::new(0, &format!("filter.{name}.clean\0"), "", &args[0]);
            }
            if args.iter().any(|a| a == "--show-toplevel") {
                let top = format!("{}\n", root_for_exec.join("elsewhere").display());
                return CommandResult::new(0, &top, "", &args[0]);
            }
            let stdout = if args.iter().any(|a| a == "status") {
                "# branch.oid 0abc\n# branch.head main\n"
            } else {
                ""
            };
            CommandResult::new(0, stdout, "", &args[0])
        });
        let missing = root.join("no-system-config");
        let names = [
            "unpopulated",
            "dotdot",
            "symlinked",
            "outside",
            "failing",
            "moved",
            "nested",
            "deep",
            "wide",
        ];
        let roots: Vec<String> = names
            .iter()
            .map(|name| spelling(&root.join(name)))
            .collect();
        let (items, warnings) = collect_repositories_with(&exec, &roots, 0, 1.0, &missing);
        let status: std::collections::HashMap<&str, &str> = items
            .iter()
            .map(|item| (item.name.as_str(), item.status.as_str()))
            .collect();
        assert_eq!(status["unpopulated"], "clean");
        assert_eq!(status["nested"], "clean");
        let deep_path = format!("{}d", "d/".repeat(MAX_SUBMODULE_DEPTH));
        let mut expected = vec![
            (
                "dotdot",
                "submodule '../dotdot': path cannot be checked".to_string(),
            ),
            (
                "symlinked",
                "submodule 'sub': .git is a symbolic link".to_string(),
            ),
            (
                "outside",
                "submodule 'sub': git directory is outside the repository".to_string(),
            ),
            (
                "failing",
                "submodule check failed: fatal: index file corrupt".to_string(),
            ),
            (
                "moved",
                "submodule 'sub': work tree is outside the repository".to_string(),
            ),
            (
                "deep",
                format!(
                    "submodule {}: too many submodules to check",
                    repr_str(&deep_path)
                ),
            ),
            (
                "wide",
                format!("submodule 's{MAX_SUBMODULES}': too many submodules to check"),
            ),
        ];
        expected.sort();
        let mut reported: Vec<(&str, String)> = expected
            .iter()
            .map(|(name, _)| {
                assert_eq!(status[name], "unknown", "{name}");
                let prefix = format!(
                    "git status failed for {}: not run: ",
                    root.join(name).display()
                );
                let warning = warnings
                    .iter()
                    .find(|warning| warning.starts_with(&prefix))
                    .unwrap_or_else(|| panic!("{name}: {warnings:?}"));
                (*name, warning[prefix.len()..].to_string())
            })
            .collect();
        reported.sort();
        assert_eq!(reported, expected);
        assert_eq!(warnings.len(), expected.len(), "{warnings:?}");

        // Each call as (directory, arguments after it, whole call).
        let calls: Vec<(PathBuf, Vec<String>, Vec<String>)> = log
            .lock()
            .unwrap()
            .iter()
            .map(|call| {
                let at = call.iter().position(|a| a == "-C").unwrap();
                (
                    PathBuf::from(&call[at + 1]),
                    call[at + 2..].to_vec(),
                    call.clone(),
                )
            })
            .collect();
        let within = |name: &str| -> Vec<&(PathBuf, Vec<String>, Vec<String>)> {
            calls
                .iter()
                .filter(|(dir, _, _)| dir.starts_with(root.join(name)))
                .collect()
        };
        // An unpopulated gitlink is skipped like Git skips it.
        let unpopulated: Vec<&str> = within("unpopulated")
            .iter()
            .map(|(_, rest, _)| rest[0].as_str())
            .collect();
        assert_eq!(unpopulated, ["ls-files", "status", "log"]);
        // Queries inside a submodule run the way `git status` checks it,
        // and every driver found, at every depth, is disabled.
        let nested = within("nested");
        for (dir, rest, _) in &nested {
            let inside = *dir != root.join("nested");
            assert_eq!(rest[0] == "--git-dir=.git", inside, "{rest:?}");
        }
        let (_, _, status_call) = nested
            .iter()
            .find(|(_, rest, _)| rest[0] == "status")
            .unwrap();
        for name in ["top", "a", "b"] {
            assert!(
                status_call.contains(&format!("filter.{name}.clean=")),
                "{name}: {status_call:?}"
            );
        }
        // A refused repository never runs `git status`.
        for (name, _) in &expected {
            assert!(
                !within(name).iter().any(|(_, rest, _)| rest[0] == "status"),
                "{name}"
            );
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
