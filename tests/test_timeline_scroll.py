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

    await waitForBottom(true);
    const initialGap = bottomGap();

    document.querySelector(".timeline").style.width = "260px";
    await waitForBottom(false);
    const reflowGap = bottomGap();

    await waitForBottom(true);
    document.getElementById("delayed").style.height = "700px";
    await waitForBottom(false);
    const delayedExpansionGap = bottomGap();

    const anchor = document.querySelector('[data-message-ts="10"]');
    anchor.scrollIntoView({ block: "start" });
    await wait(80);
    const anchorTop = anchor.getBoundingClientRect().top;
    const replaced = window.conduitApplyTimelinePatch({
      type: "replace-message",
      message_ts: "10",
      html: '<article class="message" data-message-ts="10" style="height:240px">replacement</article>',
      part_html: ""
    });
    await wait(100);
    const replacementTop = document
      .querySelector('[data-message-ts="10"]')
      .getBoundingClientRect().top;

    window.timelineScrollResult = {
      initialGap,
      reflowGap,
      delayedExpansionGap,
      replaced,
      anchorDelta: replacementTop - anchorTop
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
#delayed {{ height: 20px; }}
</style></head><body>
<main class="timeline"><ol class="message-list">{messages}</ol>
<div id="delayed"></div></main>
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
    assert abs(payload["initialGap"]) <= 2, payload
    assert abs(payload["reflowGap"]) <= 2, payload
    assert abs(payload["delayedExpansionGap"]) <= 2, payload
    assert payload["replaced"] is True, payload
    assert abs(payload["anchorDelta"]) <= 2, payload


if __name__ == "__main__":
    main()
