# Security policy

## Reporting a vulnerability

If you've found a security vulnerability in `abracadabra`, please
report it privately by emailing:

    matsuro-hadouken@protonmail.com

Please do **not** open a public GitHub issue for security reports.

When reporting, include:

- A description of the vulnerability
- Steps to reproduce
- The version (`abracadabra --version`) and platform
- Any suggested mitigation

You should receive an acknowledgement within 72 hours. We aim to
issue a patch release within 14 days of confirmation.

## Supported versions

Only the latest minor release line receives security updates. For
example, while `v0.2.x` is current, only `v0.2.x` is supported —
`v0.1.x` is not.

## Scope

`abracadabra` reads local log files and renders them in a terminal
UI. It does **not** make network calls, spawn shells, or write
outside `/tmp/abracadabra-yank-N.txt` (the alert-yank target).
Vulnerabilities of interest:

- Malicious log input that causes a panic, infinite loop, or
  unbounded memory growth in the parser
- TUI rendering that could be exploited via terminal escape
  sequences from a crafted log line
- Path-traversal or symlink issues with the yank target
- Build-time supply chain (dependency vulnerabilities, captured
  via `cargo audit` in CI)

Out of scope: bugs in the log files themselves (we just read what
the validator emits) and reports against unsupported Solana /
Alpenglow versions.
