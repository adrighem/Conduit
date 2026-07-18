# Initial Concept

Conduit is a lightweight GNOME desktop client for Slack written in Rust with GTK4, libadwaita, and WebKitGTK.

# Product Overview

## Target Users
- Linux/GNOME users who want a native Slack client.
- Developers and power users who prefer local, keyring-backed Slack access.

## Goals
- Provide a native desktop shell for Slack conversations and workspace navigation.
- Keep authentication local and understandable.
- Support practical Slack workflows such as reading conversations, search, saved items, reactions, message posting, and file uploads.

## Key Features
- Slack OAuth PKCE user-token authentication.
- Secure token storage through the system keyring.
- Adaptive sidebar navigation for messages, unread conversations, files, and saved items.
- Cached conversations and message histories.
- Actionable desktop notifications and internal navigation from search results.
- Desktop and browser activation through the official `slack://` scheme for workspace-safe native navigation.
- Per-conversation and per-thread drafts that survive navigation and restart.
- Slack Web API integration for read/write messaging workflows.

## Non-Goals
- Replacing every Slack web UI feature.
- Intercepting ordinary Slack HTTPS links or requiring a browser extension.
- Bot-token-only workspace operation as the primary connection path.
- Multi-workspace account switching until the core workspace experience is stable.

## Success Metrics
- Users can connect a workspace without manually editing application state.
- Common Slack read/write workflows work reliably with clear status and error messages.
- Supported `slack://` links reach the intended target without crossing workspace boundaries.
- Authentication secrets are not logged and are stored through the system keyring.
