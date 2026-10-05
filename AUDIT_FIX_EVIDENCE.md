# Audit remediation evidence

> Historical record for the Python 0.2.0 release. The validation commands
> below applied to that implementation; the current Rust checks are listed in
> [CONTRIBUTING.md](CONTRIBUTING.md).

Release: `0.2.0`

Date: 2026-08-28

## GF-AUD-011

- `git`, `systemctl`, and `crontab` resolve only from reviewed absolute candidates.
- Selected paths and bounded version banners are attached to collector metadata.
- External commands receive a fixed minimal environment and report distinct `timeout`,
  `exec-error`, `nonzero`, and `ok` states.
- Regression tests prepend hostile PATH entries, remove approved binaries, and exercise timeout,
  permission, and nonzero outcomes.

## GF-AUD-012

- One shared sanitizer neutralizes `=`, `+`, `-`, and `@` after any leading whitespace.
- Every string cell in both CSV paths uses the policy; JSON retains the original exact strings.
- Regression tests cover all dangerous prefixes, tab/CR/space prefixes, every report column,
  ordinary values, and JSON exactness.

## GF-AUD-032

- Git discovery accepts regular `.git` directories/files and bare layouts while rejecting marker
  symlinks and descriptor-replacement races.
- Env discovery rejects symlinks and reads regular files through a no-follow descriptor with byte
  and line caps.
- Walk, disappearance, and permission errors are recorded without aborting unrelated inventory.
- Regression tests cover worktree/bare layouts, external env symlinks, oversized env files, and a
  disappearing file beside healthy inventory.

## Validation

- Tier 2: `PYTHONPATH=src python -m pytest -q`
- Static: `python -m ruff check src tests`
- Formatting: `python -m ruff format --check src tests`
- Packaging: `python -m build`; `python -m twine check dist/*`
- Patch hygiene: `git diff --check`
