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

  let viewportAnchor = null;
  let viewportAnchorTop = 0;
  let viewportWidth = window.innerWidth;
  let restoringViewportAnchor = false;
  let rememberViewportAnchorFrame = 0;

  function rememberViewportAnchor() {
    if (restoringViewportAnchor) return;
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
    const nextWidth = window.innerWidth;
    if (Math.abs(nextWidth - viewportWidth) < 0.5) {
      scheduleRememberViewportAnchor();
      return;
    }
    viewportWidth = nextWidth;
    if (!viewportAnchor || !viewportAnchor.isConnected) {
      rememberViewportAnchor();
      return;
    }

    const root = timelineRoot();
    const currentTop = viewportAnchor.getBoundingClientRect().top;
    root.scrollTop += currentTop - viewportAnchorTop;
    restoringViewportAnchor = true;
    requestAnimationFrame(function () {
      restoringViewportAnchor = false;
    });
  }

  window.addEventListener("scroll", scheduleRememberViewportAnchor, { passive: true });
  window.addEventListener("resize", preserveViewportAnchorDuringResize, { passive: true });
  if ("ResizeObserver" in window) {
    new ResizeObserver(preserveViewportAnchorDuringResize).observe(document.documentElement);
  }
  requestAnimationFrame(rememberViewportAnchor);

  function withPreservedScroll(mutate) {
    const root = timelineRoot();
    const wasAtBottom = root.scrollHeight - root.scrollTop - root.clientHeight <= 48;
    const anchor = visibleAnchor();
    const anchorTop = anchor ? anchor.getBoundingClientRect().top : 0;
    const oldScrollTop = root.scrollTop;
    const changed = mutate();
    if (!changed) return false;
    function restore() {
      if (wasAtBottom) {
        root.scrollTop = root.scrollHeight;
      } else if (anchor && anchor.isConnected) {
        root.scrollTop += anchor.getBoundingClientRect().top - anchorTop;
      } else {
        root.scrollTop = oldScrollTop;
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
