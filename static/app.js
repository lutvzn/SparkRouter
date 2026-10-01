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

// Progressive copy buttons ship hidden in the markup and are only revealed
// when the async Clipboard API is usable, so browsers without it never see a
// dead control.
function revealCopyButtons() {
  if (!(navigator.clipboard && typeof navigator.clipboard.writeText === "function")) return;
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

async function copyModelId(button) {
  const modelId = button.dataset.copyModel;
  const status = document.getElementById("copy-status");
  button.disabled = true;
  try {
    await navigator.clipboard.writeText(modelId);
    setCopyLabel(button, "Copied");
    button.classList.add("is-copied");
    window.setTimeout(() => button.classList.remove("is-copied"), 1200);
    if (status) status.textContent = "Copied " + modelId;
  } catch (_) {
    // Never surface the raw rejection; the visible model ID is the recovery path.
    setCopyLabel(button, "Copy failed");
    if (status) status.textContent = "Could not copy. Model ID: " + modelId;
  } finally {
    button.disabled = false;
  }
}

document.body.addEventListener("click", (event) => {
  const button = event.target.closest("[data-copy-model]");
  if (button) copyModelId(button);
  const info = event.target.closest("[data-model-info]");
  if (info) openModelInfo(info);
});

function openModelInfo(button) {
  const source = button.closest(".model-card")?.querySelector(".model-info-source");
  const dialog = document.getElementById("model-info-dialog");
  const body = dialog?.querySelector(".model-info-body");
  if (!source || !dialog || !body || typeof dialog.showModal !== "function") return;
  const details = source.cloneNode(true);
  details.hidden = false;
  const heading = details.querySelector("h2");
  if (heading) heading.id = "model-info-heading";
  body.replaceChildren(details);
  if (!dialog.open) dialog.showModal();
}

document.getElementById("model-info-dialog")?.addEventListener("click", (event) => {
  if (event.target.id === "model-info-dialog") event.target.close();
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
  const apply = () => {
    const query = input.value.trim().toLowerCase();
    let visible = 0;
    grid.querySelectorAll(".provider-tile").forEach((tile) => {
      const title = tile.querySelector(".provider-tile-title");
      const name = title ? title.textContent.toLowerCase() : "";
      const match = query === "" || name.includes(query);
      tile.hidden = !match;
      if (match) visible += 1;
    });
    if (empty) {
      empty.textContent = query ? "No providers match \u201c" + query + "\u201d." : "No providers match.";
      empty.hidden = visible !== 0;
    }
  };
  input.addEventListener("input", apply);
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") event.preventDefault();
  });
  apply();
}

filterProviderTiles();

// Combo steps suggest the selected connection's refreshed model IDs. Typed IDs
// stay valid when that connection has no list.
function bindModelSuggestions() {
  const select = document.getElementById("provider_id");
  const input = document.getElementById("model");
  if (!select || !input) return;
  const apply = () => {
    const list = document.getElementById("models-" + select.value);
    if (list && list.options.length > 0) {
      input.setAttribute("list", list.id);
    } else {
      input.removeAttribute("list");
    }
  };
  select.addEventListener("change", apply);
  apply();
}

bindModelSuggestions();

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
