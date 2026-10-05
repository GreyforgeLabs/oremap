# Usage Notes

## Focused Audit

Use repeated `--focus` flags to reduce a large workstation scan to one workflow family.

```bash
oremap scan \
  --repo-root ~/work \
  --wrapper-dir ~/bin \
  --focus ingest \
  --focus worker \
  --format markdown
```

## Local-Only Fixture Scan

Use this shape in tests, CI, and examples where systemd or cron should not be queried.

```bash
oremap scan --systemd-scope off --no-cron --repo-root .
```

## Classification Limits

The classifier is intentionally conservative. Active/enabled items and dirty
repositories are keep candidates. An old source timestamp only produces
`review`: modification and commit dates do not prove when an item last ran.
Only an explicit `--retire-keyword` can produce a retirement candidate. Treat
`review` as a useful result, not a failure.

JSON reports include `complete`. Exit 1 means one or more requested collectors
failed or produced a partial result; read the warnings before acting on that
report. Files written with `--output` are atomically replaced with mode `0600`.
