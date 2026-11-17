# Security

langis handles Signal keys, messages and an encrypted database, so security
reports are welcome and taken seriously.

## Reporting a vulnerability

Please don't open a public issue. Report it privately through GitHub's
private vulnerability reporting: the **Security** tab of this repository,
then **Report a vulnerability**. Include what you found, how to reproduce
it, and what an attacker could do with it.

You'll get an answer within a week. Once a fix is ready, the report is
credited in the commit unless you'd rather it wasn't.

## Scope

In scope: this repository's code, the crates in `crates/` and the changes
to the vendored libraries in `vendor/` (listed in `vendor/README.md`).

Out of scope, and better reported upstream:

- The Signal protocol, libsignal and RingRTC: Signal, at
  <https://signal.org/security/>.
- presage and libsignal-service-rs as they are upstream: the whisperfish
  projects on GitHub.
- Signal's servers and the official apps.

## What has been reviewed

A security code review of the core, calls, Storage Service sync, key
transparency and every vendored change was done in November 2026, by AI
reviewers rather than a professional audit. Its findings are fixed. No
professional audit has been done; treat langis as early software.
