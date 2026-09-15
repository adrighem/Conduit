# Slash Command Detection and Giphy Support Plan

## Phase 1: Generic Slash Command Detection & Allowlist

- [x] Task: Implement generic slash command tokenizer and allowlist validation in `src/composer.rs` 842c376
- [x] Task: Add composer UI feedback and guard against sending unsupported slash commands c2e2fbc
- [x] Task: Unit test slash command detection, argument extraction, and allowlist validation 23acb5d

## Phase 2: Slack API Client & Runtime Dispatch

- [ ] Task: Implement `chat.command` endpoint in `src/slack.rs` with error handling
- [ ] Task: Add `RuntimeCommand::ExecuteSlashCommand` to `src/runtime.rs` and `src/runtime_mailbox.rs`
- [ ] Task: Wire slash command execution from composer through runtime dispatch in `src/window.rs`
- [ ] Task: Unit and integration tests for `chat.command` client calls and runtime command admission

## Phase 3: Giphy Command Support & Ephemeral Handling

- [ ] Task: Add `/giphy` to the supported slash command registry with parameter handling
- [ ] Task: Handle ephemeral message responses and block actions (Send, Shuffle, Cancel) for Giphy previews
- [ ] Task: Test Giphy command execution in channels and thread contexts

## Phase 4: Verification & Quality Gates

- [ ] Task: Run full test suite (`cargo test`), format check (`cargo fmt --check`), and linter (`cargo clippy`)
- [ ] Task: Verify manual user workflow for valid and invalid slash commands
