(function () {
  "use strict";

  const locale = document.documentElement.dataset.timeLocale;
  if (!locale) return;
  const selector = "time.metadata[datetime]";
  const millisecondsPerDay = 86_400_000;
  let timeFormatter;
  let weekdayTimeFormatter;
  let dateTimeFormatter;
  let dateYearTimeFormatter;
  let relativeDayFormatter;
  let resolvedLocale;

  try {
    if (
      Intl.DateTimeFormat.supportedLocalesOf([locale], { localeMatcher: "lookup" }).length === 0
    ) {
      return;
    }
    timeFormatter = new Intl.DateTimeFormat(locale, {
      hour: "numeric",
      minute: "2-digit"
    });
    weekdayTimeFormatter = new Intl.DateTimeFormat(locale, {
      weekday: "long",
      hour: "numeric",
      minute: "2-digit"
    });
    dateTimeFormatter = new Intl.DateTimeFormat(locale, {
      day: "numeric",
      month: "short",
      hour: "numeric",
      minute: "2-digit"
    });
    dateYearTimeFormatter = new Intl.DateTimeFormat(locale, {
      day: "numeric",
      month: "short",
      year: "numeric",
      hour: "numeric",
      minute: "2-digit"
    });
    if (
      typeof Intl.RelativeTimeFormat === "function" &&
      Intl.RelativeTimeFormat.supportedLocalesOf([locale], { localeMatcher: "lookup" }).length > 0
    ) {
      relativeDayFormatter = new Intl.RelativeTimeFormat(locale, { numeric: "auto" });
    }
    resolvedLocale = timeFormatter.resolvedOptions().locale;
  } catch (_) {
    return;
  }

  function localCalendarDay(date) {
    return Math.trunc(Date.UTC(date.getFullYear(), date.getMonth(), date.getDate()) / millisecondsPerDay);
  }

  function relativeDayAndTime(date, daysOld) {
    if (daysOld !== 1) {
      return weekdayTimeFormatter.format(date);
    }
    if (!relativeDayFormatter || typeof weekdayTimeFormatter.formatToParts !== "function") {
      return null;
    }

    const relativeDay = relativeDayFormatter.format(-1, "day");
    let replacedWeekday = false;
    const text = weekdayTimeFormatter.formatToParts(date).map(function (part) {
      if (part.type !== "weekday") return part.value;
      replacedWeekday = true;
      return relativeDay;
    }).join("");
    return replacedWeekday ? text : null;
  }

  function formatTimestamp(date, now) {
    const daysOld = localCalendarDay(now) - localCalendarDay(date);
    if (daysOld === 0) return timeFormatter.format(date);
    if (daysOld >= 1 && daysOld <= 5) return relativeDayAndTime(date, daysOld);

    const includeYear = daysOld >= 183 && date.getFullYear() !== now.getFullYear();
    return (includeYear ? dateYearTimeFormatter : dateTimeFormatter).format(date);
  }

  function timestampElements(root) {
    if (!root || typeof root.querySelectorAll !== "function") return [];
    const elements = Array.from(root.querySelectorAll(selector));
    if (typeof root.matches === "function" && root.matches(selector)) elements.unshift(root);
    return elements;
  }

  function localizeTimestamps(root) {
    const now = new Date();
    timestampElements(root).forEach(function (element) {
      const date = new Date(element.getAttribute("datetime"));
      if (Number.isNaN(date.getTime())) return;
      try {
        const text = formatTimestamp(date, now);
        if (text === null) return;
        element.textContent = text;
        element.lang = resolvedLocale;
        element.dataset.conduitLocalized = "true";
      } catch (_) {
        // Keep the server-rendered fallback if WebKit rejects a date.
      }
    });
  }

  window.conduitLocalizeTimestamps = localizeTimestamps;
  localizeTimestamps(document);
})();
