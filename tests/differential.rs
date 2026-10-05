//! Differential tests against the Python 0.3.0 reference implementation.
//!
//! Skipped unless `OREMAP_PY_REF` (or the pre-rename name
//! `SERVICE_CARTOGRAPHER_PY_REF`) points at the reference `src/` directory
//! (for example a `git worktree` of the last Python release). The reference
//! still ships as the `service_cartographer` module; it runs with its
//! argparse program name set to `oremap`, so help wrapping is compared at the
//! new name's width. Every case must produce identical stdout, stderr, and
//! exit status, apart from the version string, the `generated_at` timestamp,
//! and the renamed Markdown report title.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

/// Runs the Python reference with `prog="service-cartographer"` replaced by
/// `oremap`. Subcommand programs (`oremap scan`) derive from the parent.
const PYTHON_ORACLE: &str = "\
import argparse, runpy
_init = argparse.ArgumentParser.__init__
def _renamed(self, *args, **kwargs):
    if kwargs.get('prog') == 'service-cartographer':
        kwargs['prog'] = 'oremap'
    _init(self, *args, **kwargs)
argparse.ArgumentParser.__init__ = _renamed
runpy.run_module('service_cartographer.cli', run_name='__main__')
";

fn reference() -> Option<String> {
    ["OREMAP_PY_REF", "SERVICE_CARTOGRAPHER_PY_REF"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|path| {
            Path::new(path)
                .join("service_cartographer/cli.py")
                .is_file()
        })
        // The oracle runs inside a temporary directory, so a relative
        // reference path (such as `../oremap-python/src`) must be made
        // absolute before it is passed as `PYTHONPATH`.
        .map(|path| {
            fs::canonicalize(&path)
                .expect("canonicalize Python reference")
                .to_string_lossy()
                .into_owned()
        })
}

fn normalize(text: &str) -> String {
    let mut out = Vec::new();
    let mut generated_done = false;
    for line in text.split('\n') {
        let line = line
            .replace("oremap 0.3.0", "oremap VERSION")
            .replace(
                &format!("oremap {}", env!("CARGO_PKG_VERSION")),
                "oremap VERSION",
            )
            .replace("# Service Cartographer Report", "# oremap Report");
        if !generated_done
            && (line.starts_with("  \"generated_at\": ") || line.starts_with("- Generated: "))
        {
            generated_done = true;
            out.push("GENERATED".to_string());
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

fn run(program: &mut Command, cwd: &Path, home: &Path, args: &[&str]) -> (String, String, i32) {
    let output = program
        .current_dir(cwd)
        .env("HOME", home)
        .env("COLUMNS", "80")
        .args(args)
        .output()
        .expect("run");
    (
        normalize(&String::from_utf8_lossy(&output.stdout)),
        normalize(&String::from_utf8_lossy(&output.stderr)),
        output.status.code().unwrap_or(-1),
    )
}

fn write_executable(path: &Path, content: &[u8]) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn build_fixture(root: &Path) {
    let work = root.join("work");
    fs::create_dir_all(work.join("wt")).unwrap();
    fs::write(work.join("wt/.git"), "gitdir: /outside/not-read\n").unwrap();
    fs::create_dir_all(work.join("bare.git/objects")).unwrap();
    fs::create_dir_all(work.join("bare.git/refs")).unwrap();
    fs::write(work.join("bare.git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::create_dir_all(work.join("ext.git")).unwrap();
    fs::create_dir_all(work.join("linked")).unwrap();
    symlink("../ext.git", work.join("linked/.git")).unwrap();
    for nested in [
        "node_modules/pkg/.git",
        ".cachex/r/.git",
        "deep/a/b/.git",
        "plain/.git",
    ] {
        fs::create_dir_all(work.join(nested)).unwrap();
    }
    fs::write(work.join(".env"), "TOKEN=secret\nPUBLIC=1\n").unwrap();
    fs::write(
        work.join(".envrc"),
        b"export A=1\n B = 2\n#C=3\n\xe2\x80\x83D=4\nE\x1f=5\n1F=6\n_G=7\nH=8\r\nI=9\n",
    )
    .unwrap();
    fs::write(work.join("app.env"), "X=1\n").unwrap();
    symlink(".env", work.join(".env.link")).unwrap();
    let mut big = b"FIRST=ok\n".to_vec();
    big.extend(std::iter::repeat_n(b'X', 65_546));
    fs::write(work.join("big.env"), big).unwrap();
    fs::create_dir_all(work.join("sub/deeper/deepest")).unwrap();
    fs::write(work.join("sub/.env"), "SUB=1\n").unwrap();
    fs::write(work.join("sub/deeper/deepest/.env"), "DEEPEST=1\n").unwrap();

    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    write_executable(&bin.join("tool"), b"#!/usr/bin/env bash\necho hi\n");
    write_executable(&bin.join("spaced"), b"   #!/bin/sh -e  \r\nrest\n");
    write_executable(
        &bin.join("secret-tool"),
        b"#!/bin/sh TOKEN=abc --password pw\n",
    );
    write_executable(&bin.join("=formula"), b"#!/bin/sh\n");
    write_executable(&bin.join("pipe|name"), b"#!/bin/sh\n");
    fs::write(bin.join("noexec"), "#!/bin/sh\n").unwrap();

    let units = root.join("units");
    fs::create_dir_all(units.join("c.path")).unwrap();
    for name in [
        "a.service",
        ".hidden.service",
        "b.timer",
        "noext",
        "x.txt",
        ".service",
    ] {
        fs::write(units.join(name), "").unwrap();
    }
    symlink("/nonexistent", units.join("d.socket")).unwrap();
}

#[test]
fn rust_matches_python_reference() {
    let Some(reference) = reference() else {
        eprintln!("skipped: set OREMAP_PY_REF to the Python reference src/");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    build_fixture(&root);

    let base = [
        "--repo-root",
        "work",
        "--env-root",
        "work",
        "--wrapper-dir",
        "bin",
        "--no-cron",
    ];
    let mut cases: Vec<Vec<&str>> = vec![
        vec!["--help"],
        vec!["scan", "--help"],
        vec!["--version"],
        vec!["bogus"],
        vec!["scan", "--format", "xml"],
        vec!["scan", "--no"],
        vec!["scan", "-x"],
        vec!["scan", "--stale-days", "it's"],
    ];
    for format in ["json", "markdown", "csv"] {
        let mut case = vec!["scan", "--format", format, "--systemd-scope", "off"];
        case.extend(base);
        cases.push(case.clone());
        case.extend([
            "--include-env-names",
            "--absolute-paths",
            "--repo-depth",
            "4",
        ]);
        cases.push(case);
    }
    let mut focused = vec!["scan", "--format", "json", "--systemd-scope", "off"];
    focused.extend(base);
    focused.extend([
        "--focus",
        "TOOL",
        "--retire-keyword",
        "spaced",
        "--keep-keyword",
        "secret",
    ]);
    cases.push(focused);
    cases.push(vec![
        "scan",
        "--format",
        "json",
        "--systemd-scope",
        "user",
        "--unit-dir",
        "units",
        "--no-systemd-discovered",
        "--no-cron",
        "--no-repos",
        "--no-env",
        "--no-wrappers",
    ]);
    cases.push(vec![
        "scan",
        "--format",
        "json",
        "--systemd-scope",
        "user",
        "--unit-dir",
        "units",
        "--timeout",
        "0",
        "--no-cron",
        "--no-repos",
        "--no-env",
        "--no-wrappers",
    ]);

    let mut mismatches = Vec::new();
    for case in &cases {
        let mut python = Command::new("python3");
        python
            .env("PYTHONPATH", &reference)
            .args(["-c", PYTHON_ORACLE]);
        let expected = run(&mut python, &root, &root, case);
        let mut rust = Command::new(env!("CARGO_BIN_EXE_oremap"));
        let actual = run(&mut rust, &root, &root, case);
        if expected != actual {
            mismatches.push(format!(
                "{case:?}\n--- python\n{expected:?}\n--- rust\n{actual:?}"
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n\n"));
}
