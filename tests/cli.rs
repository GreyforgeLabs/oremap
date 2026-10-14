//! End-to-end tests for the `oremap` binary and its deprecated alias.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

fn bin() -> Command {
    let mut command = Command::cargo_bin("oremap").expect("binary built");
    command.env("COLUMNS", "80");
    command
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf-8 stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf-8 stderr")
}

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn running_as_root() -> bool {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("Uid:"))
                .map(|line| line.split_whitespace().nth(2) == Some("0"))
        })
        .unwrap_or(false)
}

/// Pull `"key": "value"` string values out of the report without a JSON crate.
fn json_strings(json: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\": \"");
    json.lines()
        .filter_map(|line| {
            let start = line.find(&needle)? + needle.len();
            let rest = &line[start..];
            Some(rest[..rest.rfind('"')?].to_string())
        })
        .collect()
}

// tests/test_cli.py::test_cli_json_scan_with_local_fixture
#[test]
fn test_cli_json_scan_with_local_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_root = tmp.path().join("repo");
    fs::create_dir(&repo_root).unwrap();
    fs::write(repo_root.join(".env"), "TOKEN=secret\n").unwrap();
    let wrapper_dir = tmp.path().join("bin");
    fs::create_dir(&wrapper_dir).unwrap();
    write_executable(&wrapper_dir.join("demo-wrapper"), "#!/usr/bin/env bash\n");

    let output = bin()
        .args([
            "scan",
            "--format",
            "json",
            "--systemd-scope",
            "off",
            "--no-cron",
            "--no-repos",
        ])
        .arg("--repo-root")
        .arg(&repo_root)
        .arg("--env-root")
        .arg(&repo_root)
        .arg("--wrapper-dir")
        .arg(&wrapper_dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let json = stdout(&output);
    assert!(json.contains("\"complete\": true"));
    // Persisted identifier: the rename to oremap keeps the old schema value
    // so existing report consumers keep matching it exactly.
    assert_eq!(
        json_strings(&json, "schema"),
        ["service-cartographer.report.v1"]
    );
    let mut names = json_strings(&json, "name");
    names.sort();
    assert_eq!(names, [".env", "demo-wrapper"]);
    assert!(!json.contains("secret"));
}

// tests/test_cli.py::test_cli_redacts_warnings (end-to-end variant: a real
// unreadable directory whose path contains the home prefix and a token)
#[test]
fn test_cli_redacts_warnings() {
    if running_as_root() {
        eprintln!("skipped: permission checks do not apply to root");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let home_path = fs::canonicalize(home.path()).unwrap();
    let secret_dir = home_path.join("private").join("TOKEN=abc123");
    fs::create_dir_all(&secret_dir).unwrap();
    fs::set_permissions(&secret_dir, fs::Permissions::from_mode(0o000)).unwrap();

    let output = bin()
        .env("HOME", &home_path)
        .args([
            "scan",
            "--format",
            "json",
            "--systemd-scope",
            "off",
            "--no-cron",
        ])
        .args(["--no-repos", "--no-wrappers", "--env-depth", "5"])
        .arg("--env-root")
        .arg(&home_path)
        .output()
        .unwrap();
    fs::set_permissions(&secret_dir, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(output.status.code(), Some(1));
    let json = stdout(&output);
    assert!(json.contains("\"complete\": false"));
    assert!(!json.contains("abc123"));
    assert!(!json.contains(&home_path.display().to_string()));
    assert!(json.contains("~/private"));
    assert!(json.contains("TOKEN=<redacted>"));
}

// tests/test_cli.py::test_cli_writes_markdown
#[test]
fn test_cli_writes_markdown() {
    let tmp = tempfile::tempdir().unwrap();
    let output_path = tmp.path().join("report.md");
    let output = bin()
        .args(["scan", "--format", "markdown", "--output"])
        .arg(&output_path)
        .args([
            "--systemd-scope",
            "off",
            "--no-cron",
            "--no-repos",
            "--no-env",
        ])
        .arg("--wrapper-dir")
        .arg(tmp.path().join("missing"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    let report = fs::read_to_string(&output_path).unwrap();
    assert!(report.starts_with("# oremap Report\n"), "{report}");
    assert!(report.contains("Keep / Retire Matrix"));
    let mode = fs::metadata(&output_path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

// tests/test_cli.py::test_report_output_replaces_existing_file_privately
#[test]
fn test_report_output_replaces_existing_file_privately() {
    let tmp = tempfile::tempdir().unwrap();
    let output_path = tmp.path().join("report.json");
    fs::write(&output_path, "old content").unwrap();
    fs::set_permissions(&output_path, fs::Permissions::from_mode(0o644)).unwrap();

    let output = bin()
        .args(["scan", "--format", "json", "--output"])
        .arg(&output_path)
        .args([
            "--systemd-scope",
            "off",
            "--no-cron",
            "--no-repos",
            "--no-env",
            "--no-wrappers",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let written = fs::read_to_string(&output_path).unwrap();
    assert!(written.contains("\"complete\": true"));
    let mode = fs::metadata(&output_path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let leftovers: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() != "report.json")
        .collect();
    assert!(leftovers.is_empty(), "temporary files left behind");
}

#[test]
fn output_creates_missing_parent_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let output_path = tmp.path().join("a/b/report.csv");
    let output = bin()
        .args(["scan", "--format", "csv", "--output"])
        .arg(&output_path)
        .args([
            "--systemd-scope",
            "off",
            "--no-cron",
            "--no-repos",
            "--no-env",
            "--no-wrappers",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        fs::read_to_string(&output_path).unwrap(),
        "suggested_action,kind,name,path,status,active,enabled,last_activity,reason\r\n"
    );
}

#[test]
fn output_write_failure_exits_one() {
    let tmp = tempfile::tempdir().unwrap();
    let output = bin()
        .args(["scan", "--format", "json", "--output"])
        .arg(tmp.path())
        .args([
            "--systemd-scope",
            "off",
            "--no-cron",
            "--no-repos",
            "--no-env",
            "--no-wrappers",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).starts_with("oremap: error: "));
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
}

#[test]
fn help_and_version_match_reference_text() {
    let top = bin().arg("--help").output().unwrap();
    assert_eq!(top.status.code(), Some(0));
    assert_eq!(stdout(&top), include_str!("golden/help_top_80.txt"));

    let scan = bin().args(["scan", "--help"]).output().unwrap();
    assert_eq!(stdout(&scan), include_str!("golden/help_scan_80.txt"));

    let narrow = bin().env("COLUMNS", "40").arg("-h").output().unwrap();
    assert_eq!(stdout(&narrow), include_str!("golden/help_top_40.txt"));
    let narrow_scan = bin()
        .env("COLUMNS", "40")
        .args(["scan", "-h"])
        .output()
        .unwrap();
    assert_eq!(
        stdout(&narrow_scan),
        include_str!("golden/help_scan_40.txt")
    );

    let version = bin().arg("--version").output().unwrap();
    assert_eq!(version.status.code(), Some(0));
    assert_eq!(
        stdout(&version),
        format!("oremap {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn usage_errors_exit_two_with_reference_messages() {
    let output = bin().args(["scan", "--format", "xml"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(stderr(&output), include_str!("golden/err_format.txt"));

    let top_usage = "usage: oremap [-h] [--version] {scan} ...\n";
    let cases: [(&[&str], &str); 6] = [
        (
            &["bogus"],
            "oremap: error: argument command: invalid choice: 'bogus' (choose from 'scan')\n",
        ),
        (
            &["scan", "extra"],
            "oremap: error: unrecognized arguments: extra\n",
        ),
        (
            &["--frob"],
            "oremap: error: unrecognized arguments: --frob\n",
        ),
        (
            &["scan", "--version"],
            "oremap: error: unrecognized arguments: --version\n",
        ),
        (
            &["--format", "json"],
            "oremap: error: argument command: invalid choice: 'json' (choose from 'scan')\n",
        ),
        (
            &["-h=foo"],
            "oremap: error: argument -h/--help: ignored explicit argument 'foo'\n",
        ),
    ];
    for (args, message) in cases {
        let output = bin().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(stderr(&output), format!("{top_usage}{message}"), "{args:?}");
    }

    let scan_errors: [(&[&str], &str); 5] = [
        (
            &["scan", "--stale-days", "x"],
            "oremap scan: error: argument --stale-days: invalid int value: 'x'\n",
        ),
        (
            &["scan", "--timeout", "x"],
            "oremap scan: error: argument --timeout: invalid float value: 'x'\n",
        ),
        (
            &["scan", "--no"],
            "oremap scan: error: ambiguous option: --no could match --no-systemd-discovered, --no-cron, --no-env, --no-repos, --no-wrappers\n",
        ),
        (
            &["scan", "--focus", "-x"],
            "oremap scan: error: argument --focus: expected one argument\n",
        ),
        (
            &["scan", "--show-hostname=1"],
            "oremap scan: error: argument --show-hostname: ignored explicit argument '1'\n",
        ),
    ];
    for (args, message) in scan_errors {
        let output = bin().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let text = stderr(&output);
        assert!(text.starts_with("usage: oremap scan [-h]"), "{text}");
        assert!(text.ends_with(message), "{text}");
    }
}

#[test]
fn abbreviations_and_negative_numbers_are_accepted() {
    let output = bin()
        .args([
            "scan",
            "--form=csv",
            "--stale-days",
            "-5",
            "--systemd-scope",
            "off",
        ])
        .args(["--no-cron", "--no-repos", "--no-env", "--no-wrappers"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).starts_with("suggested_action,kind,name"));
}

#[test]
fn bare_invocation_runs_a_default_scan() {
    let home = tempfile::tempdir().unwrap();
    let output = bin()
        .current_dir(home.path())
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(matches!(output.status.code(), Some(0 | 1)));
    assert!(stdout(&output).starts_with("# oremap Report\n"));
}

#[test]
fn path_cannot_substitute_approved_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let attacker = tmp.path().join("attacker");
    fs::create_dir(&attacker).unwrap();
    let marker = tmp.path().join("attacker-ran");
    for name in ["git", "systemctl", "crontab"] {
        write_executable(
            &attacker.join(name),
            &format!("#!/bin/sh\ntouch '{}'\necho attacker\n", marker.display()),
        );
    }
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();

    let output = bin()
        .env("PATH", &attacker)
        .args(["scan", "--format", "json", "--no-env", "--no-wrappers"])
        .args(["--systemd-scope", "user", "--no-systemd-discovered"])
        .arg("--unit-dir")
        .arg(tmp.path().join("no-units"))
        .arg("--repo-root")
        .arg(&repo)
        .output()
        .unwrap();

    assert!(!marker.exists(), "a PATH-supplied tool was executed");
    let json = stdout(&output);
    assert!(!json.contains("attacker"));
    for executable in json_strings(&json, "executable") {
        assert!(executable.starts_with("/usr/bin/") || executable.starts_with("/bin/"));
    }
}

#[test]
fn csv_cells_are_spreadsheet_safe_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let wrapper_dir = tmp.path().join("bin");
    fs::create_dir(&wrapper_dir).unwrap();
    write_executable(&wrapper_dir.join("=CMD()"), "#!/bin/sh\n");
    let output = bin()
        .args([
            "scan",
            "--format",
            "csv",
            "--systemd-scope",
            "off",
            "--no-cron",
        ])
        .args(["--no-repos", "--no-env"])
        .arg("--wrapper-dir")
        .arg(&wrapper_dir)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let csv = stdout(&output);
    assert!(csv.contains("review,wrapper,'=CMD(),"), "{csv}");
    assert!(csv.ends_with("needs owner decision\r\n"));
}

/// Every env-file value below contains this marker; no report may show it.
const SECRET: &str = "S3CR3T";

#[test]
fn env_file_values_never_reach_any_report() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(
        root.join(".env"),
        format!(
            concat!(
                "API_TOKEN={s}-plain\n",
                "QUOTED=\"{s}-quoted with spaces\"\n",
                "WITH_EQUALS={s}=a=b\n",
                "PRIVATE_KEY=\"-----BEGIN PRIVATE KEY-----\n",
                "{s}BASE64LINE0123456789abcdefABCDEF==\n",
                "{s}NAMELIKE=inside the key\n",
                "-----END PRIVATE KEY-----\"\n",
                "export EXPORTED={s}-exported\n",
                "  SPACED = {s}-spaced\n",
                "# COMMENTED={s}-comment\n",
            ),
            s = SECRET
        ),
    )
    .unwrap();
    fs::write(
        root.join(".envrc"),
        format!("export DIRENV_VALUE={SECRET}-direnv\n"),
    )
    .unwrap();
    // Past the 64 KiB read limit: only metadata and a truncation warning.
    let mut big = "PADDING=x\n".repeat(7000);
    big.push_str(&format!("LATE={SECRET}-late\n"));
    fs::write(root.join("nested/big.env"), big).unwrap();

    let out_dir = tempfile::tempdir().unwrap();
    for format in ["json", "markdown", "csv"] {
        for names in [false, true] {
            let report_path = out_dir.path().join(format!("report-{format}-{names}"));
            for to_file in [false, true] {
                let mut command = bin();
                command
                    .args(["scan", "--format", format, "--systemd-scope", "off"])
                    .args(["--no-cron", "--no-repos", "--no-wrappers"])
                    .arg("--env-root")
                    .arg(&root);
                if names {
                    command.arg("--include-env-names");
                }
                if to_file {
                    command.arg("--output").arg(&report_path);
                }
                let output = command.output().unwrap();
                // The truncated file makes the report incomplete (exit 1).
                assert_eq!(output.status.code(), Some(1), "{format} {names}");
                let mut text = stdout(&output) + &stderr(&output);
                if to_file {
                    text.push_str(&fs::read_to_string(&report_path).unwrap());
                }
                assert!(!text.contains(SECRET), "{format} names={names}: {text}");
                // CSV has no warnings section; the exit status carries it.
                assert_eq!(text.contains("env-file-truncated"), format != "csv");
                if format == "json" {
                    let counts = json_strings(&text, "var_count");
                    assert_eq!(counts, ["6", "1", "1"], "{text}");
                    let expected: &[&str] = if names {
                        &[
                            "API_TOKEN,EXPORTED,PRIVATE_KEY,QUOTED,SPACED,WITH_EQUALS",
                            "DIRENV_VALUE",
                            "PADDING",
                        ]
                    } else {
                        &[]
                    };
                    assert_eq!(json_strings(&text, "var_names"), expected);
                }
            }
        }
    }
}

const ALIAS_NOTE: &str =
    "service-cartographer: renamed to oremap; this alias will be removed in the next release\n";

fn alias() -> Command {
    let mut command = Command::cargo_bin("service-cartographer").expect("alias built");
    command.env("COLUMNS", "80");
    command
}

#[test]
fn deprecated_alias_prints_one_note_then_behaves_identically() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    fs::write(root.join(".env"), "TOKEN=secret\n").unwrap();
    let cases: [&[&str]; 6] = [
        &["--help"],
        &["scan", "--help"],
        &["--version"],
        &["bogus"],
        &["scan", "--format", "xml"],
        &[
            "scan",
            "--format",
            "json",
            "--systemd-scope",
            "off",
            "--no-cron",
            "--no-repos",
            "--no-wrappers",
            "--env-root",
            ".",
        ],
    ];
    for args in cases {
        let expected = bin()
            .current_dir(&root)
            .env("HOME", &root)
            .args(args)
            .output()
            .unwrap();
        let actual = alias()
            .current_dir(&root)
            .env("HOME", &root)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(actual.status.code(), expected.status.code(), "{args:?}");
        let strip_generated = |text: String| {
            text.lines()
                .filter(|line| !line.starts_with("  \"generated_at\": "))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(
            strip_generated(stdout(&actual)),
            strip_generated(stdout(&expected)),
            "{args:?}"
        );
        assert_eq!(
            stderr(&actual),
            format!("{ALIAS_NOTE}{}", stderr(&expected)),
            "{args:?}"
        );
    }
}
