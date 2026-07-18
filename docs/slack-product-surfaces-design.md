# Slack Product Surfaces Design

## Assessment

Most Slack product surfaces should not be implemented natively in Conduit.

Canvases, lists, workflows, clips, and similar surfaces are large Slack products with their own permissions, collaboration models, editors, realtime behavior, and enterprise policy. A lightweight GNOME desktop client should expose references to those surfaces clearly and open Slack for advanced interaction.

Huddles use a narrower hybrid boundary. Conduit can safely provide supported discovery, presence, preflight, notifications, native media infrastructure, and an exact external handoff. Production participation remains external until both Slack's unpublished bootstrap and the Amazon Chime bridge are verified compatible.

## Current Slice

This slice improves thin affordances for advanced surfaces:

- Block Kit action elements with URLs render as clickable action chips instead of inert labels.
- The existing external link handler opens those URLs in the user's browser or Slack-capable handler.
- Non-URL Block Kit actions remain non-interactive labels because Conduit does not implement Slack interactivity callbacks.

This helps messages that contain workflow buttons, canvas/list links, or other Slack-hosted deep links without adding native editors or callback infrastructure.

## Boundary

Conduit should natively render references and summaries when Slack includes them in messages. It should not natively create, edit, administer, or run advanced Slack product surfaces.

For huddles, Conduit may render supported active-room metadata and maintain a generic, capability-gated media engine. That engine and its synthetic harness are not evidence of Slack huddle interoperability. See [Slack huddles](huddles.md) for the current boundary.

## Deferred

The following remains out of scope unless Slack provides stable lightweight APIs that fit Conduit:

- Native canvas editor.
- Native list/project-management UI.
- Native workflow builder.
- Starting huddles or enabling native production participation without verified Slack and Amazon Chime adapters.
- Slack AI authoring, recaps, or summaries.
- Admin-only Slack Connect or product-surface policy management.

## Tests

The implementation should keep unit coverage for:

- Block Kit action URL rendering.
- Existing non-URL action labels.
- External link safety checks.
- Huddle discovery scoping, capability gating, and exact HTTPS fallback construction.
