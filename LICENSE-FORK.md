# LZT Hardening Addendum

This is a Lou-maintained fork. The upstream LICENSE applies; this
addendum documents the LZT hardening policy applied on top.

## Hardening scope

- **Sanitization**: PRs touching public-facing files (README, examples,
  docs) MUST pass `exporters-check`-style grep gate (no RFC1918 IPs,
  no `/home/lou/`, no `/root/`, no PII, no recruiter handles).
- **Telemetry**: no auto-publish of runtime data. Users opt in explicitly.
- **Defensive defaults**: fail-closed on ambiguous errors. No silent
  fallbacks that could mask production issues.

## Maintenance

- Bug reports: file issues with reproduction case + expected vs actual
- PRs: reviewed within 14 days
- Security issues: see SECURITY.md (or file private issue if absent)
- Releases: tagged + signed when applicable

## Trademark

This fork is NOT a Lou-affiliated project unless explicitly stated.
"LZT" refers to the Lou's Zone Telemetry hardening framework.

## Commercial use

Permitted per upstream LICENSE.

## Re-fork

Permitted with attribution to both upstream and this fork.

## Cross-references

- Lou's hardening lab (private): https://github.com/louzt/ci-debug-lab
- This fork's purpose: defensive engineering for production CI/CD