# Security Policy

Conduit handles Slack user tokens, OAuth authorization codes, workspace metadata, and message content. Treat these as sensitive even when reporting a bug.

## Reporting A Vulnerability

Do not open a public issue for vulnerabilities or reports that include secrets, private workspace data, private messages, logs containing access tokens, OAuth authorization codes, cookies, or Slack app credentials.

Use GitHub private vulnerability reporting for this repository when available. If that is not available, contact the maintainer privately through the contact channel listed on the maintainer's GitHub profile.

Please include:

- A short description of the issue.
- Steps to reproduce, if safe to share.
- The affected Conduit commit or version.
- Your operating system and desktop environment.
- Any relevant logs with tokens, codes, cookies, workspace secrets, and private messages removed.

## Supported Versions

Conduit is currently an early development project. Security fixes target the `main` branch unless a released version is explicitly marked as supported.

## Secret Hygiene

Never post or commit:

- Slack access tokens or app-level tokens.
- OAuth authorization codes.
- Cookies or keyring contents.
- Slack app client secrets.
- Private message contents or private workspace data.

If you accidentally disclose a Slack token, revoke or rotate it in Slack immediately before continuing the report.
