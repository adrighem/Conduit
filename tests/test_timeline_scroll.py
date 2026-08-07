#!/usr/bin/python3
"""Exercise timeline scroll anchoring in the production WebKit engine."""

from __future__ import annotations

import base64
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


ANIMATED_GIF = bytes.fromhex(
    "47494638396102000200f00000ff000000000021ff0b4e45545343415045322e30"
    "030100000021f904000a0000002c000000000200020000020284510021f904000a"
    "0000002c0000000002000200800000ff00000002028451003b"
)


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
    document.getElementById("delayed").style.height = "700px";
    document.getElementById("delayed").dispatchEvent(new Event("load"));
    await waitForBottom(false);
    const delayedExpansionGap = bottomGap();

    await waitForBottom(true);
    const sentApplied = window.conduitApplyTimelinePatch({
      type: "insert-message",
      position: "append",
      message_ts: "22",
      arrival: "sent",
      html: '<li class="message-list-item"><article class="message" data-message-ts="22">Sent message</article></li>'
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
      html: '<li class="message-list-item"><article class="message" data-message-ts="23">Incoming message</article></li>'
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
      html: '<li class="message-list-item"><article class="message" data-message-ts="24">Reduced motion</article></li>'
    });
    const reducedMotionArrivalClass = document
      .querySelector('[data-message-ts="24"]')
      .classList.contains("sent-message-arrival");
    window.matchMedia = originalMatchMedia;

    await nextFrame();
    await nextFrame();
    await nextFrame();
    const anchor = document.querySelector('[data-message-ts="10"]');
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
      html: '<li class="message-list-item"><article class="message" data-message-ts="25">Sent while reading</article></li>'
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
      return '<li class="message-list-item"><article class="message" data-message-ts="' + index +
        '" style="min-height:' + (80 + index % 3 * 20) + 'px">Snapshot ' + index +
        ' with changed wrapping and dimensions.</article></li>';
    }).join("");
    const snapshotApplied = window.conduitApplyTimelineDelta([{
      type: "replace-snapshot",
      list_html: snapshotHtml,
      load_more_html: '<nav class="timeline-action"><a href="conduit://load-older">Older</a></nav>'
    }, {
      type: "replace-message",
      message_ts: "10",
      html: '<article class="message" data-message-ts="10" style="min-height:100px">Snapshot 10 batched replacement.</article>',
      part_html: ""
    }]);
    await wait(100);
    const snapshotAnchor = document.querySelector('[data-message-ts="10"]');
    const snapshotAnchorDelta = snapshotAnchor.getBoundingClientRect().top - snapshotAnchorTop;
    const snapshotLoadMore = document.querySelector(".timeline-action").textContent;

    const gifApplied = window.conduitApplyTimelinePatch({
      type: "update-image",
      asset_key: "animated-gif",
      source: {
        uri: "conduit-asset://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        kind: "image"
      }
    });
    const animatedGif = document.querySelector('img[data-image-key="animated-gif"]');
    animatedGif.loading = "eager";
    animatedGif.src = window.animatedGifSource;
    animatedGif.scrollIntoView({ block: "center" });
    for (let attempt = 0; attempt < 40 && !animatedGif.complete; attempt += 1) {
      await wait(25);
    }

    const readSnapshotHtml = Array.from({ length: 12 }, (_, offset) => {
      const timestamp = String(1710000001 + offset) + ".000001";
      return '<li class="message-list-item"><article class="message" data-message-ts="' +
        timestamp + '">Read marker ' + timestamp + '</article></li>';
    }).join("");
    const readSnapshotApplied = window.conduitApplyTimelinePatch({
      type: "replace-snapshot",
      list_html: readSnapshotHtml,
      load_more_html: ""
    });
    await nextFrame();
    await nextFrame();
    await nextFrame();
    const readTarget = document.querySelector('[data-message-ts="1710000006.000001"]');
    if (!readTarget) throw new Error("read target missing after snapshot");
    const unreadSeparator = document.createElement("li");
    unreadSeparator.className = "unread-separator";
    unreadSeparator.textContent = "New";
    readTarget.closest(".message-list-item").before(unreadSeparator);
    const readConfigured = window.conduitApplyTimelinePatch({
      type: "configure-read-state",
      read_marker_url: "conduit://mark-read?channel=C1&ts=0&probe=early",
      first_unread_ts: "1710000006.000001"
    });
    readTarget.scrollIntoView({ block: "center" });
    await wait(250);
    document.querySelector('[data-message-ts="1710000001.000001"]')
      .scrollIntoView({ block: "center" });
    await wait(350);
    const stableReadConfigured = window.conduitApplyTimelinePatch({
      type: "configure-read-state",
      read_marker_url: "conduit://mark-read?channel=C1&ts=0&probe=stable",
      first_unread_ts: "1710000006.000001"
    });
    await nextFrame();
    await nextFrame();
    await nextFrame();
    readTarget.scrollIntoView({ block: "center" });
    await wait(600);
    const separatorAfterRead = document.querySelector(".unread-separator");
    const separatorParent = separatorAfterRead ? separatorAfterRead.parentElement.tagName : "";
    const appendedReadApplied = window.conduitApplyTimelinePatch({
      type: "insert-message",
      position: "append",
      message_ts: "1710000013.000001",
      html: '<li class="message-list-item"><article class="message" data-message-ts="1710000013.000001">Later unread</article></li>'
    });
    await nextFrame();
    await nextFrame();
    await nextFrame();
    const appendedRead = document.querySelector('[data-message-ts="1710000013.000001"]');
    if (!appendedRead) throw new Error("appended read target missing");
    appendedRead.scrollIntoView({ block: "center" });
    await wait(600);
    await wait(600);
    const readDisabled = window.conduitApplyTimelinePatch({
      type: "configure-read-state",
      read_marker_url: null,
      first_unread_ts: null
    });
    const disabledAppendApplied = window.conduitApplyTimelinePatch({
      type: "insert-message",
      position: "append",
      message_ts: "1710000014.000001",
      html: '<li class="message-list-item"><article class="message" data-message-ts="1710000014.000001">Disabled unread</article></li>'
    });
    await nextFrame();
    await nextFrame();
    await nextFrame();
    const disabledRead = document.querySelector('[data-message-ts="1710000014.000001"]');
    if (!disabledRead) throw new Error("disabled read target missing");
    disabledRead.scrollIntoView({ block: "center" });
    await wait(600);
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
      snapshotLoadMore,
      gifApplied,
      gifElement: animatedGif.tagName,
      gifNaturalWidth: animatedGif.naturalWidth,
      gifSourceIsGif: animatedGif.currentSrc.startsWith("data:image/gif;base64,"),
      readSnapshotApplied,
      readConfigured,
      stableReadConfigured,
      separatorParent,
      appendedReadApplied,
      readDisabled,
      disabledAppendApplied
    };
  })().catch((error) => {
    window.timelineScrollError = String(error) + "\n" +
      String(error && error.stack ? error.stack : "");
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
    messages = "".join(
        f'<li class="message-list-item"><article class="message" data-message-ts="{index}">'
        f'Message {index} with enough wrapping text to exercise a narrower timeline. '
        f'This content deliberately spans several words and lines.</article></li>'
        for index in range(1, 22)
    )
    animated_gif_source = "data:image/gif;base64," + base64.b64encode(
        ANIMATED_GIF
    ).decode("ascii")
    html = f"""<!doctype html>
<html><head><meta charset="utf-8"><style>
html, body {{ margin: 0; padding: 0; }}
.timeline {{ box-sizing: border-box; width: 580px; }}
.message-list {{ list-style: none; margin: 0; padding: 0; }}
.message {{ box-sizing: border-box; display: block; min-height: 90px; padding: 12px; }}
#delayed {{ height: 20px; }}
</style></head><body>
<main class="timeline" data-timeline-positioning="pending"
 data-timeline-mode="preserve" data-focus-message-ts="10"
 data-timeline-sticky-key="test:sticky" data-timeline-anchor-key="test:anchor"><ol class="message-list">{messages}</ol>
<img id="delayed" alt=""></main>
<div data-image-key="animated-gif" data-image-alt="Animated GIF"
 data-image-unavailable="GIF unavailable">Loading GIF</div>
<script>window.animatedGifSource = {json.dumps(animated_gif_source)};</script>
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
    read_mark_uris: list[str] = []

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

    def on_decide_policy(
        _view: WebKit.WebView, decision, decision_type: WebKit.PolicyDecisionType
    ) -> bool:
        if decision_type not in (
            WebKit.PolicyDecisionType.NAVIGATION_ACTION,
            WebKit.PolicyDecisionType.NEW_WINDOW_ACTION,
        ):
            return False
        uri = decision.get_navigation_action().get_request().get_uri()
        if uri.startswith("conduit://mark-read?"):
            read_mark_uris.append(uri)
            decision.ignore()
            return True
        return False

    def on_timeout() -> bool:
        fail(TimeoutError("WebKit timeline scroll test timed out"))
        return GLib.SOURCE_REMOVE

    web_view.connect("decide-policy", on_decide_policy)
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
    assert payload["snapshotText"] == "Snapshot 10 batched replacement.", payload
    assert abs(payload["snapshotAnchorDelta"]) <= 2, payload
    assert payload["snapshotLoadMore"] == "Older", payload
    assert payload["gifApplied"] is True, payload
    assert payload["gifElement"] == "IMG", payload
    assert payload["gifNaturalWidth"] == 2, payload
    assert payload["gifSourceIsGif"] is True, payload
    assert payload["readSnapshotApplied"] is True, payload
    assert payload["readConfigured"] is True, payload
    assert payload["stableReadConfigured"] is True, payload
    assert payload["separatorParent"] == "OL", payload
    assert payload["appendedReadApplied"] is True, payload
    assert payload["readDisabled"] is True, payload
    assert payload["disabledAppendApplied"] is True, payload
    assert len(read_mark_uris) == 2, read_mark_uris
    assert all("probe=early" not in uri for uri in read_mark_uris), read_mark_uris
    assert all("probe=stable" in uri for uri in read_mark_uris), read_mark_uris
    assert "ts=1710000013.000001" in read_mark_uris[-1], read_mark_uris


if __name__ == "__main__":
    main()
