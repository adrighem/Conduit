# Attention And Notifications

Conduit keeps conversation unread state useful while limiting desktop notifications to messages
that are likely to need the current user's attention.

## User Behavior

By default, Conduit notifies for:

- Direct and group-direct messages.
- Explicit Slack mentions of the current user.
- Configured names and aliases.
- Configured keywords and phrases.
- Replies in threads the current user started, participated in, or subscribed to.

An ordinary non-self channel message still becomes locally unread, but does not notify.
Membership join/leave events, other non-message noise, and self-authored messages become neither
unread nor notifications. Muted or actively viewed messages can remain unread while desktop
delivery is suppressed; historical deliveries and realtime observations classified as
`already_observed` or `at_or_before_read_cursor` cannot create new unread or notification effects.

Preferences → Notifications controls the master desktop-notification setting and the direct
message, mention/name, and thread triggers. Names, aliases, keywords, and phrases are entered one
per line. Changes apply to the running workspace immediately, including candidates waiting for
user-name resolution.

Configured terms use Unicode case and diacritic normalization and collapse whitespace.
Alphanumeric term edges must land on word boundaries, so `alert` does not match `alerts`.
Punctuation remains significant: `on-call` and `on call` are different phrases. An optional
leading `@` is removed from configured names and aliases.

## Canonical Pipeline

Every message source shares the first two stages:

1. The workspace coordinator normalizes the message, conversation, thread relationship, delivery
   state, and current preference snapshot.
2. The pure `AttentionPolicy` returns independent `record_unread`, `send_notification`, and
   structured reason values.

Realtime messages then add delivery-specific stages:

1. Persistence performs a pure policy preview and atomically records the message observation and,
   when requested, a notification claim in SQLite.
2. The coordinator applies the committed message again under the latest live preferences before
   emitting a native-notification candidate. The persistence preview is deliberately not an
   observable decision.
3. GTK revalidates current preferences, mute state, and the active conversation before delivery.
   When display names are unavailable, it defers the candidate for resolution and repeats those
   checks before sending it to GNOME.

Cache and Web API history use the shared classifier to build local attention, but do not claim or
send realtime notifications. This keeps classification policy out of the window and prevents a
preference change during persistence or name resolution from delivering a stale candidate.

## Raw Slack Unread And Local Attention

Slack's aggregate unread snapshot and Conduit's message-level attention projection are separate:

- Raw counts remain available for synchronization and bootstrap decisions.
- Locally classified messages drive Conduit's unread presentation once that projection exists.
- A larger raw count cannot recreate filtered lifecycle noise or inflate local attention.
- An authoritative raw read state can clear the local projection.
- Each conversation retains its 512 most recently recorded message identities so recent realtime
  redelivery and later history reconciliation do not resurrect unread state. Older identities age
  out of that bounded window; the local read cursor still rejects messages at or before it.

Browser-session workspaces can establish the raw baseline with Slack Web's private bootstrap/counts
flow. OAuth workspaces and bootstrap failures retain the bounded per-conversation fallback. These
raw baselines remain separate when later message reconciliation builds the local projection.

## Ordering And Deduplication

One session-owned actor queue carries realtime messages, user/profile changes, and reactions.
Message and reaction UI fan-out stays ordered behind the actor; user changes also use it for cache
persistence. The callback-facing queue is intentionally unbounded because the Slack transport
callback is synchronous and cannot await capacity without blocking the transport. The actor drains
before reconnect.

Message observation and notification claiming share one SQLite transaction. The 512 most recently
recorded message identities are retained per conversation, while the notification-delivery ledger
retains the 512 most recently claimed identities for the workspace. Within those bounded windows,
`already_observed` means the message identity was retained and cannot add unread state or claim
another notification. `at_or_before_read_cursor` means its timestamp is not newer than the durable
local read cursor and likewise cannot restore attention. Older identities can age out, so this is
bounded replay protection rather than permanent global deduplication.

## Diagnostics And Privacy

Each runtime session keeps counters for committed decisions, unread decisions, notification
candidates, origin, delivery state, stable reason categories, attention-ledger outcomes,
notification claims, and queue depth/high-water values. Ledger outcomes such as `accepted`,
`already_observed`, and `at_or_before_read_cursor` describe message observation and notification
claiming; they are not general success counters for history, user/profile, or reaction persistence.

Enable only these traces with:

```sh
RUST_LOG=conduit::attention=trace conduit
```

The `conduit::attention` target contains only numeric values, booleans, and constant category codes
such as `direct_message`, `keyword_or_phrase`, and `already_observed`. It never includes message
text, configured names or terms, workspace/user/conversation/message identifiers, match offsets, or
preference snapshots. This privacy statement is target-specific; general `--debug` output can
contain private workspace metadata.

While the queue is live, `attention_queue_high_water` is emitted when depth first reaches 1 and
each new power-of-two peak. After a transport run ends and the actor drains,
`attention_metrics_snapshot` reports current depth and the cumulative runtime-session counters and
peak. A normal post-drain depth of zero does not reset the peak, so a later snapshot cannot be read
as measurements for only the most recent reconnect cycle. Counters are in memory only. A
notification-candidate or claim count does not mean GNOME displayed it; GTK and the desktop
notification service remain downstream.

## Reproducible Burst Measurement

Run the two ignored release-mode measurements with a sanitized environment:

```sh
conduit_user_home=/home/you
env -i HOME="${conduit_user_home}" \
  PATH="${conduit_user_home}/.cargo/bin:/usr/local/bin:/usr/bin:/bin" \
  CARGO_HOME="${conduit_user_home}/.cargo" \
  RUSTUP_HOME="${conduit_user_home}/.rustup" \
  LANG=C.UTF-8 CI=true RUSTFLAGS=-Dwarnings \
  cargo test --release --locked realtime_attention_ -- \
  --ignored --nocapture --test-threads=1
```

The classifier workload makes 10,000 decisions per iteration and reports the median of five
measured iterations after warm-up. The ordered-actor workload contains 1,200 ingress events: 1,000
unique identities plus 200 exact direct-message redeliveries. Its strict semantic assertions are:

- 1,000 accepted observations and 200 `already_observed` outcomes.
- 900 local-unread decisions, 500 notification claims, and 500 native-notification candidates.
  The harness does not run GTK or assert that GNOME displayed them.
- Queue depth returns to zero. The reported peak is the observed live high-water value and may vary
  with scheduling between 1 and the 1,200-event ingress size.
- History and thread snapshots reconcile all 1,000 unique identities in the in-memory coordinator
  without adding unread state. Reopening SQLite still yields durable local counts of 700 for the
  channel and 200 for the direct message, while a synthetic raw Slack count remains separate.

Timing is informational because hardware and storage vary. Record a range from repeated runs
together with the commit, Rust version, architecture, CPU, operating system, and temporary-storage
type. The semantic counts, queue drain, bounded replay behavior, and reconciliation results are
strict test assertions.

Two consecutive Phase 4 runs on 2026-07-23 provide an illustrative reference range:

- Linux 7.1.3-native-xanmod1 on x86_64, 13th Gen Intel Core i7-1355U, rustc 1.95.0,
  with `/tmp` on tmpfs.
- Classifier median: 7,212,485–7,741,062 ns per 10,000-decision batch, or 721–774 ns
  per decision.
- Ordered actor: 3,190–4,019 ms to enqueue and drain 1,200 events, with observed queue
  peaks of 1,196–1,198 and a final depth of zero.

These values are a host-specific regression reference, not a performance guarantee.
