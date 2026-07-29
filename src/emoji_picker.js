(function () {
  const picker = document.getElementById("emoji-picker");
  if (!picker) return;

  const PROTOCOL_VERSION = Number(picker.dataset.emojiProtocolVersion);
  const RESULT_LIMIT = Number(picker.dataset.emojiResultLimit);
  const MAX_QUERY_CHARACTERS = Number(picker.dataset.emojiMaxQueryChars);
  if (
    !Number.isSafeInteger(PROTOCOL_VERSION) ||
    PROTOCOL_VERSION <= 0 ||
    !Number.isSafeInteger(RESULT_LIMIT) ||
    RESULT_LIMIT <= 0 ||
    !Number.isSafeInteger(MAX_QUERY_CHARACTERS) ||
    MAX_QUERY_CHARACTERS <= 0
  ) {
    return;
  }
  const search = picker.querySelector("#emoji-search");
  const grid = picker.querySelector("#emoji-grid");
  const categories = picker.querySelector(".emoji-categories");
  const tabs = Array.from(picker.querySelectorAll("[data-emoji-category]"));
  const empty = picker.querySelector(".emoji-empty");
  const pageControls = picker.querySelector(".emoji-page-controls");
  const pageStatus = picker.querySelector(".emoji-page-status");
  const previousPage = picker.querySelector("[data-emoji-previous]");
  const nextPage = picker.querySelector("[data-emoji-next]");
  let activeCategory = "Smileys";
  let activeGeneration = 0;
  let currentOffset = 0;
  let currentTotal = 0;
  let currentPageSize = 0;
  let hasPrevious = false;
  let hasMore = false;
  let reactionTemplate = "";
  let opener = null;
  let selectedChoice = null;
  let choices = [];
  let searchTimer = 0;
  let pendingFocusEdge = null;

  function nativeHandlerAvailable() {
    return Boolean(
      window.webkit &&
      window.webkit.messageHandlers &&
      window.webkit.messageHandlers.conduitEmojiPicker
    );
  }

  function visibleChoices() {
    return choices;
  }

  function selectChoice(choice, focus) {
    selectedChoice = choice || null;
    choices.forEach(function (item) {
      const selected = item === selectedChoice;
      item.setAttribute("aria-selected", String(selected));
      item.tabIndex = selected ? 0 : -1;
    });
    if (selectedChoice) {
      search.setAttribute("aria-activedescendant", selectedChoice.id);
      selectedChoice.scrollIntoView({ block: "nearest" });
      if (focus) selectedChoice.focus();
    } else {
      search.removeAttribute("aria-activedescendant");
    }
  }

  function requestResults(offset, focusEdge) {
    activeGeneration += 1;
    const query = Array.from(search.value.trim())
      .slice(0, MAX_QUERY_CHARACTERS)
      .join("");
    pendingFocusEdge = focusEdge || null;
    selectedChoice = null;
    choices = [];
    currentPageSize = 0;
    search.removeAttribute("aria-activedescendant");
    grid.replaceChildren();
    grid.setAttribute("aria-busy", "true");
    empty.hidden = true;
    pageControls.hidden = true;
    if (!nativeHandlerAvailable()) {
      grid.removeAttribute("aria-busy");
      empty.hidden = false;
      return;
    }
    window.webkit.messageHandlers.conduitEmojiPicker.postMessage({
      version: PROTOCOL_VERSION,
      generation: activeGeneration,
      query,
      category: query.length === 0 ? activeCategory : null,
      offset: Math.max(0, Number(offset) || 0)
    });
  }

  function validCustomEmojiUrl(value) {
    try {
      const url = new URL(value);
      return url.protocol === "https:" || url.protocol === "http:";
    } catch (_) {
      return false;
    }
  }

  function createChoice(entry, index) {
    if (!entry || typeof entry.name !== "string" || typeof entry.label !== "string") {
      return null;
    }
    const choice = document.createElement("button");
    choice.id = "emoji-choice-" + activeGeneration + "-" + index;
    choice.type = "button";
    choice.className = "emoji-choice";
    choice.setAttribute("role", "gridcell");
    choice.setAttribute("aria-selected", "false");
    choice.setAttribute(
      "aria-label",
      typeof entry.accessible_label === "string"
        ? entry.accessible_label
        : ":" + entry.name + ": - " + entry.label
    );
    choice.title = ":" + entry.name + ":";
    choice.tabIndex = -1;
    choice.dataset.emojiName = entry.name;
    choice.dataset.emojiLabel = entry.label;
    choice.dataset.category =
      typeof entry.category === "string" ? entry.category : "";

    if (entry.value_kind === "unicode" && typeof entry.value === "string") {
      choice.textContent = entry.value;
      return choice;
    }
    if (
      entry.value_kind === "custom-image" &&
      typeof entry.value === "string" &&
      validCustomEmojiUrl(entry.value)
    ) {
      const image = document.createElement("img");
      image.className = "custom-emoji";
      image.alt = "";
      image.setAttribute("aria-hidden", "true");
      image.loading = "lazy";
      image.src = entry.value;
      choice.appendChild(image);
      return choice;
    }
    return null;
  }

  function updatePageControls(result) {
    currentOffset = Math.max(0, Number(result.offset) || 0);
    currentTotal = Math.max(0, Number(result.total) || 0);
    hasPrevious = Boolean(result.has_previous);
    hasMore = Boolean(result.has_more);
    previousPage.disabled = !hasPrevious;
    nextPage.disabled = !hasMore;
    pageControls.hidden = !hasPrevious && !hasMore;
    const end = Math.min(currentOffset + currentPageSize, currentTotal);
    pageStatus.textContent =
      currentTotal === 0 ? "" : (currentOffset + 1) + "-" + end + " / " + currentTotal;
  }

  window.conduitReceiveEmojiPickerResult = function (result) {
    if (
      !picker.open ||
      !result ||
      result.version !== PROTOCOL_VERSION ||
      result.generation !== activeGeneration ||
      !Array.isArray(result.entries)
    ) {
      return false;
    }

    const entries = result.entries.slice(0, RESULT_LIMIT);
    currentPageSize = entries.length;
    choices = entries
      .map(createChoice)
      .filter(Boolean);
    grid.replaceChildren(...choices);
    grid.removeAttribute("aria-busy");
    empty.hidden = choices.length !== 0;
    updatePageControls(result);

    const focusLast = pendingFocusEdge === "last";
    const choice = focusLast ? choices[choices.length - 1] : choices[0];
    selectChoice(choice || null, pendingFocusEdge !== null);
    pendingFocusEdge = null;
    return true;
  };

  function moveSelection(offset) {
    const visible = visibleChoices();
    if (visible.length === 0) return;
    const current = Math.max(0, visible.indexOf(selectedChoice));
    if (offset > 0 && current === visible.length - 1 && hasMore) {
      requestResults(currentOffset + visible.length, "first");
      return;
    }
    if (offset < 0 && current === 0 && hasPrevious) {
      requestResults(Math.max(0, currentOffset - RESULT_LIMIT), "last");
      return;
    }
    const next = Math.max(0, Math.min(visible.length - 1, current + offset));
    selectChoice(visible[next], false);
  }

  function activateChoice(choice) {
    if (!choice || !reactionTemplate.startsWith("conduit://reaction?")) return;
    const url = reactionTemplate.replace(
      "__REACTION__",
      encodeURIComponent(choice.dataset.emojiName)
    );
    picker.close();
    window.location.href = url;
  }

  function cancelPicker(event) {
    if (event) {
      event.preventDefault();
      event.stopPropagation();
    }
    if (picker.open) picker.close("cancel");
  }

  document.addEventListener("click", function (event) {
    const menuAction = event.target.closest(".more-actions-menu a");
    if (menuAction) {
      const menu = menuAction.closest("details");
      if (menu) menu.open = false;
    }
    const trigger = event.target.closest("[data-open-emoji-picker]");
    if (!trigger) return;
    event.preventDefault();
    opener = trigger;
    reactionTemplate = trigger.dataset.reactionTemplate || "";
    search.value = "";
    categories.hidden = false;
    picker.showModal();
    search.focus();
    requestResults(0);
  });

  picker.querySelector(".picker-close").addEventListener("click", cancelPicker);
  picker.addEventListener("cancel", cancelPicker);
  document.addEventListener("keydown", function (event) {
    if (!picker.open || (event.key !== "Escape" && event.key !== "Esc")) return;
    cancelPicker(event);
  }, true);
  picker.addEventListener("keydown", function (event) {
    if (event.key === "ArrowUp" || event.key === "ArrowDown") {
      event.preventDefault();
      event.stopPropagation();
      moveSelection(event.key === "ArrowUp" ? -1 : 1);
    } else if (event.key === "Enter" && selectedChoice) {
      event.preventDefault();
      event.stopPropagation();
      activateChoice(selectedChoice);
    }
  }, true);
  picker.addEventListener("click", function (event) {
    const choice = event.target.closest(".emoji-choice");
    if (choice && picker.contains(choice)) {
      activateChoice(choice);
      return;
    }
    if (event.target !== picker) return;
    const bounds = picker.getBoundingClientRect();
    const inside = event.clientX >= bounds.left && event.clientX <= bounds.right
      && event.clientY >= bounds.top && event.clientY <= bounds.bottom;
    if (!inside) cancelPicker(event);
  });
  picker.addEventListener("close", function () {
    window.clearTimeout(searchTimer);
    selectedChoice = null;
    choices = [];
    currentPageSize = 0;
    grid.replaceChildren();
    grid.removeAttribute("aria-busy");
    pageControls.hidden = true;
    if (opener) opener.focus();
  });
  search.addEventListener("input", function () {
    categories.hidden = search.value.trim().length > 0;
    window.clearTimeout(searchTimer);
    searchTimer = window.setTimeout(function () {
      requestResults(0);
    }, 60);
  });
  tabs.forEach(function (tab) {
    tab.addEventListener("click", function () {
      activeCategory = tab.dataset.emojiCategory;
      tabs.forEach(function (item) {
        item.setAttribute("aria-selected", String(item === tab));
      });
      search.value = "";
      categories.hidden = false;
      requestResults(0, "first");
    });
  });
  previousPage.addEventListener("click", function () {
    if (hasPrevious) requestResults(Math.max(0, currentOffset - RESULT_LIMIT), "first");
  });
  nextPage.addEventListener("click", function () {
    if (hasMore) requestResults(currentOffset + currentPageSize, "first");
  });
})();
