#!/usr/bin/python3
"""Exercise Conduit's timestamp localizer in the production WebKit engine."""

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


FIXED_CLOCK = r"""
const NativeDate = Date;
window.Date = class ConduitTestDate extends NativeDate {
  constructor(...values) {
    super(...(values.length ? values : ["2026-07-20T12:00:00+02:00"]));
  }
  static now() {
    return NativeDate.parse("2026-07-20T12:00:00+02:00");
  }
};
"""

PROBE = r"""
(() => {
  const inserted = window.conduitApplyTimelinePatch({
    type: "insert-message",
    position: "append",
    html: '<li class="message-list-item"><article data-message-ts="dynamic-ts"><time id="dynamic" class="metadata" datetime="2026-07-09T13:00:00+02:00">dynamic fallback</time></article></li>'
  });

  const dateTimeFormatter = new Intl.DateTimeFormat("nl-NL", {
    day: "numeric",
    month: "short",
    hour: "numeric",
    minute: "2-digit"
  });
  const directDate = new Date(2026, 6, 10, 13, 0);
  const direct = dateTimeFormatter.format(directDate);
  const directParts = dateTimeFormatter.formatToParts(directDate);
  const timeFormatter = new Intl.DateTimeFormat("nl-NL", {
    hour: "numeric",
    minute: "2-digit"
  });
  const weekdayTimeFormatter = new Intl.DateTimeFormat("nl-NL", {
    weekday: "long",
    hour: "numeric",
    minute: "2-digit"
  });

  function snapshot(id) {
    const element = document.getElementById(id);
    return {
      text: element.textContent,
      datetime: element.getAttribute("datetime"),
      title: element.title,
      lang: element.lang,
      localized: element.dataset.conduitLocalized || ""
    };
  }

  const dynamic = snapshot("dynamic");
  const replaced = window.conduitApplyTimelinePatch({
    type: "replace-message",
    message_ts: "dynamic-ts",
    html: '<article data-message-ts="dynamic-ts"><time id="replacement" class="metadata" datetime="2026-07-08T13:00:00+02:00">replacement fallback</time></article>',
    part_html: ""
  });

  function isolatedFallback(locale) {
    const frame = document.createElement("iframe");
    document.body.append(frame);
    const frameDocument = frame.contentDocument;
    const localeAttribute = locale ? ' data-time-locale="' + locale + '"' : "";
    frameDocument.open();
    frameDocument.write(
      '<!doctype html><html' + localeAttribute + '><body>' +
      '<time id="fallback" class="metadata" datetime="2026-07-10T13:00:00+02:00">server fallback</time>' +
      '<script>' + window.timestampLocalizationSource + '<\/script></body></html>'
    );
    frameDocument.close();
    const value = frameDocument.getElementById("fallback").textContent;
    frame.remove();
    return value;
  }

  const unsupportedLocale = ["ber-DZ", "agr-PE", "aa-DJ"].find(function (locale) {
    return Intl.DateTimeFormat.supportedLocalesOf([locale], { localeMatcher: "lookup" }).length === 0;
  }) || null;

  return JSON.stringify({
    direct,
    directParts,
    expectedDynamic: dateTimeFormatter.format(new Date(2026, 6, 9, 13, 0)),
    expectedReplacement: dateTimeFormatter.format(new Date(2026, 6, 8, 13, 0)),
    expectedToday: timeFormatter.format(new Date(2026, 6, 20, 13, 0)),
    expectedWeekday: weekdayTimeFormatter.format(new Date(2026, 6, 16, 13, 0)),
    expectedYesterdayWord: new Intl.RelativeTimeFormat("nl-NL", { numeric: "auto" }).format(-1, "day"),
    cLocaleFallback: isolatedFallback(null),
    unsupportedLocale,
    unsupportedLocaleFallback: unsupportedLocale ? isolatedFallback(unsupportedLocale) : null,
    inserted,
    replaced,
    timelineTimestampCount: document.querySelectorAll(".message-list time").length,
    initial: snapshot("initial"),
    dynamic,
    replacement: snapshot("replacement"),
    today: snapshot("today"),
    yesterday: snapshot("yesterday"),
    weekday: snapshot("weekday"),
    previousYear: snapshot("previous-year"),
    invalid: snapshot("invalid")
  });
})()
"""


def main() -> None:
    timestamp_script = Path(sys.argv[1]).read_text(encoding="utf-8")
    timeline_script = Path(sys.argv[2]).read_text(encoding="utf-8")
    assert "</script" not in timestamp_script.lower()
    assert "</script" not in timeline_script.lower()
    script_json = json.dumps(timestamp_script)
    html = f"""<!doctype html>
<html lang="en" data-time-locale="nl-NL">
<head><meta charset="utf-8"><script>{FIXED_CLOCK}\nwindow.timestampLocalizationSource = {script_json};</script></head>
<body>
  <ol class="message-list"><li class="message-list-item"><article data-message-ts="initial-ts">
    <time id="initial" class="metadata" datetime="2026-07-10T13:00:00+02:00" title="fallback title">initial fallback</time>
  </article></li></ol>
  <time id="today" class="metadata" datetime="2026-07-20T13:00:00+02:00">today fallback</time>
  <time id="yesterday" class="metadata" datetime="2026-07-19T13:00:00+02:00">yesterday fallback</time>
  <time id="weekday" class="metadata" datetime="2026-07-16T13:00:00+02:00">weekday fallback</time>
  <time id="previous-year" class="metadata" datetime="2025-12-31T13:00:00+01:00">year fallback</time>
  <time id="invalid" class="metadata" datetime="invalid">invalid fallback</time>
  <script>{timestamp_script}</script>
  <script>{timeline_script}</script>
</body>
</html>"""

    Gtk.init()
    loop = GLib.MainLoop()
    window = Gtk.Window()
    web_view = WebKit.WebView()
    window.set_child(web_view)
    window.present()
    outcome: dict[str, object] = {}

    def finish_with_error(error: BaseException) -> None:
        outcome["error"] = error
        loop.quit()

    def on_evaluated(view: WebKit.WebView, result, _data=None) -> None:
        try:
            value = view.evaluate_javascript_finish(result)
            outcome["payload"] = json.loads(value.to_string())
        except BaseException as error:  # GLib exceptions do not inherit predictably.
            finish_with_error(error)
            return
        loop.quit()

    def on_load_changed(view: WebKit.WebView, event: WebKit.LoadEvent) -> None:
        if event == WebKit.LoadEvent.FINISHED:
            view.evaluate_javascript(PROBE, -1, None, None, None, on_evaluated, None)

    def on_timeout() -> bool:
        finish_with_error(TimeoutError("WebKit timestamp localization timed out"))
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

    assert payload["initial"] == {
        "text": payload["direct"],
        "datetime": "2026-07-10T13:00:00+02:00",
        "title": "fallback title",
        "lang": "nl-NL",
        "localized": "true",
    }
    assert payload["inserted"] is True
    assert payload["replaced"] is True
    assert payload["timelineTimestampCount"] == 2
    part_types = [part["type"] for part in payload["directParts"]]
    parts = {part["type"]: part["value"] for part in payload["directParts"]}
    assert parts["day"] == "10"
    assert parts["month"].lower().startswith("jul")
    assert parts["hour"] == "13"
    assert parts["minute"] == "00"
    assert part_types.index("day") < part_types.index("month") < part_types.index("hour")
    assert payload["dynamic"]["text"] == payload["expectedDynamic"]
    assert payload["dynamic"]["localized"] == "true"
    assert payload["replacement"]["text"] == payload["expectedReplacement"]
    assert payload["replacement"]["localized"] == "true"
    assert payload["today"]["text"] == payload["expectedToday"]
    assert payload["expectedYesterdayWord"] in payload["yesterday"]["text"]
    assert payload["weekday"]["text"] == payload["expectedWeekday"]
    assert "2025" in payload["previousYear"]["text"]
    assert payload["cLocaleFallback"] == "server fallback"
    if payload["unsupportedLocale"] is not None:
        assert payload["unsupportedLocaleFallback"] == "server fallback"
    assert payload["invalid"] == {
        "text": "invalid fallback",
        "datetime": "invalid",
        "title": "",
        "lang": "",
        "localized": "",
    }


if __name__ == "__main__":
    main()
