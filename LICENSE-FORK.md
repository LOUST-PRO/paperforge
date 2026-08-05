# Lou-maintained Hardening Addendum

This is a Lou-maintained fork. The upstream LICENSE applies; this
addendum documents the hardening policy applied on top.

## Hardening scope

- **Sanitization**: PRs touching public-facing files (README, examples,
  docs) MUST pass an `exporters-check`-style grep gate that rejects
  internal IPs, home paths, PII, and recruiter handles.
- **Telemetry**: no auto-publish of runtime data. Users opt in explicitly.
- **Defensive defaults**: fail-closed on ambiguous errors. No silent
  fallbacks that could mask production issues.

## Maintenance

- Bug reports: file issues with reproduction case + expected vs actual
- PRs: reviewed within 14 days
- Security issues: see SECURITY.md (or file private issue if absent)
- Releases: tagged + signed when applicable

## Commercial use

Permitted per upstream LICENSE.

## Re-fork

Permitted with attribution to both upstream and this fork.

## Cross-references

- This fork's purpose: defensive engineering for production CI/CD