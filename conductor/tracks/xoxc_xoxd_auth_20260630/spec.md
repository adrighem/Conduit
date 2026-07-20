# Track Spec: Support Authentication Using XOXC/XOXD Tokens

## Context

Conduit currently supports Slack OAuth PKCE and stores the resulting user token in the system keyring. The Slack MCP server documentation describes an alternate browser-session setup using an `xoxc-*` Slack browser token and an `xoxd-*` Slack `d` cookie.

## Requirements

- Allow Conduit to authenticate from an XOXC/XOXD browser-session token pair when no keyring token is already stored.
- Read Conduit-specific environment variables first:
  - `CONDUIT_SLACK_XOXC_TOKEN`
  - `CONDUIT_SLACK_XOXD_TOKEN`
  - `CONDUIT_SLACK_USER_AGENT` (optional)
- Accept Slack MCP-compatible aliases:
  - `SLACK_MCP_XOXC_TOKEN`
  - `SLACK_MCP_XOXD_TOKEN`
  - `SLACK_MCP_USER_AGENT` (optional)
- Validate imported credentials with Slack `auth.test` before saving them to the keyring.
- Send the `xoxc` value as the Slack API bearer token and the `xoxd` value as cookie `d`.
- Match the established browser-session request shape by also sending `xoxc` as form field `token` and adding Slack's `d-s` session cookie.
- Preserve an explicitly supplied User-Agent instead of silently substituting a different browser fingerprint.
- Explain that Enterprise Slack may require the exact User-Agent from the browser that supplied the session and may additionally require a browser-compatible TLS handshake.
- Report browser-session validation failures with actionable, credential-safe guidance.
- Preserve the existing OAuth PKCE flow as the primary UI connection path.
- Offer XOXC/XOXD browser-session authentication as an option in the connect UI.
- Let users switch between OAuth and XOXC/XOXD entry without restarting the app.
- Do not log imported token or cookie values.
- Document the environment variables and security expectations.

## Acceptance Criteria

- With no stored keyring token and both XOXC/XOXD variables set, Conduit imports the browser session and connects through the existing Slack runtime path.
- If only one browser-session variable is set, startup reports a clear authentication configuration error.
- Slack API requests built from imported credentials include `Authorization: Bearer <xoxc>` and `Cookie: d=<xoxd>`.
- Browser-session form requests also include `token=<xoxc>` and a current `d-s` cookie.
- Optional user-agent configuration is applied to Slack requests for browser-session credentials.
- Omitting the optional User-Agent does not forge a stale browser identity, and connectivity failures explain how to retry with the exact source-browser User-Agent.
- The connect screen can toggle to XOXC/XOXD mode, enter both token values, and connect without browser OAuth.
- Incomplete XOXC/XOXD UI input shows a local validation message before any runtime command is sent.
- Unit tests cover token import parsing and authenticated request header construction.
- README documents the token import flow and links to the upstream Slack MCP documentation.

## Out of Scope

- Support for bot-token-only authentication.
- Custom TLS fingerprinting remains a separate stack decision; the UI and documentation must disclose when an Enterprise policy can require it.
- Slack Edge API replacement for existing Web API calls.
