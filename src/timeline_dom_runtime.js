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

  function cachedAssetSource(value) {
    if (!value || typeof value !== "object") return null;
    if (value.kind !== "image" && value.kind !== "video") return null;
    if (typeof value.uri !== "string") return null;
    const prefix = "conduit-asset://";
    if (!value.uri.startsWith(prefix)) return null;
    const key = value.uri.slice(prefix.length);
    if (!/^[0-9a-f]{64}$/.test(key)) return null;
    return value;
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

  let dwellTimer = null;
  let scrollSettleTimer = null;
  let reportedReadTimestamps = new Set();
  let observer = null;

  function isMessageQualifiedVisible(element) {
    if (!element) return false;
    const rect = element.getBoundingClientRect();
    const viewportHeight = window.innerHeight || document.documentElement.clientHeight;
    if (viewportHeight <= 0 || rect.height <= 0) return false;

    const visibleTop = Math.max(0, rect.top);
    const visibleBottom = Math.min(viewportHeight, rect.bottom);
    const intersectionHeight = Math.max(0, visibleBottom - visibleTop);
    const intersectionRatio = intersectionHeight / rect.height;

    const criterionA = intersectionRatio >= 0.8;
    const criterionB =
      rect.height >= 0.5 * viewportHeight &&
      intersectionHeight >= 0.5 * viewportHeight;

    return criterionA || criterionB;
  }

  function dispatchMessagesRead(timestamps) {
    if (!timestamps || timestamps.length === 0) return;
    if (
      window.conduitBridge &&
      window.conduitBridge.messageHandlers &&
      window.conduitBridge.messageHandlers.conduit
    ) {
      window.conduitBridge.messageHandlers.conduit.postMessage({
        type: "messages_read",
        timestamps: timestamps
      });
    }
  }

  function checkAndDispatchReadMessages() {
    const messages = document.querySelectorAll("[data-message-ts]");
    const unreadQualified = [];
    messages.forEach(function (el) {
      const ts = el.dataset.messageTs;
      if (ts && !reportedReadTimestamps.has(ts)) {
        if (isMessageQualifiedVisible(el)) {
          unreadQualified.push(ts);
        }
      }
    });
    if (unreadQualified.length > 0) {
      unreadQualified.forEach(function (ts) {
        reportedReadTimestamps.add(ts);
      });
      dispatchMessagesRead(unreadQualified);
    }
  }

  function scheduleDwellTimer() {
    if (dwellTimer || scrollSettleTimer) return;
    dwellTimer = window.setTimeout(function () {
      dwellTimer = null;
      checkAndDispatchReadMessages();
    }, 2000);
  }

  function onScrollReadState() {
    if (dwellTimer) {
      window.clearTimeout(dwellTimer);
      dwellTimer = null;
    }
    if (scrollSettleTimer) {
      window.clearTimeout(scrollSettleTimer);
    }
    scrollSettleTimer = window.setTimeout(function () {
      scrollSettleTimer = null;
      scheduleDwellTimer();
    }, 150);
  }

  function observeNewMessages(container) {
    if (!observer || !container) return;
    const elements = container.querySelectorAll
      ? Array.from(container.querySelectorAll("[data-message-ts]"))
      : [];
    if (container.matches && container.matches("[data-message-ts]")) {
      elements.push(container);
    }
    elements.forEach(function (el) {
      observer.observe(el);
    });
  }

  function initReadDetection() {
    cleanupReadDetection();
    if (typeof IntersectionObserver !== "undefined") {
      observer = new IntersectionObserver(
        function () {
          if (!scrollSettleTimer) {
            scheduleDwellTimer();
          }
        },
        {
          threshold: [0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0]
        }
      );
      document.querySelectorAll("[data-message-ts]").forEach(function (el) {
        observer.observe(el);
      });
    }
    scheduleDwellTimer();
  }

  function cleanupReadDetection() {
    if (dwellTimer) {
      window.clearTimeout(dwellTimer);
      dwellTimer = null;
    }
    if (scrollSettleTimer) {
      window.clearTimeout(scrollSettleTimer);
      scrollSettleTimer = null;
    }
    if (observer) {
      observer.disconnect();
      observer = null;
    }
  }

  let viewportAnchor = null;
  let viewportAnchorTop = 0;
  let restoringViewportAnchor = false;
  let viewportPinnedToBottom = isNearBottom();
  let rememberViewportAnchorFrame = 0;
  let resizeCorrectionFrame = 0;
  let scrollMutationGeneration = 0;
  let snapshotReplaced = false;
  const timeline = document.querySelector(".timeline");
  let initialPositionPending = Boolean(timeline);
  let automaticPositioning = false;
  let userInteracted = false;

  function notifyHost(action) {
    if (!timeline) return;
    const generation = timeline.dataset.timelineGeneration
      ? Number(timeline.dataset.timelineGeneration)
      : null;
    if (generation !== null) {
      window.location.href = "conduit://timeline-" + action + "?generation=" +
        encodeURIComponent(String(generation));
    }
  }

  function storedAtBottom() {
    if (!timeline) return true;
    const mode = timeline.dataset.timelineMode || "preserve";
    if (mode === "bottom") return true;
    const stickyKey = timeline.dataset.timelineStickyKey;
    if (!stickyKey) return true;
    try {
      return sessionStorage.getItem(stickyKey) !== "false";
    } catch (_) {
      return true;
    }
  }

  function storedAnchor() {
    if (!timeline) return null;
    const anchorKey = timeline.dataset.timelineAnchorKey;
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
    const stickyKey = timeline.dataset.timelineStickyKey;
    const anchorKey = timeline.dataset.timelineAnchorKey;
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
    if (!timeline || !initialPositionPending || userInteracted) return;
    automaticPositioning = true;
    const targetTs = timeline.dataset.focusMessageTs;
    const target = targetTs ? messageElement(targetTs) : null;
    if (target) {
      target.scrollIntoView({ block: "center", inline: "nearest" });
      viewportPinnedToBottom = false;
    } else if (
      (timeline.dataset.timelineMode || "preserve") === "preserve-prepend" ||
      !storedAtBottom()
    ) {
      restoreStoredAnchor();
      viewportPinnedToBottom = isNearBottom();
    } else {
      const root = timelineRoot();
      root.scrollTop = root.scrollHeight;
      viewportPinnedToBottom = true;
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
    initReadDetection();
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
    onScrollReadState();
    scheduleRememberViewportAnchor();
    if (!initialPositionPending) rememberStoredViewport();
  }, { passive: true });
  window.addEventListener("resize", preserveViewportAnchorAfterResize, { passive: true });
  if (timeline) {
    timeline.addEventListener("load", function (event) {
      if (event.target && event.target.matches && event.target.matches("img, video")) {
        preserveViewportAnchorAfterResize();
      }
    }, true);
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
    const nearBottom = isNearBottom();
    const arrivalVisible = nearBottom;
    const wasAtBottom = viewportPinnedToBottom || nearBottom;
    const anchor = wasAtBottom ? null : visibleAnchor();
    const anchorTs = anchor ? anchor.dataset.messageTs : null;
    const anchorTop = anchor ? anchor.getBoundingClientRect().top : 0;
    const oldScrollTop = root.scrollTop;
    snapshotReplaced = false;
    const changed = mutate(arrivalVisible);
    if (!changed) return false;
    if (snapshotReplaced) {
      return true;
    }
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

  function applyTimelinePatch(patch, arrivalVisible) {
    if (!patch || typeof patch.type !== "string") return false;
    if (patch.type === "replace-snapshot") {
      const list = document.querySelector(".message-list");
      if (
        !list ||
        !timeline ||
        typeof patch.list_html !== "string" ||
        typeof patch.load_more_html !== "string"
      ) return false;

      snapshotReplaced = true;
      cleanupReadDetection();
      reportedReadTimestamps.clear();

      if (rememberViewportAnchorFrame) {
        window.cancelAnimationFrame(rememberViewportAnchorFrame);
        rememberViewportAnchorFrame = 0;
      }
      if (resizeCorrectionFrame) {
        window.cancelAnimationFrame(resizeCorrectionFrame);
        resizeCorrectionFrame = 0;
      }

      const previous = list.previousElementSibling;
      if (previous && previous.matches(".timeline-action")) previous.remove();
      if (patch.load_more_html) list.before(fragment(patch.load_more_html));
      list.replaceChildren(fragment(patch.list_html));

      if (typeof patch.empty_label === "string") {
        list.dataset.emptyLabel = patch.empty_label;
      }
      if (typeof patch.timeline_mode === "string") {
        timeline.dataset.timelineMode = patch.timeline_mode;
      }
      if (typeof patch.sticky_key === "string") {
        timeline.dataset.timelineStickyKey = patch.sticky_key;
      }
      if (typeof patch.anchor_key === "string") {
        timeline.dataset.timelineAnchorKey = patch.anchor_key;
      }
      if (typeof patch.focus_message_ts === "string" && patch.focus_message_ts) {
        timeline.dataset.focusMessageTs = patch.focus_message_ts;
      } else {
        delete timeline.dataset.focusMessageTs;
      }
      if (typeof patch.generation === "number") {
        timeline.dataset.timelineGeneration = String(patch.generation);
      } else {
        delete timeline.dataset.timelineGeneration;
      }
      if (window.getSelection) {
        try {
          const sel = window.getSelection();
          if (sel) sel.removeAllRanges();
        } catch (_) {}
      }
      document.querySelectorAll("details.author-actions[open]").forEach(function (menu) {
        menu.open = false;
      });
      document.querySelectorAll(".mention-actions > button[aria-expanded='true']").forEach(function (button) {
        button.setAttribute("aria-expanded", "false");
        if (button.nextElementSibling) button.nextElementSibling.hidden = true;
      });

      viewportAnchor = null;
      viewportAnchorTop = 0;
      restoringViewportAnchor = false;
      userInteracted = false;
      initialPositionPending = true;
      automaticPositioning = true;
      timeline.setAttribute("data-timeline-positioning", "pending");

      applyInitialPosition();
      requestAnimationFrame(function () {
        applyInitialPosition();
        requestAnimationFrame(commitInitialPosition);
      });

      return true;
    }

      if (patch.type === "insert-message") {
        const list = document.querySelector(".message-list");
        if (
          !list ||
          typeof patch.html !== "string" ||
          typeof patch.message_ts !== "string"
        ) return false;
        const content = fragment(patch.html);
        if (patch.arrival === "sent") {
          animateSentMessage(content, patch.message_ts, arrivalVisible);
        }
        if (patch.position === "prepend") list.prepend(content);
        else list.append(content);
        observeNewMessages(list);
        if (!scrollSettleTimer) scheduleDwellTimer();
        return true;
      }

      if (patch.type === "replace-message") {
        const target = messageElement(patch.message_ts);
        if (!target || typeof patch.html !== "string") return false;
        const html = target.classList.contains("message-part") ? patch.part_html : patch.html;
        if (typeof html !== "string") return false;
        const content = fragment(html);
        if (patch.arrival === "sent") {
          animateSentMessage(content, patch.message_ts, arrivalVisible);
        }
        const parent = target.parentElement;
        target.replaceWith(content);
        observeNewMessages(parent || document);
        if (!scrollSettleTimer) scheduleDwellTimer();
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
        const source = cachedAssetSource(patch.source);
        targets.forEach(function (target) {
          if (source) {
            const isVideo = source.kind === "video";
            if ((isVideo && target.matches("video")) || (!isVideo && target.matches("img"))) {
              target.src = source.uri;
            } else if (isVideo) {
              const video = document.createElement("video");
              video.preload = "metadata";
              video.muted = true;
              video.playsInline = true;
              video.src = source.uri;
              video.setAttribute("aria-label", target.dataset.imageAlt || "");
              video.dataset.imageKey = patch.asset_key;
              video.dataset.imageAlt = target.dataset.imageAlt || "";
              video.dataset.imageUnavailable = target.dataset.imageUnavailable || "";
              target.replaceWith(video);
            } else {
              const image = document.createElement("img");
              image.loading = "lazy";
              image.decoding = "async";
              image.src = source.uri;
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
  }

  window.conduitApplyTimelinePatch = function (patch) {
    return withPreservedScroll(function (arrivalVisible) {
      return applyTimelinePatch(patch, arrivalVisible);
    });
  };

  window.conduitApplyTimelineDelta = function (patches) {
    if (!Array.isArray(patches) || patches.length === 0) return false;
    return withPreservedScroll(function (arrivalVisible) {
      return patches.every(function (patch) {
        return applyTimelinePatch(patch, arrivalVisible);
      });
    });
  };
})();
