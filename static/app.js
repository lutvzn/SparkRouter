// Surface failed HTMX actions without injecting server-provided HTML.
function showActionError(message) {
  const error = document.getElementById("action-error");
  error.textContent = message;
  error.hidden = false;
}
document.body.addEventListener("htmx:beforeRequest", () => {
  document.getElementById("action-error").hidden = true;
});
document.body.addEventListener("htmx:responseError", (event) => {
  let message = "The action failed. Reload the page and try again.";
  try {
    message = JSON.parse(event.detail.xhr.responseText).error.message || message;
  } catch (_) { /* Non-JSON error responses use the safe default message. */ }
  showActionError(message);
});
document.body.addEventListener("htmx:sendError", () => {
  showActionError("Unable to reach the gateway. Check the connection and try again.");
});

// CSS provides hover/focus help even without JavaScript. Explicitly focus on
// tap (including browsers that do not focus clicked buttons) and allow Escape
// to dismiss a tooltip without moving keyboard focus or the mouse pointer.
document.querySelectorAll(".field-help").forEach((help) => {
  const reopen = () => help.classList.remove("help-dismissed");
  help.addEventListener("pointerenter", reopen);
  help.addEventListener("focusin", reopen);
  help.querySelector(".info-button").addEventListener("click", (event) => {
    reopen();
    event.currentTarget.focus({ preventScroll: true });
  });
});
document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape") return;
  document.querySelectorAll(".field-help").forEach((help) => {
    help.classList.add("help-dismissed");
  });
});

// Progressive copy buttons ship hidden in the markup and are only revealed
// when a clipboard API is available. Plain HTTP LAN addresses need the legacy
// copy fallback because the async Clipboard API requires a secure context.
function revealCopyButtons() {
  if (!(navigator.clipboard && typeof navigator.clipboard.writeText === "function")
      && typeof document.execCommand !== "function") return;
  document.querySelectorAll("[data-copy-model]").forEach((button) => {
    button.hidden = false;
  });
}

// Copy buttons carry an inline icon; only their label span may change so the
// icon survives success and failure updates.
function setCopyLabel(button, text) {
  const label = button.querySelector(".btn-label");
  if (label) {
    label.textContent = text;
  } else {
    button.textContent = text;
  }
}

function copyWithFallback(text) {
  const previousFocus = document.activeElement;
  const input = document.createElement("textarea");
  input.value = text;
  input.readOnly = true;
  input.className = "clipboard-fallback";
  document.body.appendChild(input);
  try {
    input.focus({ preventScroll: true });
    input.select();
    if (!document.execCommand("copy")) throw new Error("Copy unavailable");
  } finally {
    input.remove();
    if (previousFocus && typeof previousFocus.focus === "function") {
      previousFocus.focus({ preventScroll: true });
    }
  }
}

let copyToastTimer;
function showCopyStatus(status, message) {
  if (!status) return;
  window.clearTimeout(copyToastTimer);
  status.textContent = message;
  copyToastTimer = window.setTimeout(() => { status.textContent = ""; }, 2000);
}

async function copyModelId(button) {
  const modelId = button.dataset.copyModel;
  const status = document.getElementById("copy-status");
  button.disabled = true;
  try {
    if (navigator.clipboard && typeof navigator.clipboard.writeText === "function") {
      try {
        await navigator.clipboard.writeText(modelId);
      } catch (_) {
        copyWithFallback(modelId);
      }
    } else {
      copyWithFallback(modelId);
    }
    setCopyLabel(button, "Copied");
    button.classList.add("is-copied");
    window.setTimeout(() => button.classList.remove("is-copied"), 1200);
    showCopyStatus(status, "Text copied");
  } catch (_) {
    // Never surface the raw rejection; the visible model ID is the recovery path.
    setCopyLabel(button, "Copy failed");
    showCopyStatus(status, "Could not copy. Copy the model ID manually.");
  } finally {
    button.disabled = false;
  }
}

document.body.addEventListener("click", (event) => {
  const button = event.target.closest("[data-copy-model]");
  if (button) copyModelId(button);
});

revealCopyButtons();
document.body.addEventListener("htmx:afterSwap", revealCopyButtons);

// Provider search filters tile titles as the user types. It never submits, and
// it ignores protocol badges so "open" does not keep every OpenAI-compatible card.
function filterProviderTiles() {
  const input = document.getElementById("provider-search");
  const grid = document.getElementById("provider-grid");
  if (!input || !grid) return;
  const empty = document.getElementById("provider-search-empty");
  const filters = Array.from(document.querySelectorAll("[data-provider-tag-filter]"));
  const apply = () => {
    const selected = filters.filter((filter) => filter.checked).map((filter) => filter.value);
    const query = input.value.trim().toLowerCase();
    let visible = 0;
    grid.querySelectorAll(".provider-tile").forEach((tile) => {
      const title = tile.querySelector(".provider-tile-title");
      const name = title ? title.textContent.toLowerCase() : "";
      const tags = (tile.getAttribute("data-provider-tags") || "").split(" ");
      const match = (query === "" || name.includes(query))
        && (selected.length === 0 || selected.some((tag) => tags.includes(tag)));
      tile.hidden = !match;
      if (match) visible += 1;
    });
    if (empty) {
      empty.textContent = query ? "No providers match \u201c" + query + "\u201d." : "No providers match.";
      empty.hidden = visible !== 0;
    }
  };
  filters.forEach((filter) => filter.addEventListener("change", apply));
  input.addEventListener("input", apply);
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") event.preventDefault();
  });
  apply();
}

filterProviderTiles();

// The provider dropdown filters the connection dropdown. Model suggestions
// follow the selected connection. Typed IDs stay valid when it has no list.
function bindComboStepPickers() {
  document.querySelectorAll("[data-step-picker]").forEach((form) => {
  if (form.dataset.pickerBound) return;
  const provider = form.querySelector("[data-step-provider]");
  const connection = form.querySelector("[data-step-connection]");
  const input = form.querySelector("[data-step-model]");
  if (!provider || !connection || !input) return;
  form.dataset.pickerBound = "true";
  const applyModels = () => {
    const list = Array.from(form.querySelectorAll("datalist[data-models-for]")).find((list) => list.dataset.modelsFor === connection.value);
    if (list && list.options.length > 0) {
      input.setAttribute("list", list.id);
    } else {
      input.removeAttribute("list");
    }
  };
  const applyConnections = () => {
    const group = provider.value;
    let selectedVisible = false;
    let first = null;
    for (const option of connection.options) {
      const match = option.dataset.provider === group;
      option.hidden = !match;
      option.disabled = !match;
      if (match && !first) first = option;
      if (match && option.value === connection.value) selectedVisible = true;
    }
    if (!selectedVisible && first) connection.value = first.value;
    applyModels();
  };
  provider.addEventListener("change", applyConnections);
  connection.addEventListener("change", applyModels);
  applyConnections();
  });
}

bindComboStepPickers();
document.body.addEventListener("htmx:afterSwap", bindComboStepPickers);

// Edit sits beside Remove; expand the same inline form rather than navigating.
// Without JavaScript the native details disclosure remains available.
function bindComboStepEditors() {
  document.querySelectorAll("[data-step-edit-toggle]").forEach((button) => {
    if (button.dataset.editorBound) return;
    const row = document.getElementById(button.getAttribute("aria-controls"));
    const details = row?.querySelector("details");
    const summary = details?.querySelector("summary");
    if (!row || !details || !summary) return;
    button.dataset.editorBound = "true";
    row.hidden = true;
    details.open = true;
    summary.hidden = true;
    button.hidden = false;
    button.addEventListener("click", () => {
      row.hidden = !row.hidden;
      button.setAttribute("aria-expanded", String(!row.hidden));
      if (!row.hidden) row.querySelector("[data-step-model]")?.focus();
    });
  });
}

bindComboStepEditors();
document.body.addEventListener("htmx:afterSwap", bindComboStepEditors);

// Model dropdowns filter their cards as the user types. Delegation keeps this
// working after HTMX replaces a refreshed list.
document.addEventListener("input", (event) => {
  const input = event.target.closest("[data-model-search]");
  if (!input) return;
  const panel = input.closest(".model-panel");
  if (!panel) return;
  const query = input.value.trim().toLowerCase();
  let visible = 0;
  panel.querySelectorAll(".model-card").forEach((card) => {
    const text = (card.getAttribute("data-model-text") || "").toLowerCase();
    const match = query === "" || text.includes(query);
    card.hidden = !match;
    if (match) visible += 1;
  });
  const empty = panel.querySelector(".model-search-empty");
  if (empty) {
    empty.textContent = query ? "No models match \u201c" + query + "\u201d." : "No models match.";
    empty.hidden = visible !== 0;
  }
});

document.addEventListener("keydown", (event) => {
  const input = event.target.closest?.("[data-model-search]");
  if (input && event.key === "Enter") event.preventDefault();
});
