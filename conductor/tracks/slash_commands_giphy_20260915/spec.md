# Slash Command Detection and Giphy Support

## Summary

Add generic slash command parsing in the message composer, restrict execution to an allowlist of supported commands, and implement end-to-end support for the `/giphy` slash command against Slack's command dispatch APIs.

## Requirements

1. **Generic Slash Command Parsing & Validation**:
   - Detect leading `/` commands in composer input (e.g., `/command [arguments]`).
   - Parse command name and argument payload cleanly without triggering standard rich-text block formatting.
   - Restrict execution to explicitly supported slash commands (initial allowlist: `/giphy`).
   - For unsupported or unknown slash commands, provide user-facing feedback (e.g., inline warning, composer validation error, or ephemeral notification) and prevent sending as a raw text message accidentally.

2. **Slack Slash Command Execution (`src/slack.rs`)**:
   - Implement `chat.command` API client method supporting `channel`, `command`, `text`, and optional `thread_ts`.
   - Handle Slack API error responses (e.g., `unknown_command`, `app_not_found`, `action_prohibited`).

3. **Runtime Command Dispatch (`src/runtime.rs`, `src/runtime_mailbox.rs`)**:
   - Add runtime command variants for slash command dispatch with appropriate mutation lane handling and admission rules.
   - Maintain consistency with single-workspace boundary rules.

4. **Interactive Giphy Flow & Ephemeral Handling**:
   - Handle `/giphy` execution responses, including ephemeral preview responses.
   - Wire interactive block actions (Send, Shuffle, Cancel) if the workspace uses interactive Giphy mode.
   - Render resulting ephemeral / in-channel messages properly in the conversation timeline.

5. **Test Coverage & Regression Prevention**:
   - Unit tests for slash command tokenizer, parser, and allowlist validation in `composer.rs`.
   - Unit and integration tests for `chat.command` serialization and error responses in `slack.rs`.
   - Runtime admission and dispatch tests for slash command execution.

## Acceptance Criteria

- Typing `/giphy <query>` executes the Giphy command via Slack API instead of posting raw text `/giphy <query>`.
- Typing an unsupported slash command (e.g. `/foo bar`) is caught by validation and blocked with clear user feedback.
- Slash commands sent inside a thread include the target `thread_ts`.
- `cargo fmt --check`, `cargo clippy`, and `cargo test` pass without warnings.

## Out of Scope

- Arbitrary custom slash command registration dynamically discovered from Slack manifest.
- Non-Slack slash command providers.
- Local GIF rendering without Slack Giphy integration.
