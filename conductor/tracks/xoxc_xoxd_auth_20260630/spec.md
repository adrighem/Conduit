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
- Preserve the existing OAuth PKCE flow as the primary UI connection path.
- Do not log imported token or cookie values.
- Document the environment variables and security expectations.

## Acceptance Criteria

- With no stored keyring token and both XOXC/XOXD variables set, Conduit imports the browser session and connects through the existing Slack runtime path.
- If only one browser-session variable is set, startup reports a clear authentication configuration error.
- Slack API requests built from imported credentials include `Authorization: Bearer <xoxc>` and `Cookie: d=<xoxd>`.
- Optional user-agent configuration is applied to Slack requests for browser-session credentials.
- Unit tests cover token import parsing and authenticated request header construction.
- README documents the token import flow and links to the upstream Slack MCP documentation.

## Out of Scope

- Interactive UI fields for pasting XOXC/XOXD values.
- Support for bot-token-only authentication.
- Custom TLS fingerprinting.
- Slack Edge API replacement for existing Web API calls.
