#!/usr/bin/python3
"""Exercise the bounded reaction picker in the production WebKit engine."""

from __future__ import annotations

import json
import os
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
  let stage = "setup";
  (async () => {
    const wait = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));
    const waitFor = async (predicate) => {
      for (let attempt = 0; attempt < 80; attempt += 1) {
        if (predicate()) return;
        await wait(25);
      }
      throw new Error("Timed out waiting for picker state");
    };
    const picker = document.getElementById("emoji-picker");
    const opener = document.getElementById("opener");
    const search = document.getElementById("emoji-search");
    const overflow = document.getElementById("overflow");
    const overflowSummary = overflow.querySelector("summary");
    const overflowMenu = overflow.querySelector(".more-actions-menu");
    overflow.open = true;
    overflowSummary.dispatchEvent(new MouseEvent("mouseout", {
      bubbles: true,
      relatedTarget: overflowMenu
    }));
    const overflowStaysOpenInside = overflow.open;
    overflowSummary.dispatchEvent(new MouseEvent("mouseout", {
      bubbles: true,
      relatedTarget: document.body
    }));
    const overflowClosesAfterHover = !overflow.open;
    stage = "initial open";
    window.scrollTo(0, 420);
    const initialScroll = window.scrollY;

    const initialOpenStarted = performance.now();
    opener.click();
    await waitFor(() => document.querySelectorAll(".emoji-choice").length === 64);
    const initialOpenMs = performance.now() - initialOpenStarted;
    const initialChoices = Array.from(document.querySelectorAll(".emoji-choice"));
    const staleDiscarded = !initialChoices.some((choice) => choice.dataset.emojiName === "stale");
    const bounded = initialChoices.length === 64;
    const pageControlsVisible =
      !document.querySelector(".emoji-page-controls").hidden;

    document.querySelector("[data-emoji-next]").click();
    stage = "next page";
    await waitFor(() => {
      const choices = document.querySelectorAll(".emoji-choice");
      return choices.length === 6 && choices[0].dataset.emojiName === "smiley_064";
    });
    const pageForward = document.querySelector(".emoji-page-status").textContent === "65-70 / 70";
    document.querySelector("[data-emoji-previous]").click();
    stage = "previous page";
    await waitFor(() => {
      const choices = document.querySelectorAll(".emoji-choice");
      return choices.length === 64 && choices[0].dataset.emojiName === "smiley_000";
    });
    const pageBackward = document.querySelector(".emoji-page-status").textContent === "1-64 / 70";

    search.dispatchEvent(new KeyboardEvent("keydown", {
      key: "ArrowDown",
      bubbles: true,
      cancelable: true
    }));
    const keyboardSelected =
      document.querySelector(".emoji-choice[aria-selected='true']").dataset.emojiName;

    document.querySelector("[data-emoji-category='Workspace']").click();
    stage = "workspace category";
    await waitFor(() => {
      const choice = document.querySelector(".emoji-choice");
      return choice && choice.dataset.emojiName === "workspace_party";
    });
    const customImage = document.querySelector(".emoji-choice img.custom-emoji");
    const customRendered = Boolean(
      customImage && customImage.src.startsWith("http://127.0.0.1:9/")
    );

    search.value = "party parr";
    search.dispatchEvent(new Event("input", { bubbles: true }));
    stage = "search";
    await waitFor(() => {
      const choice = document.querySelector(".emoji-choice");
      return choice && choice.dataset.emojiName === "party_parrot";
    });
    const searchMatchedCustom =
      document.querySelectorAll(".emoji-choice").length === 1;

    document.dispatchEvent(new KeyboardEvent("keydown", {
      key: "Escape",
      bubbles: true,
      cancelable: true
    }));
    await waitFor(() => !picker.open && document.activeElement === opener);
    const focusRestored = document.activeElement === opener;
    const scrollDelta = window.scrollY - initialScroll;

    opener.click();
    stage = "reaction reopen";
    await waitFor(() => document.querySelectorAll(".emoji-choice").length > 0);
    search.dispatchEvent(new KeyboardEvent("keydown", {
      key: "Enter",
      bubbles: true,
      cancelable: true
    }));
    await wait(100);
    const reactionScrollDelta = window.scrollY - initialScroll;

    window.emojiPickerTestResult = {
      staleDiscarded,
      bounded,
      pageControlsVisible,
      pageForward,
      pageBackward,
      keyboardSelected,
      customRendered,
      searchMatchedCustom,
      focusRestored,
      initialOpenMs,
      scrollDelta,
      reactionScrollDelta,
      closedAfterReaction: !picker.open,
      overflowStaysOpenInside,
      overflowClosesAfterHover
    };
  })().catch((error) => {
    window.emojiPickerTestError = stage + ": " + String(error) +
      (error && error.stack ? "\n" + error.stack : "");
  });
  return true;
})()
"""

READ_RESULT = r"""
JSON.stringify({
  result: window.emojiPickerTestResult || null,
  error: window.emojiPickerTestError || null
})
"""


def emoji_entry(index: int) -> dict[str, object]:
    return {
        "name": f"smiley_{index:03}",
        "label": f"Smiley {index}",
        "category": "Smileys",
        "accessible_label": f":smiley_{index:03}: - Smiley {index}",
        "value_kind": "unicode",
        "value": "😀",
    }


def custom_entry(name: str) -> dict[str, object]:
    return {
        "name": name,
        "label": name.replace("_", " "),
        "category": "Workspace",
        "accessible_label": f":{name}: - {name.replace('_', ' ')}",
        "value_kind": "custom-image",
        "value": f"http://127.0.0.1:9/{name}.png",
    }


def main() -> None:
    picker_script = Path(sys.argv[1]).read_text(encoding="utf-8")
    assert "</script" not in picker_script.lower()
    html = f"""<!doctype html>
<html><head><meta charset="utf-8"><style>
body {{ min-height: 1800px; }}
.emoji-grid {{ max-height: 220px; overflow-y: auto; }}
.emoji-page-controls[hidden] {{ display: none; }}
</style></head><body>
<div style="height: 480px"></div>
<button id="opener" data-open-emoji-picker
 data-reaction-template="conduit://reaction?channel=C1&amp;ts=1&amp;name=__REACTION__&amp;add=true">
Add reaction</button>
<article class="message">
  <nav class="quick-actions">
    <details id="overflow" class="more-actions">
      <summary>More actions</summary>
      <div class="more-actions-menu"><a href="#noop">No-op</a></div>
    </details>
  </nav>
</article>
<dialog id="emoji-picker" aria-labelledby="emoji-picker-title"
 data-emoji-protocol-version="1" data-emoji-result-limit="64"
 data-emoji-max-query-chars="128">
  <header><h2 id="emoji-picker-title">Add reaction</h2>
    <button type="button" class="picker-close">Close</button></header>
  <label for="emoji-search">Search</label>
  <input id="emoji-search" role="combobox" aria-controls="emoji-grid">
  <nav class="emoji-categories">
    <button type="button" data-emoji-category="Smileys" aria-selected="true">Smileys</button>
    <button type="button" data-emoji-category="Workspace" aria-selected="false">Workspace</button>
  </nav>
  <div id="emoji-grid" class="emoji-grid" role="grid"></div>
  <p class="emoji-empty" hidden>No emoji found</p>
  <footer class="emoji-page-controls" hidden>
    <button type="button" data-emoji-previous>Previous</button>
    <p class="emoji-page-status"></p>
    <button type="button" data-emoji-next>Next</button>
  </footer>
</dialog>
<script>{picker_script}</script>
</body></html>"""

    Gtk.init()
    loop = GLib.MainLoop()
    manager = WebKit.UserContentManager()
    assert manager.register_script_message_handler("conduitEmojiPicker", None)
    web_view = WebKit.WebView(user_content_manager=manager)
    window = Gtk.Window()
    window.set_default_size(600, 360)
    window.set_child(web_view)
    window.present()
    outcome: dict[str, object] = {}
    requests: list[dict[str, object]] = []
    reaction_uris: list[str] = []

    def fail(error: BaseException) -> None:
        outcome["error"] = error
        loop.quit()

    def deliver(payload: dict[str, object]) -> bool:
        body = "window.conduitReceiveEmojiPickerResult(JSON.parse(payload));"
        arguments = GLib.Variant(
            "a{sv}",
            {"payload": GLib.Variant("s", json.dumps(payload))},
        )
        web_view.call_async_javascript_function(
            body, -1, arguments, None, None, None, None, None
        )
        return GLib.SOURCE_REMOVE

    def result_for(
        request: dict[str, object], entries: list[dict[str, object]], total: int
    ) -> dict[str, object]:
        offset = int(request["offset"])
        return {
            "version": 1,
            "generation": request["generation"],
            "offset": offset,
            "total": total,
            "has_previous": offset > 0,
            "has_more": offset + min(len(entries), 64) < total,
            "entries": entries,
        }

    def on_picker_query(_manager, value) -> None:
        try:
            request = json.loads(value.to_json(0))
            requests.append(request)
            if request["query"] == "party parr":
                payload = result_for(request, [custom_entry("party_parrot")], 1)
                GLib.timeout_add(10, deliver, payload)
            elif request["category"] == "Workspace":
                payload = result_for(request, [custom_entry("workspace_party")], 1)
                GLib.timeout_add(10, deliver, payload)
            else:
                stale = result_for(request, [emoji_entry(0)], 1)
                stale["generation"] = int(request["generation"]) - 1
                stale["entries"][0]["name"] = "stale"
                offset = int(request["offset"])
                valid_entries = (
                    [emoji_entry(index) for index in range(70)]
                    if offset == 0
                    else [emoji_entry(index) for index in range(offset, 70)]
                )
                valid = result_for(request, valid_entries, 70)
                GLib.timeout_add(5, deliver, stale)
                GLib.timeout_add(30, deliver, valid)
        except BaseException as error:
            fail(error)

    def on_decide_policy(
        _view: WebKit.WebView, decision, decision_type: WebKit.PolicyDecisionType
    ) -> bool:
        if decision_type not in (
            WebKit.PolicyDecisionType.NAVIGATION_ACTION,
            WebKit.PolicyDecisionType.NEW_WINDOW_ACTION,
        ):
            return False
        navigation = decision
        uri = navigation.get_navigation_action().get_request().get_uri()
        if uri.startswith("conduit://reaction?"):
            reaction_uris.append(uri)
            decision.ignore()
            return True
        return False

    def on_result(view: WebKit.WebView, result, _data=None) -> None:
        try:
            value = view.evaluate_javascript_finish(result)
            payload = json.loads(value.to_string())
            if payload["error"]:
                raise RuntimeError(f"{payload['error']}; requests={requests}")
            if payload["result"] is None:
                GLib.timeout_add(50, poll_result)
                return
            outcome["payload"] = payload["result"]
            loop.quit()
        except BaseException as error:
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
        GLib.timeout_add(50, poll_result)

    def on_load_changed(view: WebKit.WebView, event: WebKit.LoadEvent) -> None:
        if event == WebKit.LoadEvent.FINISHED:
            view.evaluate_javascript(
                START_PROBE, -1, None, None, None, on_started, None
            )

    def on_timeout() -> bool:
        fail(TimeoutError("WebKit emoji picker test timed out"))
        return GLib.SOURCE_REMOVE

    manager.connect(
        "script-message-received::conduitEmojiPicker", on_picker_query
    )
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
    assert payload["staleDiscarded"] is True, payload
    assert payload["bounded"] is True, payload
    assert payload["pageControlsVisible"] is True, payload
    assert payload["pageForward"] is True, payload
    assert payload["pageBackward"] is True, payload
    assert payload["keyboardSelected"] == "smiley_001", payload
    assert payload["customRendered"] is True, payload
    assert payload["searchMatchedCustom"] is True, payload
    assert payload["focusRestored"] is True, payload
    assert isinstance(payload["initialOpenMs"], (int, float)), payload
    assert payload["initialOpenMs"] > 0, payload
    assert abs(payload["scrollDelta"]) <= 2, payload
    assert abs(payload["reactionScrollDelta"]) <= 2, payload
    assert payload["closedAfterReaction"] is True, payload
    assert payload["overflowStaysOpenInside"] is True, payload
    assert payload["overflowClosesAfterHover"] is True, payload
    assert any(request["category"] == "Workspace" for request in requests), requests
    assert any(request["query"] == "party parr" for request in requests), requests
    assert any(request["offset"] == 64 for request in requests), requests
    assert reaction_uris and "name=workspace_party" in reaction_uris[-1], reaction_uris
    if os.environ.get("CONDUIT_MEASURE_EMOJI_PICKER") == "1":
        print(json.dumps(payload, sort_keys=True))


if __name__ == "__main__":
    main()
