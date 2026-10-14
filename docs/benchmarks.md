# Benchmarks: Python 0.3.0 vs Rust 0.4.0 (oremap)

Measured on 2026-10-05 on `greyarch` (Linux 7.2.5 x86_64, AMD Ryzen 5 3600,
12 threads, 16 GB RAM) while other jobs were running (1-minute load average
about 5 to 8). Treat the numbers as indicative, not as lab-grade
measurements.

- Python: `service-cartographer` 0.3.0 from a checkout of the last Python
  release, run as `PYTHONPATH=<ref>/src python3 -m service_cartographer.cli`
  under CPython 3.14.7.
- Rust: `target/release/oremap` built with
  `cargo build --release --locked` (fat LTO, one codegen unit, stripped,
  `panic = "abort"`).

Both versions produced identical reports for every command below, apart from
the `generated_at` timestamp. The Rust timings were taken before the tool was
renamed from `service-cartographer` to `oremap`; the rename only changes the
program name and the Markdown report title.

## Wall time

`hyperfine` is not installed on the host, so each figure is the median of a
shell loop that times every run with `date +%s%N`. Each loop had two warm-up
runs and discarded their output:

```bash
for ((i=0;i<N;i++)); do
  s=$(date +%s%N); "$@" >/dev/null 2>&1; e=$(date +%s%N)
  echo $(( (e-s)/1000 ))
done | sort -n   # median taken from the sorted list
```

| Command | Runs | Python median | Rust median | Speed-up |
|---|---:|---:|---:|---:|
| `--version` | 31 | 56.56 ms | 1.93 ms | 29x |
| `--help` | 31 | 57.08 ms | 2.16 ms | 26x |
| Filesystem-only scan (1) | 21 | 64.48 ms | 3.68 ms | 18x |
| Default scan (2) | 21 | 265.75 ms | 137.12 ms | 1.9x |

1. `scan --format json --systemd-scope off --no-cron --no-repos --env-root ~/Work/workspaces/openforge-native --wrapper-dir ~/.local/bin`
   gives 60 wrapper items and walks the env-file tree with no subprocesses.
2. `scan --format json` run from `~/Work/workspaces/openforge-native`
   (user systemd, crontab probe, `~/bin` and `~/.local/bin` wrappers,
   repositories to depth 2, env files) gives 508 items: 421 units, 78
   wrappers and 9 repositories. Most of the time goes to the `systemctl` and
   `git` processes the scan runs. Rust starts the independent read-only
   queries at the same time and merges the results in their original order.
   That is where most of its gain on this command comes from.

## Peak memory (RSS)

`/usr/bin/time` is not installed on the host, so a minimal stand-in measured
peak memory. It forks, execs the command, calls `wait4` and prints
`ru_maxrss` (KiB), which is the same field `time -v` reports. As with
`time -v`, the value includes any child processes the command waited for.
Each figure is the median of 11 runs.

| Command | Python | Rust |
|---|---:|---:|
| `--version` | 17,864 KiB | 2,664 KiB |
| Filesystem-only scan (1) | 18,032 KiB | 2,960 KiB |
| Default scan (2) | 19,140 KiB | 6,040 KiB |

The Rust figure for the default scan is set by the external tools it runs,
not by the scanner itself. Measured alone, `systemctl --user list-units`
peaks at about 6,500 KiB and `git status --porcelain` at about 5,800 KiB.

## Installed footprint

| Artifact | Size |
|---|---:|
| Rust release binary (`target/release/oremap`, stripped) | 852,144 bytes |
| Python package source (`src/service_cartographer/*.py`) | 41,723 bytes |
| Python bytecode cache created on first run (`__pycache__`) | about 132,700 bytes |

For 0.4.0 only, `cargo install` also installs the deprecated
`service-cartographer` alias binary (852,864 bytes), a second copy of the
same program that prints a rename notice first. It is removed in the next
release.

The Python package also needs a Python 3.11+ interpreter. On this host that
is CPython 3.14: `libpython3.14.so` alone is 6.36 MB, and the standard
library under `/usr/lib/python3.14`, not counting `site-packages`, is 68.8 MB. The Rust binary is
self-contained: its only dynamic dependencies are the system C runtime
(`libc.so.6` and `libgcc_s.so.1`), and it runs on hosts without Python.

## 0.4.0 vs 0.4.1

Measured on 2026-10-14 on the same host with a 1-minute load average of
about 17 to 21, so small differences are noise. Both binaries were built
with `cargo build --release --locked`. Each run forks, execs, and waits for
the binary, timing it with `CLOCK_MONOTONIC` and reading `ru_maxrss` from
`wait4`. The two binaries alternate run by run (2 warm-up runs, then 21
timed runs each). Each figure is the median of three such rounds' medians.
Process counts come from an `LD_PRELOAD` shim that logs every
`posix_spawn` and `fork` call.

| Command | 0.4.0 | 0.4.1 | Change |
|---|---:|---:|---:|
| `--version` | 1.73 ms | 1.52 ms | noise |
| Filesystem-only scan (1) | 4.58 ms | 4.35 ms | noise |
| Default scan (2) | 195.5 ms | 196.7 ms | noise (3) |
| 64 repositories (4) | 316.1 ms | 253.8 ms | 1.25x |
| Env-file walk, about 26,000 directories (5) | 200.4 ms | 182.9 ms | 1.10x |
| Repository discovery in `/usr/share` (6) | 328.3 ms | 162.9 ms | 2.0x |

| Processes started | 0.4.0 | 0.4.1 |
|---|---:|---:|
| Default scan (2), 6 repositories | 22 | 16 |
| 64 repositories (4) | 193 | 129 |

Peak RSS is unchanged: about 2.7 MB for `--version` and 6.1 MB for the
default scan, where `systemctl` sets the peak. The stripped release binary
is 851,240 bytes (852,144 for 0.4.0).

1. As above.
2. As above. This host has 437 units, 93 wrappers and 6 repositories.
3. `systemctl --user list-unit-files` alone takes about 190 ms here; the
   scan waits for it, so the default scan cannot finish sooner.
4. `scan --format json --systemd-scope off --no-cron --no-env --no-wrappers --repo-root ~/Work/workspaces --repo-depth 2`.
5. `scan --format json --systemd-scope off --no-cron --no-repos --no-wrappers --env-root ~/Work --env-depth 4`.
6. `scan --format json --systemd-scope off --no-cron --no-env --no-wrappers --repo-root /usr/share --repo-depth 6`
   walks about 9,000 directories that hold 46,000 symlinks and no repositories.
