# Security Policy

## Supported Versions

| Version | Supported          |
|---------|--------------------|
| latest  | :white_check_mark: |
| < 1.0   | :x:                |

## Reporting a Vulnerability

Email: security@<repo-domain>.tld (replace with actual contact, or file
a private issue if email not configured).

Response time: 7 days for initial acknowledgement, 30 days for triage.

## Security Hardening (LZT additions)

- Sanitization grep gate on public-facing changes
- No telemetry or auto-publish
- CODEOWNERS enforced for sensitive paths