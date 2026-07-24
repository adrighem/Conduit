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

  function visibleAnchor() {
    return Array.from(document.querySelectorAll("[data-message-ts]")).find(function (element) {
      const rect = element.getBoundingClientRect();
      return rect.bottom >= 0 && rect.top <= window.innerHeight;
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
  let scrollMutationGeneration = 0;
  const timeline = document.querySelector(".timeline");
  const initialMode = timeline ? timeline.dataset.timelineMode || "preserve" : "preserve";
  const stickyKey = timeline ? timeline.dataset.timelineStickyKey : "";
  const anchorKey = timeline ? timeline.dataset.timelineAnchorKey : "";
  const generation = timeline && timeline.dataset.timelineGeneration
    ? Number(timeline.dataset.timelineGeneration)
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

  ["wheel", "touchstart", "pointerdown"].forEach(function (eventName) {
    window.addEventListener(eventName, noteUserInteraction, { passive: true, capture: true });
  });
  window.addEventListener("keydown", function (event) {
    if (["ArrowUp", "ArrowDown", "PageUp", "PageDown", "Home", "End", " "].includes(event.key)) {
      noteUserInteraction();
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
    if (initialPositionPending) return;
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
    const root = timelineRoot();
    const wasAtBottom = viewportPinnedToBottom || isNearBottom();
    const anchor = visibleAnchor();
    const anchorTs = anchor ? anchor.dataset.messageTs : null;
    const anchorTop = anchor ? anchor.getBoundingClientRect().top : 0;
    const oldScrollTop = root.scrollTop;
    const changed = mutate();
    if (!changed) return false;
    const generation = ++scrollMutationGeneration;
    function restore() {
      if (generation !== scrollMutationGeneration) return;
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
    return true;
  }

  window.conduitApplyTimelinePatch = function (patch) {
    if (!patch || typeof patch.type !== "string") return false;
    return withPreservedScroll(function () {
      if (patch.type === "replace-snapshot") {
        const list = document.querySelector(".message-list");
        if (
          !list ||
          typeof patch.list_html !== "string" ||
          typeof patch.load_more_html !== "string"
        ) return false;
        const previous = list.previousElementSibling;
        if (previous && previous.matches(".timeline-action")) previous.remove();
        if (patch.load_more_html) list.before(fragment(patch.load_more_html));
        list.replaceChildren(fragment(patch.list_html));
        return true;
      }

      if (patch.type === "insert-message") {
        const list = document.querySelector(".message-list");
        if (!list || typeof patch.html !== "string") return false;
        if (patch.position === "prepend") list.prepend(fragment(patch.html));
        else list.append(fragment(patch.html));
        return true;
      }

      if (patch.type === "replace-message") {
        const target = messageElement(patch.message_ts);
        if (!target || typeof patch.html !== "string") return false;
        const html = target.classList.contains("message-part") ? patch.part_html : patch.html;
        if (typeof html !== "string") return false;
        target.replaceWith(fragment(html));
        return true;
      }

      if (patch.type === "remove-message") {
        const target = messageElement(patch.message_ts);
        if (!target) return false;
        const item = target.closest(".message-list-item");
        const stack = target.closest(".message-stack");
        target.remove();
        if (item && (!stack || stack.querySelectorAll("[data-message-ts]").length === 0)) item.remove();
        return true;
      }

      if (patch.type === "replace-region") {
        const target = messageElement(patch.message_ts);
        if (!target || typeof patch.html !== "string") return false;
        const region = target.querySelector('[data-message-region="' + patch.region + '"]');
        if (!region) return false;
        region.replaceChildren(fragment(patch.html));
        return true;
      }

      if (patch.type === "update-image") {
        const targets = imageElements(patch.asset_key);
        if (targets.length === 0) return false;
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
        return true;
      }

      if (patch.type === "update-user") {
        const targets = authorElements(patch.user_id);
        const mentions = mentionElements(patch.user_id);
        if (targets.length === 0 && mentions.length === 0) return false;
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
        return true;
      }
      return false;
    });
  };
})();
