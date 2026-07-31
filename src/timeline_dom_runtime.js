(function () {
  function timelineRoot() {
    return document.scrollingElement || document.documentElement;
  }

  function messageElement(messageTs) {
    return Array.from(document.querySelectorAll("[data-message-ts]")).find(function (element) {
      return element.dataset.messageTs === messageTs;
    }) || null;
  }

  function imageElements(assetKey) {
    return Array.from(document.querySelectorAll("[data-image-key]")).filter(function (element) {
      return element.dataset.imageKey === assetKey;
    });
  }

  function authorElements(userId) {
    return Array.from(document.querySelectorAll("[data-author-user-id]")).filter(function (element) {
      return element.dataset.authorUserId === userId;
    });
  }

  function mentionElements(userId) {
    return Array.from(document.querySelectorAll("[data-mention-user-id]")).filter(function (element) {
      return element.dataset.mentionUserId === userId;
    });
  }

  function fragment(html) {
    const template = document.createElement("template");
    template.innerHTML = html;
    if (typeof window.conduitLocalizeTimestamps === "function") {
      window.conduitLocalizeTimestamps(template.content);
    }
    return template.content;
  }

  function messageElementIn(root, messageTs) {
    return Array.from(root.querySelectorAll("[data-message-ts]")).find(function (element) {
      return element.dataset.messageTs === messageTs;
    }) || null;
  }

  function animateSentMessage(root, messageTs, arrivalVisible) {
    if (!arrivalVisible || !root || !messageTs) return;
    if (
      window.matchMedia &&
      window.matchMedia("(prefers-reduced-motion: reduce)").matches
    ) return;
    const target = messageElementIn(root, messageTs);
    if (!target) return;
    target.classList.add("sent-message-arrival");
    let cleanupTimer = 0;
    function clearArrival() {
      target.removeEventListener("animationend", onAnimationFinished);
      target.removeEventListener("animationcancel", onAnimationFinished);
      window.clearTimeout(cleanupTimer);
      target.classList.remove("sent-message-arrival");
    }
    function onAnimationFinished(event) {
      if (event.target === target) clearArrival();
    }
    target.addEventListener("animationend", onAnimationFinished);
    target.addEventListener("animationcancel", onAnimationFinished);
    cleanupTimer = window.setTimeout(clearArrival, 1000);
  }

  function visibleAnchor() {
    const messages = Array.from(document.querySelectorAll("[data-message-ts]"));
    const fullyEntered = messages.find(function (element) {
      const rect = element.getBoundingClientRect();
      return rect.top >= 0 && rect.top <= window.innerHeight;
    });
    return fullyEntered || messages.find(function (element) {
      const rect = element.getBoundingClientRect();
      return rect.bottom > 0 && rect.top <= window.innerHeight;
    }) || null;
  }

  const bottomThreshold = 96;

  function isNearBottom() {
    const root = timelineRoot();
    return root.scrollHeight - root.scrollTop - root.clientHeight <= bottomThreshold;
  }

  let viewportAnchor = null;
  let viewportAnchorTop = 0;
  let restoringViewportAnchor = false;
  let viewportPinnedToBottom = isNearBottom();
  let rememberViewportAnchorFrame = 0;
  let resizeCorrectionFrame = 0;
  let userScrollIntentFrame = 0;
  let userScrollIntentPending = false;
  let scrollMutationGeneration = 0;
  let preservedScrollTransactions = 0;
  const timeline = document.querySelector(".timeline");
  const initialMode = timeline ? timeline.dataset.timelineMode || "preserve" : "preserve";
  const stickyKey = timeline ? timeline.dataset.timelineStickyKey : "";
  const anchorKey = timeline ? timeline.dataset.timelineAnchorKey : "";
  const generation = timeline && timeline.dataset.timelineGeneration
    ? Number(timeline.dataset.timelineGeneration)
    : null;
  const documentGeneration = timeline && timeline.dataset.timelineDocumentGeneration
    ? Number(timeline.dataset.timelineDocumentGeneration)
    : null;
  let timelineRevision = timeline && timeline.dataset.timelineRevision
    ? Number(timeline.dataset.timelineRevision)
    : null;
  let initialPositionPending = Boolean(timeline);
  let automaticPositioning = false;
  let userInteracted = false;
  let readMarkerArmed = false;

  function notifyHost(action) {
    if (generation !== null) {
      window.location.href = "conduit://timeline-" + action + "?generation=" +
        encodeURIComponent(String(generation));
    }
  }

  function storedAtBottom() {
    if (initialMode === "bottom") return true;
    if (!stickyKey) return true;
    try {
      return sessionStorage.getItem(stickyKey) !== "false";
    } catch (_) {
      return true;
    }
  }

  function storedAnchor() {
    if (!anchorKey) return null;
    try {
      return JSON.parse(sessionStorage.getItem(anchorKey) || "null");
    } catch (_) {
      return null;
    }
  }

  function rememberStoredViewport() {
    if (!timeline) return;
    const root = timelineRoot();
    const anchor = visibleAnchor();
    const payload = {
      scrollTop: root.scrollTop,
      scrollHeight: root.scrollHeight
    };
    if (anchor) {
      payload.ts = anchor.dataset.messageTs;
      payload.top = anchor.getBoundingClientRect().top;
    }
    try {
      if (stickyKey) sessionStorage.setItem(stickyKey, isNearBottom() ? "true" : "false");
      if (anchorKey) sessionStorage.setItem(anchorKey, JSON.stringify(payload));
    } catch (_) {
    }
  }

  function restoreStoredAnchor() {
    const payload = storedAnchor();
    if (!payload) return;
    const root = timelineRoot();
    const anchor = payload.ts ? messageElement(payload.ts) : null;
    if (anchor && typeof payload.top === "number") {
      root.scrollTop += anchor.getBoundingClientRect().top - payload.top;
    } else if (
      typeof payload.scrollTop === "number" &&
      typeof payload.scrollHeight === "number"
    ) {
      root.scrollTop = payload.scrollTop + root.scrollHeight - payload.scrollHeight;
    }
  }

  function applyInitialPosition() {
    if (!initialPositionPending || userInteracted) return;
    automaticPositioning = true;
    const targetTs = timeline.dataset.focusMessageTs;
    const target = targetTs ? messageElement(targetTs) : null;
    if (target) {
      target.scrollIntoView({ block: "center", inline: "nearest" });
      viewportPinnedToBottom = false;
    } else if (initialMode === "preserve-prepend" || !storedAtBottom()) {
      restoreStoredAnchor();
      viewportPinnedToBottom = isNearBottom();
    } else {
      const root = timelineRoot();
      root.scrollTop = root.scrollHeight;
      viewportPinnedToBottom = true;
    }
  }

  function timestampAfter(left, right) {
    return left.localeCompare(right) > 0;
  }

  function armReadMarker() {
    if (readMarkerArmed || !timeline || !timeline.dataset.readMarkerUrl) return;
    readMarkerArmed = true;
    if (!("IntersectionObserver" in window)) return;
    const readMarkerUrl = timeline.dataset.readMarkerUrl;
    const sentinel = document.getElementById("timeline-read-sentinel");
    if (sentinel) {
      const sentinelObserver = new IntersectionObserver(function (entries) {
        if (!entries.some(function (entry) { return entry.isIntersecting; })) return;
        sentinelObserver.disconnect();
        window.location.href = readMarkerUrl;
      }, { threshold: 1.0 });
      sentinelObserver.observe(sentinel);
      return;
    }

    let lastSent = "";
    let timer = 0;
    const visible = new Set();
    function schedule() {
      window.clearTimeout(timer);
      timer = window.setTimeout(function () {
        const newest = Array.from(visible).sort().pop();
        if (!newest || !timestampAfter(newest, lastSent)) return;
        lastSent = newest;
        const ordered = Array.from(document.querySelectorAll("[data-message-ts]"));
        const currentIndex = ordered.findIndex(function (message) {
          return message.dataset.messageTs === newest;
        });
        const next = currentIndex >= 0 ? ordered[currentIndex + 1] : null;
        const separator = document.querySelector(".unread-separator");
        if (separator && next) next.before(separator);
        else if (separator) separator.remove();
        const target = new URL(readMarkerUrl);
        target.searchParams.set("ts", newest);
        window.location.href = target.toString();
      }, 500);
    }
    const observer = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        const ts = entry.target.dataset.messageTs;
        if (!ts) return;
        if (entry.isIntersecting) visible.add(ts); else visible.delete(ts);
      });
      schedule();
    }, { threshold: 0.01 });
    function observeUnreadMessages() {
      const boundary = document.querySelector(".unread-separator");
      if (!boundary) return;
      let afterBoundary = false;
      document.querySelectorAll(".unread-separator, [data-message-ts]").forEach(function (node) {
        if (node.classList.contains("unread-separator")) {
          afterBoundary = true;
          return;
        }
        if (afterBoundary && !node.dataset.readObserved) {
          node.dataset.readObserved = "true";
          observer.observe(node);
        }
      });
    }
    observeUnreadMessages();
    const list = document.querySelector(".message-list");
    if (list) {
      new MutationObserver(observeUnreadMessages).observe(list, {
        childList: true,
        subtree: true
      });
    }
  }

  function revealTimeline() {
    if (timeline) timeline.removeAttribute("data-timeline-positioning");
  }

  function commitInitialPosition() {
    if (!initialPositionPending || userInteracted) return;
    applyInitialPosition();
    initialPositionPending = false;
    automaticPositioning = false;
    revealTimeline();
    rememberStoredViewport();
    rememberViewportAnchor();
    armReadMarker();
    notifyHost("positioned");
  }

  function noteUserInteraction() {
    if (userInteracted || !initialPositionPending) return;
    userInteracted = true;
    initialPositionPending = false;
    automaticPositioning = false;
    revealTimeline();
    rememberStoredViewport();
    rememberViewportAnchor();
    armReadMarker();
    notifyHost("interacted");
  }

  function cancelPendingViewportRestore() {
    scrollMutationGeneration += 1;
    restoringViewportAnchor = false;
    userScrollIntentPending = true;
    if (userScrollIntentFrame) cancelAnimationFrame(userScrollIntentFrame);
    userScrollIntentFrame = requestAnimationFrame(function () {
      userScrollIntentFrame = requestAnimationFrame(rememberUserViewport);
    });
  }

  function rememberUserViewport() {
    if (userScrollIntentFrame) cancelAnimationFrame(userScrollIntentFrame);
    userScrollIntentFrame = 0;
    userScrollIntentPending = false;
    restoringViewportAnchor = false;
    viewportPinnedToBottom = isNearBottom();
    const anchor = visibleAnchor();
    if (!anchor) return;
    viewportAnchor = anchor;
    viewportAnchorTop = anchor.getBoundingClientRect().top;
  }

  function noteUserScrollIntent() {
    noteUserInteraction();
    cancelPendingViewportRestore();
  }

  ["wheel", "touchstart", "pointerdown"].forEach(function (eventName) {
    window.addEventListener(eventName, noteUserScrollIntent, { passive: true, capture: true });
  });
  window.addEventListener("keydown", function (event) {
    if (["ArrowUp", "ArrowDown", "PageUp", "PageDown", "Home", "End", " "].includes(event.key)) {
      noteUserScrollIntent();
    }
  }, true);

  function finishViewportRestore() {
    requestAnimationFrame(function () {
      restoringViewportAnchor = false;
    });
  }

  function scrollToPinnedBottom() {
    restoringViewportAnchor = true;
    viewportPinnedToBottom = true;
    const root = timelineRoot();
    root.scrollTop = root.scrollHeight;
    finishViewportRestore();
  }

  function rememberViewportAnchor() {
    if (restoringViewportAnchor) return;
    viewportPinnedToBottom = isNearBottom();
    const anchor = visibleAnchor();
    if (!anchor) return;
    viewportAnchor = anchor;
    viewportAnchorTop = anchor.getBoundingClientRect().top;
  }

  function scheduleRememberViewportAnchor() {
    if (restoringViewportAnchor || rememberViewportAnchorFrame) return;
    rememberViewportAnchorFrame = requestAnimationFrame(function () {
      rememberViewportAnchorFrame = 0;
      rememberViewportAnchor();
    });
  }

  document.addEventListener("click", function (event) {
    const message = event.target && event.target.closest
      ? event.target.closest("[data-message-ts]")
      : null;
    if (!message) return;
    viewportAnchor = message;
    viewportAnchorTop = message.getBoundingClientRect().top;
  }, true);

  function preserveViewportAnchorDuringResize() {
    if (initialPositionPending || userScrollIntentPending) return;
    if (viewportPinnedToBottom) {
      scrollToPinnedBottom();
      return;
    }
    if (!viewportAnchor || !viewportAnchor.isConnected) {
      rememberViewportAnchor();
      return;
    }

    const root = timelineRoot();
    const currentTop = viewportAnchor.getBoundingClientRect().top;
    const offset = currentTop - viewportAnchorTop;
    if (Math.abs(offset) <= 0.5) return;
    restoringViewportAnchor = true;
    root.scrollTop += offset;
    finishViewportRestore();
  }

  function preserveViewportAnchorAfterResize() {
    preserveViewportAnchorDuringResize();
    if (resizeCorrectionFrame) return;
    resizeCorrectionFrame = requestAnimationFrame(function () {
      resizeCorrectionFrame = 0;
      preserveViewportAnchorDuringResize();
    });
  }

  window.addEventListener("scroll", function (event) {
    if (initialPositionPending && !automaticPositioning && event.isTrusted) {
      noteUserInteraction();
      return;
    }
    if (userScrollIntentPending) rememberUserViewport();
    scheduleRememberViewportAnchor();
    if (!initialPositionPending) rememberStoredViewport();
  }, { passive: true });
  window.addEventListener("resize", preserveViewportAnchorAfterResize, { passive: true });
  if ("ResizeObserver" in window) {
    const timelineResizeObserver = new ResizeObserver(preserveViewportAnchorAfterResize);
    if (timeline) timelineResizeObserver.observe(timeline);
  }
  document.addEventListener("click", function (event) {
    const target = event.target && event.target.closest
      ? event.target.closest("a[href^='conduit://load-older']")
      : null;
    if (target) rememberStoredViewport();
  }, true);

  applyInitialPosition();
  requestAnimationFrame(function () {
    applyInitialPosition();
    requestAnimationFrame(commitInitialPosition);
  });

  function withPreservedScroll(mutate) {
    preservedScrollTransactions += 1;
    const root = timelineRoot();
    const nearBottom = isNearBottom();
    const arrivalVisible = nearBottom;
    const wasAtBottom = viewportPinnedToBottom || nearBottom;
    const anchor = wasAtBottom ? null : visibleAnchor();
    const anchorTs = anchor ? anchor.dataset.messageTs : null;
    const anchorTop = anchor ? anchor.getBoundingClientRect().top : 0;
    const oldScrollTop = root.scrollTop;
    const outcome = mutate(arrivalVisible);
    if (!outcome.changed) return outcome;
    const mutationGeneration = ++scrollMutationGeneration;
    function restore() {
      if (mutationGeneration !== scrollMutationGeneration) return;
      if (wasAtBottom) {
        scrollToPinnedBottom();
      } else {
        const stableAnchor = anchor && anchor.isConnected
          ? anchor
          : (anchorTs ? messageElement(anchorTs) : null);
        if (stableAnchor) {
          restoringViewportAnchor = true;
          root.scrollTop += stableAnchor.getBoundingClientRect().top - anchorTop;
          finishViewportRestore();
        } else {
          root.scrollTop = oldScrollTop;
        }
      }
    }
    restore();
    requestAnimationFrame(restore);
    requestAnimationFrame(function () { requestAnimationFrame(restore); });
    return outcome;
  }

  const operationNoop = 0;
  const operationChanged = 1;
  const operationCorrupt = 2;

  function validNonemptyString(value) {
    return typeof value === "string" && value.length > 0;
  }

  function applyTimelineOperation(patch, arrivalVisible) {
    if (!patch || typeof patch.type !== "string") return operationCorrupt;

    if (patch.type === "replace-snapshot") {
      const list = document.querySelector(".message-list");
      if (
        !list ||
        typeof patch.list_html !== "string" ||
        typeof patch.load_more_html !== "string"
      ) return operationCorrupt;
      const previous = list.previousElementSibling;
      if (previous && previous.matches(".timeline-action")) previous.remove();
      if (patch.load_more_html) list.before(fragment(patch.load_more_html));
      list.replaceChildren(fragment(patch.list_html));
      return operationChanged;
    }

    if (patch.type === "insert-message") {
      const list = document.querySelector(".message-list");
      if (
        !list ||
        typeof patch.html !== "string" ||
        !validNonemptyString(patch.message_ts) ||
        (patch.position !== "append" && patch.position !== "prepend")
      ) return operationCorrupt;
      if (messageElement(patch.message_ts)) return operationNoop;
      const content = fragment(patch.html);
      if (!messageElementIn(content, patch.message_ts)) return operationCorrupt;
      if (patch.arrival === "sent") {
        animateSentMessage(content, patch.message_ts, arrivalVisible);
      }
      if (patch.position === "prepend") list.prepend(content);
      else list.append(content);
      return operationChanged;
    }

    if (patch.type === "replace-message") {
      if (!validNonemptyString(patch.message_ts) || typeof patch.html !== "string") {
        return operationCorrupt;
      }
      const target = messageElement(patch.message_ts);
      if (!target) return operationCorrupt;
      const html = target.classList.contains("message-part") ? patch.part_html : patch.html;
      if (typeof html !== "string") return operationCorrupt;
      const content = fragment(html);
      if (!messageElementIn(content, patch.message_ts)) return operationCorrupt;
      if (patch.arrival === "sent") {
        animateSentMessage(content, patch.message_ts, arrivalVisible);
      }
      target.replaceWith(content);
      return operationChanged;
    }

    if (patch.type === "remove-message") {
      if (!validNonemptyString(patch.message_ts)) return operationCorrupt;
      const target = messageElement(patch.message_ts);
      if (!target) return operationCorrupt;
      const item = target.closest(".message-list-item");
      const stack = target.closest(".message-stack");
      target.remove();
      if (item && (!stack || stack.querySelectorAll("[data-message-ts]").length === 0)) {
        item.remove();
      }
      return operationChanged;
    }

    if (patch.type === "replace-region") {
      if (
        !validNonemptyString(patch.message_ts) ||
        !validNonemptyString(patch.region) ||
        typeof patch.html !== "string"
      ) return operationCorrupt;
      const target = messageElement(patch.message_ts);
      if (!target) return operationCorrupt;
      const region = Array.from(target.querySelectorAll("[data-message-region]")).find(
        function (element) { return element.dataset.messageRegion === patch.region; }
      );
      if (!region) return operationCorrupt;
      region.replaceChildren(fragment(patch.html));
      return operationChanged;
    }

    if (patch.type === "update-image") {
      if (
        !validNonemptyString(patch.asset_key) ||
        (patch.source !== null && typeof patch.source !== "string")
      ) return operationCorrupt;
      const targets = imageElements(patch.asset_key);
      if (targets.length === 0) return operationNoop;
      targets.forEach(function (target) {
        if (typeof patch.source === "string") {
          const isVideo = patch.source.startsWith("data:video/");
          if ((isVideo && target.matches("video")) || (!isVideo && target.matches("img"))) {
            target.src = patch.source;
          } else if (isVideo) {
            const video = document.createElement("video");
            video.preload = "metadata";
            video.muted = true;
            video.playsInline = true;
            video.src = patch.source;
            video.setAttribute("aria-label", target.dataset.imageAlt || "");
            video.dataset.imageKey = patch.asset_key;
            video.dataset.imageAlt = target.dataset.imageAlt || "";
            video.dataset.imageUnavailable = target.dataset.imageUnavailable || "";
            target.replaceWith(video);
          } else {
            const image = document.createElement("img");
            image.loading = "lazy";
            image.decoding = "async";
            image.src = patch.source;
            image.alt = target.dataset.imageAlt || "";
            image.dataset.imageKey = patch.asset_key;
            image.dataset.imageAlt = image.alt;
            image.dataset.imageUnavailable = target.dataset.imageUnavailable || "";
            target.replaceWith(image);
          }
        } else {
          const placeholder = document.createElement("div");
          placeholder.className = "image-placeholder";
          placeholder.dataset.imageKey = patch.asset_key;
          placeholder.dataset.imageAlt = target.dataset.imageAlt || "";
          placeholder.dataset.imageUnavailable = target.dataset.imageUnavailable || "";
          placeholder.textContent = placeholder.dataset.imageUnavailable;
          target.replaceWith(placeholder);
        }
      });
      return operationChanged;
    }

    if (patch.type === "update-user") {
      if (
        !validNonemptyString(patch.user_id) ||
        typeof patch.name !== "string" ||
        typeof patch.status_html !== "string"
      ) return operationCorrupt;
      const targets = authorElements(patch.user_id);
      const mentions = mentionElements(patch.user_id);
      if (targets.length === 0 && mentions.length === 0) return operationNoop;
      mentions.forEach(function (mention) {
        mention.textContent = "@" + patch.name;
      });
      targets.forEach(function (target) {
        const author = target.querySelector(".author-label");
        if (author) author.textContent = patch.name;
        const header = target.querySelector(".message-header");
        if (!header) return;
        const oldStatus = header.querySelector(".user-status");
        if (oldStatus) oldStatus.remove();
        if (patch.status_html) {
          const status = fragment(patch.status_html);
          const identity = author && (author.closest(".author-actions") || author);
          if (identity) identity.after(status);
        }
      });
      return operationChanged;
    }
    return operationCorrupt;
  }

  function applyTimelineOperations(operations, arrivalVisible) {
    let changed = false;
    for (const operation of operations) {
      const result = applyTimelineOperation(operation, arrivalVisible);
      if (result === operationCorrupt) return { changed, corrupt: true };
      if (result === operationChanged) changed = true;
    }
    return { changed, corrupt: false };
  }

  function currentTimelineRevision() {
    return Number.isSafeInteger(timelineRevision) && timelineRevision > 0
      ? timelineRevision
      : null;
  }

  function timelineApplyResult(status) {
    return { status, timeline_revision: currentTimelineRevision() };
  }

  window.conduitTimelineState = function () {
    return {
      document_generation: Number.isSafeInteger(documentGeneration) && documentGeneration > 0
        ? documentGeneration
        : null,
      timeline_revision: currentTimelineRevision(),
      preserved_scroll_transactions: preservedScrollTransactions
    };
  };

  window.conduitApplyTimelineDelta = function (delta) {
    if (!delta || typeof delta !== "object") return timelineApplyResult("corrupt");
    const currentRevision = currentTimelineRevision();
    const validGeneration = Number.isSafeInteger(documentGeneration) && documentGeneration > 0;
    const validEnvelope =
      Number.isSafeInteger(delta.document_generation) && delta.document_generation > 0 &&
      Number.isSafeInteger(delta.base_timeline_revision) && delta.base_timeline_revision > 0 &&
      Number.isSafeInteger(delta.timeline_revision) && delta.timeline_revision > 0;
    if (
      !validGeneration ||
      currentRevision === null ||
      !validEnvelope ||
      delta.document_generation !== documentGeneration ||
      delta.base_timeline_revision !== currentRevision ||
      delta.timeline_revision !== delta.base_timeline_revision + 1
    ) return timelineApplyResult("revision-mismatch");
    if (!Number.isSafeInteger(delta.id) || delta.id <= 0 || !Array.isArray(delta.operations)) {
      return timelineApplyResult("corrupt");
    }

    const outcome = withPreservedScroll(function (arrivalVisible) {
      return applyTimelineOperations(delta.operations, arrivalVisible);
    });
    if (outcome.corrupt) return timelineApplyResult("corrupt");
    timelineRevision = delta.timeline_revision;
    if (timeline) timeline.dataset.timelineRevision = String(timelineRevision);
    return timelineApplyResult("applied");
  };

  window.conduitApplyTimelinePatch = function (patch) {
    if (!patch || typeof patch.type !== "string") return false;
    const outcome = withPreservedScroll(function (arrivalVisible) {
      return applyTimelineOperations([patch], arrivalVisible);
    });
    return !outcome.corrupt;
  };
})();
