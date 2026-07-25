# Rich Bot Messages Design

## Status

Implemented architecture for rendering and interacting with Slack messages produced by apps and
bots. The canonical implementation lives in `rich_message.rs`, `rich_message_normalize.rs`,
`slack_message_wire.rs`, `message_html/rich_*`, and `message_handoff.rs`.

This design covers:

- legacy secondary attachments, including attachment actions;
- Block Kit, including rich text and interactive elements;
- bot and app identity;
- lossless cache round-tripping for supported fields;
- safe URL actions;
- honest fallback for callback actions that Conduit cannot invoke through a public Slack API.

It does not turn Conduit into a Block Kit authoring tool or a host for Slack apps.

## Original Problem

Conduit previously deserialized Web API history and realtime events directly into `SlackMessage`.
That type retains text, files, and raw blocks, but drops legacy attachments and bot/app identity.
The renderer supports only a subset of blocks, treats callback actions as inert labels, and uses
`attachments_html` for files rather than Slack attachments.

That caused two distinct failures:

1. Attachment-only messages, such as some Bob messages, can become blank and lose their author.
2. Block Kit messages, such as Jira messages, can display content but cannot expose their controls
   accurately. URL buttons work; callback buttons, selects, and overflow menus do not.

The existing cache cannot recover fields that were discarded before serialization.

## Product Boundary

Conduit should make every message understandable and every visible control honest.

- Content and identity are rendered natively.
- Safe HTTP(S) URL actions execute natively by opening the URL.
- Conduit-owned actions such as reactions continue through Conduit's runtime.
- Callback actions owned by another Slack app open the exact message in Slack.
- A callback control must never look as if Conduit executed it when it only opened Slack.
- Conduit must not fabricate an interaction payload or post directly to an app's Request URL.

Slack documents app interaction as a Slack-to-app flow: after a user interacts, Slack sends the
payload to the publishing app's configured Request URL or Socket Mode connection. Slack does not
document a public Web API that lets an independent client invoke another app's action.

References:

- [Handling user interaction](https://docs.slack.dev/interactivity/handling-user-interaction/)
- [Block action payloads](https://docs.slack.dev/reference/interaction-payloads/block_actions-payload)
- [Legacy interactive messages](https://docs.slack.dev/legacy/legacy-messaging/legacy-making-messages-interactive/)
- [Get a message permalink](https://docs.slack.dev/reference/methods/chat.getPermalink/)

## Architecture

```text
Web API / realtime payload
          |
          v
Slack wire decoder -- bounded, typed, never persisted or logged
          |
          v
Message normalizer -- one canonical MessageDocument and MessageAuthor
          |
          v
WorkspaceMutation -> WorkspaceCoordinator
          |                    |
          |                    +-> StoreBatch -> versioned derived cache
          v
WorkspacePatch -> TimelinePresenter
          |
          v
MessageRenderPlan + ActionCapabilities
          |
          v
semantic HTML / opaque control handles
          |
          +-> URL executor
          +-> exact-message Slack handoff
```

Parsing, domain state, presentation, and execution are separate layers. No renderer or WebKit
callback talks to Slack directly.

## 1. Slack Wire Boundary

Introduce transport-only DTOs in a focused module, for example `slack_message_wire.rs`.

```rust
struct SlackMessageWire {
    message_fields: SlackMessageFieldsWire,
    blocks: Vec<SlackBlockWire>,
    attachments: Vec<SlackAttachmentWire>,
    bot_id: Option<String>,
    app_id: Option<String>,
    bot_profile: Option<SlackBotProfileWire>,
    icons: Option<SlackIconsWire>,
}
```

The DTOs describe the Slack shapes Conduit deliberately supports. They are immediately normalized
and are never written to SQLite. They should not derive `Debug` if that could expose arbitrary
action values or message content.

Use raw JSON only inside the decoder where Slack's union shapes require it. Do not add a flattened
top-level `extra` map to the persisted model: unknown future fields could contain credentials,
short-lived response URLs, or unbounded data.

The decoder must impose limits before allocation or rendering:

- number of blocks, attachments, fields, controls, and options;
- string and URL lengths;
- nested rich-text depth;
- total normalized message size.

Malformed nodes are skipped individually. A malformed node must not discard valid siblings or the
message's accessible fallback.

Both Web API and realtime input use this same decoder and normalizer. Search-result conversion
should use it when full message payloads are available.

## 2. Canonical Message Domain

Keep Slack transport details out of the renderer. Evolve `SlackMessage` into an envelope containing
canonical author and content models:

```rust
struct SlackMessage {
    id_and_thread: MessageIdentity,
    author: MessageAuthor,
    document: MessageDocument,
    files: Vec<SlackFile>,
    reactions_and_reply_state: MessageMetadata,
    content_version: u16,
}

enum MessageAuthor {
    User { user_id: String },
    App {
        app_id: Option<String>,
        bot_id: Option<String>,
        display_name: String,
        avatar: Option<SafeImageRef>,
    },
    Unknown { display_name: String },
}

struct MessageDocument {
    nodes: Vec<MessageNode>,
    accessible_fallback: Option<String>,
    controls: Vec<MessageControl>,
}
```

`MessageNode` is a small presentation-independent AST:

```rust
enum MessageNode {
    Text(MrkdwnText),
    RichText(Vec<RichTextNode>),
    Header(PlainText),
    Section(SectionNode),
    Context(Vec<InlineNode>),
    Divider,
    Fields(Vec<FieldNode>),
    Image(ImageNode),
    Attachment(AttachmentNode),
    Actions(ActionGroup),
    Unsupported(UnsupportedNode),
}
```

The initial supported rich-text subset should include paragraphs, links, mentions, emoji, inline
styles, quotes, preformatted text, and ordered/bulleted lists. Unsupported nodes carry only a safe
type label and accessible fallback, never the raw payload.

Legacy attachments normalize into the same AST as blocks. Attachment-specific visual information
such as color bars, title links, author labels, fields, thumbnails, and images remains available as
typed data, but attachment actions become ordinary `MessageControl` values.

### Fallback order

The normalizer, not the renderer, decides fallback behavior:

1. Render all recognized blocks and attachments in source order.
2. Preserve Slack's top-level `text` as `accessible_fallback`.
3. If recognized content is empty, render the fallback.
4. If no fallback exists, render a localized “Unsupported Slack message” notice with an
   “Open in Slack” action.

Top-level text should not be visually duplicated when blocks already represent it.

## 3. Controls and Capabilities

Represent user intent without putting Slack action values in HTML or custom URLs:

```rust
struct MessageControl {
    key: ControlKey,
    label: String,
    style: ControlStyle,
    confirmation: Option<Confirmation>,
    source: ControlSource,
    behavior: ControlBehavior,
}

enum ControlSource {
    BlockKit {
        block_id: Option<String>,
        action_id: String,
        value: Option<SensitiveValue>,
    },
    LegacyAttachment {
        attachment_index: u16,
        callback_id: String,
        name: String,
        value: Option<SensitiveValue>,
    },
    Conduit,
}

enum ControlBehavior {
    Navigate(SafeUrl),
    Submit(ControlInput),
    Menu(MenuDefinition),
    Unsupported,
}

enum ActionCapability {
    Native,
    ExternalOnly,
    Unavailable,
}
```

`ControlKey` is stable within a message revision and is derived from the source position and
non-secret identifiers. `SensitiveValue` must redact its `Debug` and display representations.

An `ActionCapabilityResolver` classifies each control for the current session:

- A URL-only control without confirmation semantics is `Native` navigation after scheme and policy
  validation. It is not presented as proof that the publishing app received an interaction.
- A URL control with Slack confirmation or callback-dependent semantics is `ExternalOnly`.
- Conduit-owned controls are `Native`.
- Slack app callback actions are `ExternalOnly`.
- Dynamic, external, user, conversation, and channel selects are `ExternalOnly`; their options and
  authorization belong to the publishing app and Slack.
- malformed or unsafe controls are `Unavailable`.

The resolver is a narrow port. A future interaction transport may implement callback actions only
if a supported or explicitly approved, verified Slack client protocol exists. Merely having Socket
Mode enabled is not sufficient: Conduit's connection belongs to Conduit's app, not Bob or Jira.

## 4. Presentation and WebKit Boundary

Build a pure `MessageRenderPlan` from the canonical document plus resolved capabilities. HTML
generation consumes this plan and has no access to raw Slack JSON.

For each rendered control:

- native URL actions render as links;
- native buttons render as semantic `<button>` elements;
- external-only controls render their original shape but include an external indicator and the
  accessible description “Open this message in Slack to use {label}”;
- unsupported selects and overflow menus remain understandable without pretending to contain
  locally usable options;
- the message also exposes one clear “Open in Slack to interact” action.

WebKit receives opaque, generation-scoped control handles such as
`conduit://message-control?id=<opaque>`. The handle registry lives beside the timeline presenter
and maps the handle back to:

- workspace/session identity;
- channel and message timestamp;
- expected message revision;
- `ControlKey`.

Never embed callback IDs, values, original messages, response URLs, or credentials in DOM
attributes, navigation URLs, JavaScript, diagnostics, or accessibility labels.

When a handle is activated, GTK resolves it against the current coordinator state. Unknown, stale,
cross-workspace, or replayed handles are rejected. The DOM is never authoritative.

## 5. Action Execution

Add a typed UI intent and runtime command:

```rust
MessageControlIntent {
    target: TimelineTarget,
    message_ts: String,
    expected_revision: WorkspaceRevision,
    control_key: ControlKey,
    input: Option<ControlInputValue>,
}
```

The execution flow for controls that Conduit can actually execute is:

1. Resolve the current message and control from `WorkspaceCoordinator`.
2. Re-evaluate its capability.
3. Show a native confirmation dialog when required by a Conduit-owned action. Slack-owned
   confirmations remain part of the Slack handoff.
4. Mark the control busy using transient presentation state.
5. Execute through `MessageActionService`.
6. Feed any message result back as `WorkspaceMutation`; never patch the DOM as source of truth.
7. If Slack is authoritative but produces no immediate update, schedule one coalesced, interactive
   message/thread refresh.
8. Clear busy state and announce success or a short actionable error.

`MessageActionService` has only two initial executors:

### URL executor

- Allows HTTP(S) only.
- Uses the existing external-link policy.
- Does not attach Slack credentials or referrer data.
- Treats a Slack URL button as navigation, not as confirmation that Slack delivered the publishing
  app's interaction payload.

### Slack handoff executor

- Requests the authoritative message URL with `chat.getPermalink`, keyed by channel and message
  timestamp. This correctly handles threads and conversation types and requires no additional
  documented scope.
- Caches the validated result and may use the existing strictly validated workspace permalink
  builder only as an offline fallback.
- Opens that exact message externally, rather than routing the permalink back into Conduit.
- Explains that the action must be completed in Slack.

Do not optimistically change messages for callback actions. Realtime or refreshed Slack state is
authoritative.

## 6. Coordinator and Persistence

Normalization happens before creating `WorkspaceMutation`. Therefore cached hydration, Web API
history, local results, and realtime updates all carry the same canonical message type through the
existing coordinator.

One changed Slack message still produces:

- at most one revisioned `WorkspacePatch`;
- at most one ordered `StoreBatch`;
- one timeline delta for the affected presentation target.

Control busy/error state is transient UI state and is not persisted. Message content, author
identity, and typed control definitions are persisted.

Add a versioned cache envelope for message content. On upgrade:

- deserialize compatible old text/files/blocks when possible;
- mark old histories as needing refresh because discarded attachments cannot be reconstructed;
- prioritize refresh for visibly blank or unsupported bot messages;
- refresh other histories lazily through the existing freshness scheduler;
- never perform an unbounded workspace-wide history fetch;
- replace old rows when fresh history or realtime data arrives.

Because Slack state is a derived cache, malformed incompatible message content can be discarded
without touching keyring credentials, drafts, or workspace settings.

## 7. Author Resolution

Author selection uses this precedence:

1. cached Slack user identity when `user_id` identifies a normal user;
2. retained `bot_profile` display name and avatar;
3. retained message `username` and icon;
4. app/bot identifier with a localized generic app label;
5. “Slack”.

App identities do not receive user presence or person-profile actions. If `app_id` is available,
Conduit may offer the existing validated Slack app handoff.

Remote avatars use the existing bounded image loader and MIME validation. They are not loaded
directly with credentials from generated HTML.

## 8. Security and Privacy

- Treat action values and option values as sensitive arbitrary data.
- Never log message bodies, action values, callback IDs paired with values, raw payloads, response
  URLs, cookies, authorization headers, or complete interaction objects.
- Do not persist raw wire payloads.
- Validate URL schemes before generating a render plan.
- Resolve actions from current coordinator state rather than trusting WebKit input.
- Scope handles to one session, timeline generation, message revision, and workspace.
- Bound control activation concurrency and suppress double activation while in flight.
- Require explicit confirmation for destructive controls when Slack supplies confirmation metadata.
- Revoke all handle registries on navigation, sign-out, session replacement, and document reload.
- Use synthetic fixtures in tests; never capture production Bob or Jira payloads in the repository.

## 9. Failure Behavior

| Condition | User-visible behavior |
|---|---|
| Unsupported block | Safe fallback plus “Open in Slack” |
| Unsafe action URL | Disabled control with a short explanation |
| Callback action | Clearly marked exact-message Slack handoff |
| Stale control handle | “This message changed; try again” and refresh |
| Missing workspace permalink | Disabled handoff and actionable status |
| Malformed attachment | Render valid siblings and fallback |
| Old lossy cache row | Nonblank refresh placeholder and prioritized fetch |
| Realtime update after handoff | Normal coordinator merge and timeline patch |

## 10. Delivery Plan

### Phase 1: Lossless readable messages

- Add wire DTOs and the canonical author/document model.
- Normalize legacy attachments, headers, rich text, fields, and common images.
- Route Web API and realtime through the same normalizer.
- Persist and round-trip the canonical content.
- Add versioned lazy refresh for old cache rows.

This phase fixes blank Bob messages and bot identity without changing interaction claims.

### Phase 2: Honest complete controls

- Normalize Block Kit and legacy buttons, static selects, and overflow menus.
- Add capability resolution and a pure render plan.
- Render URL controls natively.
- Render callback controls with exact-message Slack handoff.
- Add native confirmation for actions Conduit can actually execute.

This phase makes Jira-like messages complete and usable without pretending Conduit can dispatch
another app's callback.

### Phase 3: Pipeline integration

- Route control intents through typed runtime commands.
- Resolve opaque handles against coordinator revisions.
- Add transient busy/error presentation state.
- Feed results and follow-up refreshes through the revisioned workspace pipeline.
- Use one timeline delta per affected message.

### Phase 4: Optional verified callback transport

Only start this phase if Slack publishes a suitable client API or the project explicitly approves
and verifies another protocol.

- Implement it behind `MessageActionService`.
- Capability-gate by authentication mode and runtime verification.
- Add replay protection, rate limiting, response normalization, ephemeral-message handling, and
  modal fallback.
- Automatically degrade to exact-message Slack handoff on protocol drift or missing capability.

## Acceptance Criteria

- Attachment-only bot messages render meaningful content after a fresh fetch.
- Bot/app name and avatar survive Web API, realtime, coordinator, and cache round-trips.
- Common Block Kit and legacy attachment content never produces a blank message.
- Jira-style callback buttons, selects, and overflow menus are represented accurately.
- URL buttons open only validated HTTP(S) destinations.
- Callback controls never claim local completion and open the exact Slack message.
- No action value or raw payload appears in generated HTML, custom URLs, logs, or test artifacts.
- Stale or forged control handles cannot execute.
- A fresh/realtime message replaces an old lossy cached row without duplicates.
- Unknown nodes preserve accessible fallback and valid siblings.
- Parser, normalizer, cache, renderer, action resolution, stale-handle, and refresh behavior have
  synthetic regression coverage.

## Test Matrix

- attachment-only Bob-like message with fields, image, bot profile, reactions, and thread metadata;
- legacy callback buttons, select menus, confirmation, and malformed siblings;
- Jira-like rich text, sections, actions, static selects, and overflow menus;
- Block Kit URL button and unsafe URL;
- unknown block with top-level fallback and without fallback;
- Web API/realtime/cache normalization equivalence;
- old cache content-version upgrade and bounded lazy refresh;
- user/app/unknown author precedence;
- opaque handle success, stale revision, replay, cross-workspace, and document-generation mismatch;
- external handoff permalink construction;
- message update arriving during an in-flight action;
- HTML and diagnostics scans proving sensitive action data is absent.
