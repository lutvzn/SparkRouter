// Test chat: non-streaming combo playground for the dashboard.
//
// Every dynamic string is inserted with textContent; server data is never
// treated as HTML. The gateway key lives only in the password field for the
// lifetime of the page and is never written to storage.
(function () {
  "use strict";

  const form = document.getElementById("test-form");
  const csrfInput = document.getElementById("test-csrf");
  const comboSelect = document.getElementById("test-combo");
  const keyInput = document.getElementById("test-key"); // absent unless auth is required
  const draft = document.getElementById("test-draft");
  const sendButton = document.getElementById("test-send");
  const resetButton = document.getElementById("test-reset");
  const statusNote = document.getElementById("test-status");
  const errorNote = document.getElementById("test-error");
  const log = document.getElementById("test-log");
  const logEmpty = log ? log.querySelector(".test-log-empty") : null;
  const attemptsEmpty = document.querySelector(".test-attempts-empty");
  const attemptList = document.getElementById("test-attempt-list");
  if (
    !form ||
    !csrfInput ||
    !comboSelect ||
    !draft ||
    !sendButton ||
    !resetButton ||
    !statusNote ||
    !errorNote ||
    !log ||
    !logEmpty ||
    !attemptsEmpty ||
    !attemptList
  ) {
    return;
  }

  // Only successful exchanges enter this history; the next request replays it
  // in full so multi-turn context is preserved upstream. Failures must never
  // pollute it, so their drafts stay in the textarea for retry.
  let conversation = [];
  let busy = false;
  let requestVersion = 0;
  let pendingRequest = null;

  function setNote(node, text) {
    node.textContent = text;
    node.hidden = !text;
  }

  function updateSendEnabled() {
    sendButton.disabled = busy || draft.value.trim() === "";
  }

  function setBusy(value) {
    busy = value;
    sendButton.disabled = value || draft.value.trim() === "";
    resetButton.disabled = value;
    comboSelect.disabled = value;
    if (keyInput) keyInput.disabled = value;
    // The message field stays editable during a request, but a failing outcome
    // keeps whatever is typed there for an explicit retry.
    if (value) {
      log.setAttribute("aria-busy", "true");
    } else {
      log.removeAttribute("aria-busy");
    }
  }

  function addMessage(role, text) {
    const article = document.createElement("article");
    article.className = "test-msg test-msg-" + role;
    const author = document.createElement("p");
    author.className = "test-msg-author";
    author.textContent = role === "user" ? "You" : "Assistant";
    const body = document.createElement("p");
    body.className = "test-msg-body";
    body.textContent = text;
    article.append(author, body);
    log.append(article);
    logEmpty.hidden = true;
  }

  function clearMessages() {
    log.querySelectorAll(".test-msg").forEach((message) => message.remove());
    logEmpty.hidden = false;
  }

  // Outcome values come from the gateway trace. The map only picks a badge
  // color; any unknown value still renders as plain, harmless text.
  const OUTCOME_BADGE = {
    selected: "badge ok",
    retryable_failure: "badge",
    permanent_failure: "badge fail",
    timed_out: "badge fail",
  };

  function attemptText(value, fallback) {
    return typeof value === "string" && value ? value : fallback;
  }

  function renderAttempts(attempts) {
    attemptList.replaceChildren();
    let count = 0;
    if (Array.isArray(attempts)) {
      for (const attempt of attempts) {
        const item = document.createElement("li");
        item.className = "test-attempt";

        const step = document.createElement("span");
        step.className = "test-attempt-step";
        step.textContent =
          "Step " +
          (typeof attempt.position === "number"
            ? attempt.position
            : count + 1);

        const provider = document.createElement("span");
        provider.className = "test-attempt-provider";
        provider.textContent = attemptText(
          attempt.provider_name,
          "unknown connection",
        );

        const model = document.createElement("span");
        model.className = "test-attempt-model";
        model.textContent = attemptText(attempt.model, "unknown model");

        const outcome = document.createElement("span");
        const outcomeText = attemptText(attempt.outcome, "unknown");
        outcome.className = OUTCOME_BADGE[outcomeText] || "badge";
        outcome.textContent = outcomeText.replace(/_/g, " ");

        const latency = document.createElement("span");
        latency.className = "test-attempt-latency";
        if (typeof attempt.latency_ms === "number") {
          latency.textContent = attempt.latency_ms + " ms";
        }

        item.append(step, provider, model, outcome, latency);
        attemptList.append(item);
        count += 1;
      }
    }
    attemptList.hidden = count === 0;
    attemptsEmpty.hidden = count !== 0;
    attemptsEmpty.textContent =
      count === 0 ? "The request did not reach any combo step." : "";
  }

  function clearAttempts() {
    attemptList.replaceChildren();
    attemptList.hidden = true;
    attemptsEmpty.textContent = "No response yet.";
    attemptsEmpty.hidden = false;
  }

  // A successful completion may still carry no textual answer (tool calls or
  // malformed output are unsupported here); callers then keep the draft and
  // conversation untouched instead of recording a placeholder reply.
  function replyText(completion) {
    const choice = completion && completion.choices ? completion.choices[0] : null;
    const content = choice && choice.message ? choice.message.content : null;
    return typeof content === "string" && content.trim() !== "" ? content : null;
  }

  function failureMessage(data, httpStatus) {
    if (
      data &&
      data.error &&
      typeof data.error.message === "string" &&
      data.error.message
    ) {
      return data.error.message;
    }
    if (httpStatus === 401) {
      return "The gateway rejected the credentials. Check the gateway key and try again.";
    }
    return "The request failed. The message was kept for retry.";
  }

  async function send() {
    // Capture the raw draft as typed: the field stays editable while the
    // request is in flight, so only this snapshot is sent and only an
    // unchanged field is cleared afterwards.
    const originalDraft = draft.value;
    const text = originalDraft.trim();
    if (!text || busy) return;
    const version = ++requestVersion;
    const controller = new AbortController();
    pendingRequest = controller;

    const headers = { "Content-Type": "application/json" };
    if (keyInput) {
      const key = keyInput.value.trim();
      if (key) headers.Authorization = "Bearer " + key;
    }
    const payload = {
      _csrf: csrfInput.value,
      model: "combo/" + comboSelect.value,
      messages: conversation.concat([{ role: "user", content: text }]),
    };

    setNote(errorNote, "");
    setNote(statusNote, "Sending\u2026");
    // Drop the previous trace immediately: a failure that never reaches
    // routing (auth, CSRF, network) must not leave an old trace on screen
    // as if it belonged to this request.
    clearAttempts();
    setBusy(true);
    let data = null;
    try {
      const response = await fetch("/test/chat", {
        method: "POST",
        credentials: "same-origin",
        headers,
        body: JSON.stringify(payload),
        signal: controller.signal,
      });
      try {
        data = await response.json();
      } catch (_) {
        data = null; // Non-JSON replies fall through to the generic message.
      }
      // A response from before navigation must not repopulate a restored page
      // or interfere with a newer request, even if abort arrived too late.
      if (version !== requestVersion) return;
      if (data && Array.isArray(data.attempts)) {
        renderAttempts(data.attempts);
      }
      if (response.ok && data && data.completion && !data.error) {
        const reply = replyText(data.completion);
        if (reply === null) {
          setNote(
            errorNote,
            "The combo returned no message text. Tool calls and other non-text completions are not supported here; the message was kept for retry.",
          );
          setNote(statusNote, "");
        } else {
          addMessage("user", text);
          addMessage("assistant", reply);
          conversation.push({ role: "user", content: text });
          conversation.push({ role: "assistant", content: reply });
          if (draft.value === originalDraft) draft.value = "";
          setNote(statusNote, "Response received.");
          log.lastElementChild.scrollIntoView({ block: "nearest" });
        }
      } else {
        setNote(errorNote, failureMessage(data, response.status));
        setNote(statusNote, "");
      }
    } catch (_) {
      if (version !== requestVersion) return;
      // The connection failed or was lost; the gateway may already have
      // processed and billed the request, so never assume it was not seen.
      setNote(errorNote, "Unable to reach the gateway. Check the connection and try again.");
      setNote(statusNote, "");
    } finally {
      if (version === requestVersion) {
        pendingRequest = null;
        setBusy(false);
        updateSendEnabled();
      }
    }
  }

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    send();
  });

  draft.addEventListener("input", updateSendEnabled);
  draft.addEventListener("keydown", (event) => {
    // Do not submit an IME composition or steal modified keyboard shortcuts.
    if (event.key === "Enter" && !event.isComposing) {
      event.preventDefault();
      if (!event.shiftKey && !event.ctrlKey && !event.altKey && !event.metaKey) {
        send();
      }
    }
  });

  // A different combo is a different conversation: never mix messages or
  // traces across targets. The draft stays so the same question can be retried
  // against the new combo.
  comboSelect.addEventListener("change", () => {
    conversation = [];
    clearMessages();
    clearAttempts();
    setNote(errorNote, "");
    setNote(statusNote, "");
  });

  resetButton.addEventListener("click", () => {
    conversation = [];
    clearMessages();
    clearAttempts();
    draft.value = "";
    setNote(errorNote, "");
    setNote(statusNote, "");
    updateSendEnabled();
    draft.focus();
  });

  // Leaving the page (including a BFCache save) must discard the entered
  // gateway key and the page-local chat, so a restored tab shows none of it.
  window.addEventListener("pagehide", () => {
    requestVersion += 1;
    if (pendingRequest) pendingRequest.abort();
    pendingRequest = null;
    setBusy(false);
    if (keyInput) keyInput.value = "";
    conversation = [];
    clearMessages();
    clearAttempts();
    draft.value = "";
    setNote(errorNote, "");
    setNote(statusNote, "");
    updateSendEnabled();
  });

  // A restored (BFCache) page may never see its in-flight fetch settle, so
  // the busy lock from the hidden period must not survive the restore.
  window.addEventListener("pageshow", (event) => {
    if (event.persisted) {
      setBusy(false);
      updateSendEnabled();
    }
  });

  updateSendEnabled();
})();
