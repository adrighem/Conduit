# Project Workflow

## Guiding Principles

1. **The Plan is the Source of Truth:** All work must be tracked in `plan.md`
2. **The Tech Stack is Deliberate:** Changes to the tech stack must be documented in `tech-stack.md` *before* implementation
3. **Test-Driven Development:** Write unit tests before implementing functionality
4. **High Code Coverage:** Aim for >80% code coverage for all modules
5. **User Experience First:** Every decision should prioritize user experience
6. **Non-Interactive & CI-Aware:** Prefer non-interactive commands. Use `CI=true` for watch-mode tools (tests, linters) to ensure single execution.

## Task Workflow

All tasks follow a strict lifecycle:

### Standard Task Workflow

1. **Select Task:** Choose the next available task from `plan.md` in sequential order

2. **Mark In Progress:** Before beginning work, edit `plan.md` and change the task from `[ ]` to `[~]`

3. **Write Failing Tests (Red Phase):**
   - Create or update unit tests that clearly define the expected behavior and acceptance criteria for the task.
   - Run the tests and confirm that they fail as expected.

4. **Implement to Pass Tests (Green Phase):**
   - Write the minimum application code necessary to make the failing tests pass.
   - Run the test suite again and confirm that all tests pass.

5. **Refactor:**
   - Improve clarity and remove duplication while preserving behavior.
   - Rerun tests after refactoring.

6. **Verify Coverage:**
   - Use the project's available coverage tools when configured.
   - If coverage tooling is not configured, document that limitation.

7. **Document Deviations:**
   - Stop and update `tech-stack.md` before introducing new stack choices.

8. **Commit Code Changes:**
   - Stage code and documentation changes related to the task.
   - Commit with a concise conventional message.

9. **Attach Task Summary with Git Notes:**
   - Attach a git note to the task commit with the task name, summary, modified files, and rationale.

10. **Get and Record Task Commit SHA:**
    - Update `plan.md`, mark the completed task `[x]`, and append the first 7 characters of the task commit SHA.

11. **Commit Plan Update:**
    - Commit the modified `plan.md`.

### Phase Completion Verification and Checkpointing Protocol

**Trigger:** This protocol is executed immediately after a task is completed that also concludes a phase in `plan.md`.

1. Announce that phase verification and checkpointing has begun.
2. Run the relevant automated test command and report failures before debugging.
3. Propose a manual verification plan with exact commands and expected outcomes.
4. Await explicit user confirmation before creating a checkpoint commit.
5. Create a checkpoint commit and attach a git note with the verification report.
6. Append `[checkpoint: <sha>]` to the phase heading in `plan.md`.
7. Commit the checkpoint plan update.

## Development Commands

### Setup
```bash
meson setup _build
```

### Daily Development
```bash
cargo test
cargo check
meson compile -C _build
```

### Before Committing
```bash
cargo test
cargo check
```

## Testing Requirements

- Unit tests should cover parsing, model behavior, and pure helper logic.
- Network-dependent Slack behavior should be validated through small request-building tests where possible.
- External Slack credentials must not be required for automated tests.

## Commit Guidelines

Use conventional commits:

```bash
git commit -m "feat(auth): Add Slack browser session token import"
```
