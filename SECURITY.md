# Security policy

hooktop handles sensitive data: your prompts, Claude's replies and tool output, which can include file contents and secrets. Security reports are welcome.

## Reporting a vulnerability

Please **don't open a public issue**. Report it privately through GitHub: **Security → Report a vulnerability** on this repository. Include what you found, how to reproduce it, and the version (`hooktop --version`).

You'll get a reply within a week. Fixes go out in a new release, and you'll be credited unless you'd rather not be.

## Scope

hooktop is meant to be reachable only by you, on your own machine. In scope:

- anything that lets a web page, another user account, or another machine read or change hooktop's data,
- ways around the token, `Origin`, `Host` or file-permission checks,
- leaks of the token or stored data.

Out of scope: code already running as your own user, which can read hooktop's files directly. See [docs/architecture/security.md](docs/architecture/security.md) for the full threat model.
