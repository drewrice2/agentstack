# Security Policy

## Reporting a Vulnerability

Please report security issues privately instead of opening a public issue.
Use GitHub private vulnerability reporting when it is available for this
repository. If it is unavailable, contact maintainers privately through the
repository's existing maintainer channels before public disclosure.

Include the affected version or commit, reproduction steps, impact, and any
known workaround. Do not include live credentials, tokens, or private registry
data in reports.

## Dependency Checks

CI runs formatting, Clippy, tests, and `cargo deny` for the full Cargo
workspace. The root `deny.toml` fails vulnerability, unmaintained, and unsound
advisories by default, allows only crates.io registry sources, denies unknown
Git sources, and warns on duplicate crate versions.

CI fails `cargo deny` on known vulnerabilities. There are no ignored
advisories.
