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
- Filter drivers configured for a scanned repository are switched off for its `git status`, so a crafted `.git/config` cannot make a scan run its `clean` or `process` commands.

## Scope Limits

- Git checks submodule work trees with each submodule's own configuration, which oremap does not inspect. Scan only repositories you trust, as you would before running `git status` in them yourself.

