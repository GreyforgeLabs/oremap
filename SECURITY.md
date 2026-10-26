# Security Policy

## Supported Versions

Security fixes target the latest released version.

## Reporting a Vulnerability

Report vulnerabilities through GitHub security advisories or by contacting Greyforge through the public site at <https://greyforge.tech>.

## Threat Model

`oremap` (formerly `service-cartographer`) is a local read-only scanner. It must not start, stop, enable, disable, delete, or modify services. Reports may still describe sensitive local structure, so users should review output before sharing it.

## Privacy Commitments

- Env file values are never emitted.
- Hostname is hidden by default.
- Home paths are shortened by default.
- Inline secret-looking command assignments are redacted in cron previews.
- External tools run only from reviewed absolute paths with a minimal environment and `/dev/null` as standard input; `git status` runs with optional index locks and the file-system monitor disabled, so scans never rewrite repository state.
- Filter drivers configured for a scanned repository, and for every populated submodule its `git status` checks (found from the index's gitlinks, at any depth), are switched off for that `git status`, so a crafted `.git/config` or `.git/modules/<name>/config` cannot make a scan run its `clean` or `process` commands.
- `git log` runs with `--no-show-signature`, so a repository's `log.showSignature` cannot make a scan run its `gpg.program`, and `git` runs with `GIT_NO_LAZY_FETCH=1`, so a partial clone never fetches missing objects through its configured remote (`core.sshCommand`, a remote helper) or contacts the network. Git releases before 2.44 ignore `GIT_NO_LAZY_FETCH`.

## Scope Limits

- A repository whose submodules oremap cannot follow is reported `unknown` with a warning, and its `git status` is not run: a submodule `.git` that is a symbolic link, a submodule Git directory or work tree that resolves outside the scanned repository's work tree and Git directories, a gitlink path that is absolute or contains `.` or `..`, more than 8 levels of nesting or 512 populated submodules, a driver name that cannot be passed back with `-c`, or a failed `git` query.
- The driver check reads configuration before `git status` starts. A process that can write to the repository while the scan runs can add a driver in between; scan repositories that no other user can write to.
- Drivers set in a system configuration file other than `/etc/gitconfig` (a Git built with a different `sysconfdir`) are found only when the repository's own configuration also mentions `filter` or `include`.

