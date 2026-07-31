#!/usr/bin/python3
"""Exercise timeline scroll anchoring in the production WebKit engine."""

from __future__ import annotations

import json
from pathlib import Path
import sys

try:
    import gi

    gi.require_version("Gtk", "4.0")
    gi.require_version("WebKit", "6.0")
    from gi.repository import GLib, Gtk, WebKit
except (ImportError, ValueError) as error:
    print(f"SKIP: WebKit GTK introspection is unavailable: {error}")
    raise SystemExit(77)


START_PROBE = r"""
(() => {
  (async () => {
    const wait = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));
    const nextFrame = () => new Promise((resolve) => requestAnimationFrame(resolve));
    const root = document.scrollingElement || document.documentElement;
    const bottomGap = () => root.scrollHeight - root.scrollTop - root.clientHeight;
    const waitForBottom = async (force) => {
      for (let attempt = 0; attempt < 40; attempt += 1) {
        if (force) {
          root.scrollTop = root.scrollHeight;
          await nextFrame();
          await nextFrame();
          window.dispatchEvent(new Event("scroll"));
        }
        await nextFrame();
        if (Math.abs(bottomGap()) <= 2) {
          await nextFrame();
          return;
        }
      }
    };

    await nextFrame();
    await nextFrame();
    const initialTarget = document.querySelector('[data-message-ts="10"]');
    const initialFocusDelta = initialTarget.getBoundingClientRect().top +
      initialTarget.getBoundingClientRect().height / 2 - window.innerHeight / 2;
    const initialPending = document.querySelector(".timeline")
      .hasAttribute("data-timeline-positioning");

    await waitForBottom(true);
    const initialGap = bottomGap();

    document.querySelector(".timeline").style.width = "260px";
    window.dispatchEvent(new Event("resize"));
    await waitForBottom(false);
    const reflowGap = bottomGap();

    await waitForBottom(true);
    const initialDelayedImage = document.getElementById("delayed");
    initialDelayedImage.style.height = "700px";
    initialDelayedImage.dispatchEvent(new Event("load"));
    await waitForBottom(false);
    const delayedExpansionGap = bottomGap();

    await waitForBottom(true);
    const sentApplied = window.conduitApplyTimelinePatch({
      type: "insert-message",
      position: "append",
      message_ts: "22",
      arrival: "sent",
      html: '<li><article class="message" data-message-ts="22">Sent message</article></li>'
    });
    const sentMessage = document.querySelector('[data-message-ts="22"]');
    const sentArrivalClass = sentMessage.classList.contains("sent-message-arrival");
    await nextFrame();
    await nextFrame();
    const sentAppendGap = bottomGap();
    sentMessage.dispatchEvent(new Event("animationend"));
    const sentArrivalCleared = !sentMessage.classList.contains("sent-message-arrival");

    const sentReplacementApplied = window.conduitApplyTimelinePatch({
      type: "replace-message",
      message_ts: "22",
      arrival: "sent",
      html: '<article class="message" data-message-ts="22">Sent replacement</article>',
      part_html: ""
    });
    const sentReplacement = document.querySelector('[data-message-ts="22"]');
    const sentReplacementArrivalClass =
      sentReplacement.classList.contains("sent-message-arrival");
    sentReplacement.dispatchEvent(new Event("animationend"));

    const incomingApplied = window.conduitApplyTimelinePatch({
      type: "insert-message",
      position: "append",
      message_ts: "23",
      html: '<li><article class="message" data-message-ts="23">Incoming message</article></li>'
    });
    const incomingMessage = document.querySelector('[data-message-ts="23"]');
    const incomingArrivalClass =
      incomingMessage.classList.contains("sent-message-arrival");

    const originalMatchMedia = window.matchMedia;
    window.matchMedia = () => ({ matches: true });
    const reducedMotionApplied = window.conduitApplyTimelinePatch({
      type: "insert-message",
      position: "append",
      message_ts: "24",
      arrival: "sent",
      html: '<li><article class="message" data-message-ts="24">Reduced motion</article></li>'
    });
    const reducedMotionArrivalClass = document
      .querySelector('[data-message-ts="24"]')
      .classList.contains("sent-message-arrival");
    window.matchMedia = originalMatchMedia;

    await nextFrame();
    await nextFrame();
    const anchor = document.querySelector('[data-message-ts="10"]');
    window.dispatchEvent(new WheelEvent("wheel", { deltaY: -100 }));
    anchor.scrollIntoView({ block: "start" });
    window.dispatchEvent(new Event("scroll"));
    await nextFrame();
    await nextFrame();
    await wait(80);
    const sentAwayAnchorTop = anchor.getBoundingClientRect().top;
    const sentAwayApplied = window.conduitApplyTimelinePatch({
      type: "insert-message",
      position: "append",
      message_ts: "25",
      arrival: "sent",
      html: '<li><article class="message" data-message-ts="25">Sent while reading</article></li>'
    });
    await wait(100);
    const sentAwayMessage = document.querySelector('[data-message-ts="25"]');
    const sentAwayArrivalClass =
      sentAwayMessage.classList.contains("sent-message-arrival");
    const sentAwayAnchorDelta =
      anchor.getBoundingClientRect().top - sentAwayAnchorTop;

    const anchorTop = anchor.getBoundingClientRect().top;
    const replaced = window.conduitApplyTimelinePatch({
      type: "replace-message",
      message_ts: "10",
      html: '<article class="message" data-message-ts="10" style="height:240px">replacement</article>',
      part_html: ""
    });
    await wait(100);
    const replacement = document.querySelector('[data-message-ts="10"]');
    const replacementTop = replacement.getBoundingClientRect().top;

    replacement.scrollIntoView({ block: "start" });
    await wait(80);
    const snapshotAnchorTop = replacement.getBoundingClientRect().top;
    const snapshotHtml = Array.from({ length: 24 }, (_, offset) => {
      const index = offset;
      const delayedMedia = index === 2
        ? '<div data-image-key="anchor-image" data-image-alt="Image" ' +
          'data-image-unavailable="Unavailable" style="height:20px"></div>' +
          '<div data-image-key="anchor-video" data-image-alt="Video" ' +
          'data-image-unavailable="Unavailable" style="height:20px"></div>'
        : '';
      return '<li><article class="message" data-message-ts="' + index +
        '" style="min-height:' + (80 + index % 3 * 20) + 'px">Snapshot ' + index +
        ' with changed wrapping and dimensions.' + delayedMedia + '</article></li>';
    }).join("");
    const snapshotApplied = window.conduitApplyTimelinePatch({
      type: "replace-snapshot",
      list_html: snapshotHtml,
      load_more_html: '<nav class="timeline-action"><a href="conduit://load-older">Older</a></nav>'
    });
    await wait(100);
    const snapshotAnchor = document.querySelector('[data-message-ts="10"]');
    const snapshotAnchorDelta = snapshotAnchor.getBoundingClientRect().top - snapshotAnchorTop;

    snapshotAnchor.scrollIntoView({ block: "start" });
    window.dispatchEvent(new Event("scroll"));
    await nextFrame();
    await nextFrame();
    const deltaAnchorTop = snapshotAnchor.getBoundingClientRect().top;
    const stateBeforeDelta = window.conduitTimelineState();
    const batchResult = window.conduitApplyTimelineDelta({
      id: 1,
      document_generation: 7,
      base_timeline_revision: 40,
      timeline_revision: 41,
      operations: [
        {
          type: "insert-message",
          position: "append",
          message_ts: "batch-1",
          html: '<li class="message-list-item"><article class="message" data-message-ts="batch-1" ' +
            'data-author-user-id="U-BATCH"><header class="message-header">' +
            '<span class="author-actions"><span class="author-label">Old name</span></span>' +
            '</header><span data-mention-user-id="U-BATCH">@old</span>' +
            '<span data-message-region="responses">old response</span>' +
            'Batch message</article></li>'
        },
        {
          type: "replace-message",
          message_ts: "batch-1",
          html: '<article class="message" data-message-ts="batch-1" ' +
            'data-author-user-id="U-BATCH"><header class="message-header">' +
            '<span class="author-actions"><span class="author-label">Old name</span></span>' +
            '</header><span data-mention-user-id="U-BATCH">@old</span>' +
            '<span data-message-region="responses">old response</span>' +
            'Batch edited message</article>',
          part_html: ""
        },
        {
          type: "insert-message",
          position: "append",
          message_ts: "batch-1",
          html: '<li><article class="message" data-message-ts="batch-1">duplicate</article></li>'
        },
        {
          type: "insert-message",
          position: "append",
          message_ts: "batch-delete",
          html: '<li class="message-list-item"><article class="message" ' +
            'data-message-ts="batch-delete">remove me</article></li>'
        },
        {
          type: "remove-message",
          message_ts: "batch-delete"
        },
        {
          type: "update-user",
          user_id: "U-BATCH",
          name: "Batch User",
          status_html: '<span class="user-status">Busy</span>'
        },
        {
          type: "replace-region",
          message_ts: "batch-1",
          region: "responses",
          html: "new response"
        },
        {
          type: "update-image",
          asset_key: "anchor-image",
          source: {
            uri: "conduit-asset://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            kind: "image"
          }
        },
        {
          type: "update-image",
          asset_key: "anchor-video",
          source: {
            uri: "conduit-asset://bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            kind: "video"
          }
        },
        {
          type: "update-image",
          asset_key: "missing-image",
          source: null
        },
        {
          type: "update-user",
          user_id: "U-MISSING",
          name: "Missing User",
          status_html: ""
        }
      ]
    });
    const stateAfterDelta = window.conduitTimelineState();
    await nextFrame();
    await nextFrame();
    const batchAnchorDelta = snapshotAnchor.getBoundingClientRect().top - deltaAnchorTop;
    const batchMessages = document.querySelectorAll('[data-message-ts="batch-1"]');
    const batchMessage = batchMessages[0];

    const mismatchHtml = document.querySelector(".message-list").innerHTML;
    const baseMismatchResult = window.conduitApplyTimelineDelta({
      id: 2,
      document_generation: 7,
      base_timeline_revision: 40,
      timeline_revision: 42,
      operations: [{
        type: "insert-message",
        position: "append",
        message_ts: "must-not-insert-base",
        html: '<li><article class="message" data-message-ts="must-not-insert-base">bad</article></li>'
      }]
    });
    const generationMismatchResult = window.conduitApplyTimelineDelta({
      id: 3,
      document_generation: 8,
      base_timeline_revision: 41,
      timeline_revision: 42,
      operations: [{
        type: "insert-message",
        position: "append",
        message_ts: "must-not-insert-generation",
        html: '<li><article class="message" data-message-ts="must-not-insert-generation">bad</article></li>'
      }]
    });
    const skippedRevisionResult = window.conduitApplyTimelineDelta({
      id: 4,
      document_generation: 7,
      base_timeline_revision: 41,
      timeline_revision: 43,
      operations: []
    });
    const mismatchDidNotMutate =
      document.querySelector(".message-list").innerHTML === mismatchHtml;

    const missingEnrichmentResult = window.conduitApplyTimelineDelta({
      id: 5,
      document_generation: 7,
      base_timeline_revision: 41,
      timeline_revision: 42,
      operations: [
        {
          type: "update-image",
          asset_key: "still-missing",
          source: null
        },
        {
          type: "update-user",
          user_id: "STILL-MISSING",
          name: "Nobody",
          status_html: ""
        }
      ]
    });
    const corruptResult = window.conduitApplyTimelineDelta({
      id: 6,
      document_generation: 7,
      base_timeline_revision: 42,
      timeline_revision: 43,
      operations: [{
        type: "replace-message",
        message_ts: "missing-structural-target",
        html: '<article class="message" data-message-ts="missing-structural-target">bad</article>',
        part_html: ""
      }]
    });
    const revisionAfterCorrupt = window.conduitTimelineState().timeline_revision;
    const idempotentResult = window.conduitApplyTimelineDelta({
      id: 7,
      document_generation: 7,
      base_timeline_revision: 42,
      timeline_revision: 43,
      operations: [{
        type: "insert-message",
        position: "append",
        message_ts: "batch-1",
        html: '<li><article class="message" data-message-ts="batch-1">replacement duplicate</article></li>'
      }]
    });

    const delayedImage = document.querySelector('[data-image-key="anchor-image"]');
    const delayedVideo = document.querySelector('[data-image-key="anchor-video"]');
    const mediaAnchorTop = snapshotAnchor.getBoundingClientRect().top;
    delayedImage.style.height = "180px";
    delayedImage.dispatchEvent(new Event("load"));
    await nextFrame();
    await nextFrame();
    await wait(80);
    const delayedImageAnchorDelta =
      snapshotAnchor.getBoundingClientRect().top - mediaAnchorTop;
    const videoAnchorTop = snapshotAnchor.getBoundingClientRect().top;
    delayedVideo.style.height = "160px";
    delayedVideo.dispatchEvent(new Event("loadedmetadata"));
    await nextFrame();
    await nextFrame();
    await wait(80);
    const delayedVideoAnchorDelta =
      snapshotAnchor.getBoundingClientRect().top - videoAnchorTop;

    const failedMediaAnchorTop = snapshotAnchor.getBoundingClientRect().top;
    delayedImage.style.height = "210px";
    delayedImage.dispatchEvent(new Event("error"));
    await nextFrame();
    await nextFrame();
    await wait(80);
    const failedMediaAnchorDelta =
      snapshotAnchor.getBoundingClientRect().top - failedMediaAnchorTop;

    await waitForBottom(true);
    delayedImage.style.height = "240px";
    delayedImage.dispatchEvent(new Event("load"));
    delayedVideo.style.height = "220px";
    delayedVideo.dispatchEvent(new Event("loadedmetadata"));
    await nextFrame();
    await nextFrame();
    await wait(80);
    const delayedMediaBottomGap = bottomGap();

    delayedImage.style.height = "280px";
    delayedImage.dispatchEvent(new Event("error"));
    delayedVideo.style.height = "260px";
    delayedVideo.dispatchEvent(new Event("error"));
    await nextFrame();
    await nextFrame();
    await wait(80);
    const failedMediaBottomGap = bottomGap();

    snapshotAnchor.scrollIntoView({ block: "start" });
    window.dispatchEvent(new Event("scroll"));
    await nextFrame();
    await nextFrame();
    const cancellationResult = window.conduitApplyTimelineDelta({
      id: 8,
      document_generation: 7,
      base_timeline_revision: 43,
      timeline_revision: 44,
      operations: [{
        type: "replace-message",
        message_ts: "2",
        html: '<article class="message" data-message-ts="2" style="height:520px">resized above viewport</article>',
        part_html: ""
      }]
    });
    root.scrollTop -= 137;
    const immediateUserScrollTop = root.scrollTop;
    window.dispatchEvent(new WheelEvent("wheel", { deltaY: -137 }));
    window.dispatchEvent(new Event("scroll"));
    await nextFrame();
    await nextFrame();
    await wait(100);
    const cancelledRestoreDelta = root.scrollTop - immediateUserScrollTop;
    const compatibilityNoopApplied = window.conduitApplyTimelinePatch({
      type: "update-user",
      user_id: "COMPAT-MISSING",
      name: "Nobody",
      status_html: ""
    });

    window.timelineScrollResult = {
      initialFocusDelta,
      initialPending,
      initialGap,
      reflowGap,
      delayedExpansionGap,
      sentApplied,
      sentArrivalClass,
      sentAppendGap,
      sentArrivalCleared,
      sentReplacementApplied,
      sentReplacementArrivalClass,
      incomingApplied,
      incomingArrivalClass,
      reducedMotionApplied,
      reducedMotionArrivalClass,
      sentAwayApplied,
      sentAwayArrivalClass,
      sentAwayAnchorDelta,
      replaced,
      replacementText: replacement.textContent,
      replacementInlineHeight: replacement.style.height,
      anchorDelta: replacementTop - anchorTop,
      snapshotApplied,
      snapshotText: snapshotAnchor.textContent,
      snapshotAnchorDelta,
      snapshotLoadMore: document.querySelector(".timeline-action").textContent,
      stateBeforeDelta,
      batchResult,
      stateAfterDelta,
      batchAnchorDelta,
      batchMessageCount: batchMessages.length,
      batchDeleted: !document.querySelector('[data-message-ts="batch-delete"]'),
      batchMessageText: batchMessage.textContent,
      batchAuthor: batchMessage.querySelector(".author-label").textContent,
      batchMention: batchMessage.querySelector('[data-mention-user-id="U-BATCH"]').textContent,
      batchStatus: batchMessage.querySelector(".user-status").textContent,
      batchResponse: batchMessage.querySelector('[data-message-region="responses"]').textContent,
      baseMismatchResult,
      generationMismatchResult,
      skippedRevisionResult,
      mismatchDidNotMutate,
      missingEnrichmentResult,
      corruptResult,
      revisionAfterCorrupt,
      idempotentResult,
      idempotentMessageCount: document.querySelectorAll('[data-message-ts="batch-1"]').length,
      idempotentMessageText: document.querySelector('[data-message-ts="batch-1"]').textContent,
      delayedImageTag: delayedImage.tagName,
      delayedVideoTag: delayedVideo.tagName,
      delayedImageAnchorDelta,
      delayedVideoAnchorDelta,
      failedMediaAnchorDelta,
      delayedMediaBottomGap,
      failedMediaBottomGap,
      cancellationResult,
      cancelledRestoreDelta,
      compatibilityNoopApplied,
      finalState: window.conduitTimelineState()
    };
  })().catch((error) => {
    window.timelineScrollError = String(error && error.stack ? error.stack : error);
  });
  return true;
})()
"""

READ_RESULT = r"""
JSON.stringify({
  result: window.timelineScrollResult || null,
  error: window.timelineScrollError || null
})
"""


def main() -> None:
    timeline_script = Path(sys.argv[1]).read_text(encoding="utf-8")
    assert "</script" not in timeline_script.lower()
    assert "ResizeObserver" not in timeline_script
    assert 'document.addEventListener("load"' in timeline_script
    assert 'document.addEventListener("loadedmetadata"' in timeline_script
    assert 'document.addEventListener("error"' in timeline_script
    messages = "".join(
        f'<li><article class="message" data-message-ts="{index}">'
        f'Message {index} with enough wrapping text to exercise a narrower timeline. '
        f'This content deliberately spans several words and lines.</article></li>'
        for index in range(1, 22)
    )
    html = f"""<!doctype html>
<html><head><meta charset="utf-8"><style>
html, body {{ margin: 0; padding: 0; }}
.timeline {{ box-sizing: border-box; width: 580px; }}
.message-list {{ list-style: none; margin: 0; padding: 0; }}
.message {{ box-sizing: border-box; display: block; min-height: 90px; padding: 12px; }}
#delayed {{ display: block; height: 20px; }}
</style></head><body>
<main class="timeline" data-timeline-positioning="pending"
 data-timeline-mode="preserve" data-focus-message-ts="10"
 data-timeline-document-generation="7" data-timeline-revision="40"
 data-timeline-sticky-key="test:sticky" data-timeline-anchor-key="test:anchor"><ol class="message-list">{messages}</ol>
<img id="delayed" alt=""></main>
<script>{timeline_script}</script>
</body></html>"""

    Gtk.init()
    loop = GLib.MainLoop()
    window = Gtk.Window()
    window.set_default_size(600, 360)
    web_view = WebKit.WebView()
    window.set_child(web_view)
    window.present()
    outcome: dict[str, object] = {}

    def fail(error: BaseException) -> None:
        outcome["error"] = error
        loop.quit()

    def on_result(view: WebKit.WebView, result, _data=None) -> None:
        try:
            value = view.evaluate_javascript_finish(result)
            payload = json.loads(value.to_string())
            if payload["error"]:
                raise RuntimeError(payload["error"])
            if payload["result"] is None:
                GLib.timeout_add(100, poll_result)
                return
            outcome["payload"] = payload["result"]
            loop.quit()
        except BaseException as error:  # GLib exceptions do not inherit predictably.
            fail(error)

    def poll_result() -> bool:
        web_view.evaluate_javascript(
            READ_RESULT, -1, None, None, None, on_result, None
        )
        return GLib.SOURCE_REMOVE

    def on_started(view: WebKit.WebView, result, _data=None) -> None:
        try:
            view.evaluate_javascript_finish(result)
        except BaseException as error:
            fail(error)
            return
        GLib.timeout_add(100, poll_result)

    def on_load_changed(view: WebKit.WebView, event: WebKit.LoadEvent) -> None:
        if event == WebKit.LoadEvent.FINISHED:
            view.evaluate_javascript(
                START_PROBE, -1, None, None, None, on_started, None
            )

    def on_timeout() -> bool:
        fail(TimeoutError("WebKit timeline scroll test timed out"))
        return GLib.SOURCE_REMOVE

    web_view.connect("load-changed", on_load_changed)
    GLib.timeout_add_seconds(15, on_timeout)
    web_view.load_html(html, "app://conduit/")
    loop.run()
    window.destroy()

    if "error" in outcome:
        raise outcome["error"]  # type: ignore[misc]
    payload = outcome["payload"]
    assert isinstance(payload, dict)
    assert abs(payload["initialFocusDelta"]) <= 2, payload
    assert payload["initialPending"] is False, payload
    assert abs(payload["initialGap"]) <= 2, payload
    assert abs(payload["reflowGap"]) <= 2, payload
    assert abs(payload["delayedExpansionGap"]) <= 2, payload
    assert payload["sentApplied"] is True, payload
    assert payload["sentArrivalClass"] is True, payload
    assert abs(payload["sentAppendGap"]) <= 2, payload
    assert payload["sentArrivalCleared"] is True, payload
    assert payload["sentReplacementApplied"] is True, payload
    assert payload["sentReplacementArrivalClass"] is True, payload
    assert payload["incomingApplied"] is True, payload
    assert payload["incomingArrivalClass"] is False, payload
    assert payload["reducedMotionApplied"] is True, payload
    assert payload["reducedMotionArrivalClass"] is False, payload
    assert payload["sentAwayApplied"] is True, payload
    assert payload["sentAwayArrivalClass"] is False, payload
    assert abs(payload["sentAwayAnchorDelta"]) <= 2, payload
    assert payload["replaced"] is True, payload
    assert payload["replacementText"] == "replacement", payload
    assert payload["replacementInlineHeight"] == "240px", payload
    assert abs(payload["anchorDelta"]) <= 2, payload
    assert payload["snapshotApplied"] is True, payload
    assert payload["snapshotText"] == "Snapshot 10 with changed wrapping and dimensions.", payload
    assert abs(payload["snapshotAnchorDelta"]) <= 2, payload
    assert payload["snapshotLoadMore"] == "Older", payload
    assert payload["stateBeforeDelta"]["document_generation"] == 7, payload
    assert payload["stateBeforeDelta"]["timeline_revision"] == 40, payload
    assert payload["batchResult"] == {
        "status": "applied",
        "timeline_revision": 41,
    }, payload
    assert payload["stateAfterDelta"]["document_generation"] == 7, payload
    assert payload["stateAfterDelta"]["timeline_revision"] == 41, payload
    assert (
        payload["stateAfterDelta"]["preserved_scroll_transactions"]
        - payload["stateBeforeDelta"]["preserved_scroll_transactions"]
        == 1
    ), payload
    assert abs(payload["batchAnchorDelta"]) <= 2, payload
    assert payload["batchMessageCount"] == 1, payload
    assert payload["batchDeleted"] is True, payload
    assert "Batch edited message" in payload["batchMessageText"], payload
    assert "duplicate" not in payload["batchMessageText"], payload
    assert payload["batchAuthor"] == "Batch User", payload
    assert payload["batchMention"] == "@Batch User", payload
    assert payload["batchStatus"] == "Busy", payload
    assert payload["batchResponse"] == "new response", payload
    assert payload["baseMismatchResult"] == {
        "status": "revision-mismatch",
        "timeline_revision": 41,
    }, payload
    assert payload["generationMismatchResult"] == {
        "status": "revision-mismatch",
        "timeline_revision": 41,
    }, payload
    assert payload["skippedRevisionResult"] == {
        "status": "revision-mismatch",
        "timeline_revision": 41,
    }, payload
    assert payload["mismatchDidNotMutate"] is True, payload
    assert payload["missingEnrichmentResult"] == {
        "status": "applied",
        "timeline_revision": 42,
    }, payload
    assert payload["corruptResult"] == {
        "status": "corrupt",
        "timeline_revision": 42,
    }, payload
    assert payload["revisionAfterCorrupt"] == 42, payload
    assert payload["idempotentResult"] == {
        "status": "applied",
        "timeline_revision": 43,
    }, payload
    assert payload["idempotentMessageCount"] == 1, payload
    assert "replacement duplicate" not in payload["idempotentMessageText"], payload
    assert payload["delayedImageTag"] == "IMG", payload
    assert payload["delayedVideoTag"] == "VIDEO", payload
    assert abs(payload["delayedImageAnchorDelta"]) <= 2, payload
    assert abs(payload["delayedVideoAnchorDelta"]) <= 2, payload
    assert abs(payload["failedMediaAnchorDelta"]) <= 2, payload
    assert abs(payload["delayedMediaBottomGap"]) <= 2, payload
    assert abs(payload["failedMediaBottomGap"]) <= 2, payload
    assert payload["cancellationResult"] == {
        "status": "applied",
        "timeline_revision": 44,
    }, payload
    assert abs(payload["cancelledRestoreDelta"]) <= 2, payload
    assert payload["compatibilityNoopApplied"] is True, payload
    assert payload["finalState"]["document_generation"] == 7, payload
    assert payload["finalState"]["timeline_revision"] == 44, payload


if __name__ == "__main__":
    main()
