// Test chat: non-streaming combo playground for the dashboard.
//
// Every dynamic string is inserted with textContent; server data is never
// treated as HTML. The gateway key lives only in the password field for the
// lifetime of the page and is never written to storage.
(function () {
  "use strict";

  const form = document.getElementById("test-form");
  const modeSelect = document.getElementById("test-mode");
  const evaluation = document.getElementById("test-evaluation");
  const stateInput = document.getElementById("test-state");
  const questionsInput = document.getElementById("test-questions");
  const welcomeText = document.querySelector(".test-log-empty p");
  function isEvaluation() { return modeSelect && modeSelect.value === "systemone"; }
  const mediaPanel = document.getElementById("test-media");
  const mediaHint = document.getElementById("test-media-hint");
  const uploadControls = document.getElementById("test-upload-controls");
  const fileInput = document.getElementById("test-file");
  const maskControl = document.getElementById("test-mask-control");
  const maskInput = document.getElementById("test-mask");
  const voiceControl = document.getElementById("test-voice-control");
  const voiceInput = document.getElementById("test-voice");
  const mediaOptions = document.getElementById("test-media-options");
  const MEDIA = {
    image: { label: "Image", hint: "Enter an image prompt. For inline previews, request response_format: b64_json where supported." },
    "image-edit": { label: "Image edit", upload: true, hint: "Upload an image, optionally a mask, and enter an edit prompt. OpenAI-compatible image editing only." },
    video: { label: "Video", hint: "Enter a video prompt. Native video jobs are polled inline and may take several minutes. Closing the page does not guarantee job cancellation." },
    music: { label: "Music", hint: "Enter a music prompt, or supply lyrics/tags/composition_plan in advanced options. Account access and supported fields vary by provider." },
    speech: { label: "Speech", hint: "Enter speech text and a provider-specific voice. OpenAI/MiniMax require a voice; ElevenLabs uses its voice ID; Deepgram selects the voice by model. PCM may require downloading rather than browser playback." },
    transcription: { label: "Transcription", upload: true, hint: "Upload audio/video for transcription. The entire multipart request must fit 32 MiB. Set language or response_format in advanced options if needed." },
    translation: { label: "Audio translation", upload: true, hint: "Upload audio/video for translation to English using an OpenAI-compatible provider. The entire multipart request must fit 32 MiB." },
  };
  function mediaMode() { return modeSelect ? MEDIA[modeSelect.value] : null; }
  function hasInput() {
    if (!comboSelect.value) return false;
    if (isEvaluation()) return stateInput.value.trim() !== "" && questionsInput.value.trim() !== "";
    const media = mediaMode();
    if (media && media.upload && !(fileInput && fileInput.files && fileInput.files[0] && fileInput.files[0].size > 0)) return false;
    if (media && media.upload && modeSelect.value !== "image-edit") return true;
    if (media && modeSelect.value === "music" && mediaOptions && mediaOptions.value.trim() !== "{}") return true;
    return draft.value.trim() !== "";
  }
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
  function newSessionId() {
    return Array.from(crypto.getRandomValues(new Uint32Array(4)),
      (value) => value.toString(16).padStart(8, "0")).join("");
  }
  let conversation = [];
  let sessionId = newSessionId();
  let busy = false;
  let requestVersion = 0;
  let pendingRequest = null;
  let objectUrls = [];
  let players = [];

  function setNote(node, text) {
    node.textContent = text;
    node.hidden = !text;
  }

  function updateSendEnabled() {
    sendButton.disabled = busy || !hasInput();
  }

  function setBusy(value) {
    busy = value;
    sendButton.disabled = value || !hasInput();
    if (modeSelect) modeSelect.disabled = value;
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
    author.textContent = role === "user" ? "You" : (isEvaluation() ? "SystemOne result" : "Assistant");
    const body = document.createElement(isEvaluation() ? "pre" : "p");
    body.className = "test-msg-body";
    body.textContent = text;
    article.append(author, body);
    log.append(article);
    logEmpty.hidden = true;
  }

  function clearMessages() {
    for (const player of players) {
      player.pause();
      player.removeAttribute("src");
      player.load();
    }
    players = [];
    for (const url of objectUrls) URL.revokeObjectURL(url);
    objectUrls = [];
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
    return "The request failed. Inputs were kept for an explicit retry.";
  }

  function mediaPayload(text) {
    let extra;
    try {
      extra = JSON.parse(mediaOptions.value);
      if (!extra || typeof extra !== "object" || Array.isArray(extra)) throw new Error();
    } catch (_) { throw new Error("Advanced media options must be a JSON object."); }
    const mode = modeSelect.value;
    const config = MEDIA[mode];
    // Never let advanced fields change the target, add files, or leak CSRF.
    const reserved = ["model", "_csrf"].concat(config.upload ? ["file", "image", "image[]", "mask"] : []);
    for (const key of reserved) delete extra[key];
    extra.model = "combo/" + comboSelect.value;
    if (mode === "speech") {
      extra.input = text;
      if (voiceInput.value.trim()) extra.voice = voiceInput.value.trim();
    } else if (["image", "image-edit", "video", "music"].includes(mode) && text) extra.prompt = text;
    if (!config.upload) return { path: "/test/media/" + mode, body: JSON.stringify(extra), multipart: false };
    const file = fileInput.files[0];
    const mask = mode === "image-edit" && maskInput.files ? maskInput.files[0] : null;
    if (file.size + (mask ? mask.size : 0) >= 32 * 1024 * 1024) throw new Error("Files plus multipart framing must fit 32 MiB. Choose smaller files.");
    const data = new FormData();
    for (const [key, value] of Object.entries(extra)) {
      const values = key.endsWith("[]") && Array.isArray(value) ? value : [value];
      for (const item of values) data.append(key, typeof item === "string" ? item : JSON.stringify(item));
    }
    data.append(mode === "image-edit" ? "image" : "file", file);
    if (mask) data.append("mask", mask);
    return { path: "/test/media-upload/" + mode, body: data, multipart: true };
  }

  function mediaTrace(response) {
    try {
      const header = response.headers.get("x-sparkrouter-attempts");
      if (!header) {
        attemptsEmpty.textContent = "No routing diagnostics available for this response.";
        return;
      }
      const trace = JSON.parse(new TextDecoder().decode(Uint8Array.from(atob(header), c => c.charCodeAt(0))));
      renderAttempts(trace.attempts);
      if (trace.truncated) {
        attemptsEmpty.hidden = false;
        attemptsEmpty.textContent = "Only the first attempts fit the diagnostic header. Full metadata is in History when logging is enabled.";
      }
    } catch (_) {
      attemptsEmpty.textContent = "Routing diagnostics could not be decoded.";
      // Missing/bad diagnostics never hide an otherwise valid output.
    }
  }

  function blobLink(parent, blob, filename, image) {
    const url = URL.createObjectURL(blob);
    objectUrls.push(url);
    if (image) {
      const preview = document.createElement("img");
      preview.src = url;
      preview.alt = "Generated image";
      preview.className = "test-media-preview";
      parent.append(preview);
    } else {
      const player = document.createElement("audio");
      player.controls = true;
      player.preload = "metadata";
      player.src = url;
      player.className = "test-media-player";
      players.push(player);
      parent.append(player);
    }
    const link = document.createElement("a");
    link.href = url;
    link.download = filename;
    link.textContent = "Download " + filename;
    link.className = "btn";
    parent.append(link);
  }

  function renderMedia(value, blob, text) {
    const article = document.createElement("article");
    article.className = "test-msg test-msg-assistant";
    const title = document.createElement("p");
    title.className = "test-msg-author";
    title.textContent = MEDIA[modeSelect.value].label + " result";
    article.append(title);
    if (blob) {
      const type = blob.type.split(";")[0];
      const extension = ({ "audio/mpeg": "mp3", "audio/wav": "wav", "audio/x-wav": "wav", "audio/flac": "flac", "audio/ogg": "ogg", "audio/opus": "opus", "audio/aac": "aac" })[type] || "bin";
      blobLink(article, blob, "audio." + extension, false);
    } else {
      const body = document.createElement("pre");
      body.className = "test-msg-body";
      body.textContent = text !== null ? text : JSON.stringify(value, (key, item) => key === "b64_json" && typeof item === "string" ? "[base64 image; preview/download below]" : item, 2);
      article.append(body);
      if (value && Array.isArray(value.data)) {
        for (const item of value.data) {
          if (!item || typeof item !== "object") continue;
          if (typeof item.b64_json === "string") {
            try {
              const bytes = Uint8Array.from(atob(item.b64_json), c => c.charCodeAt(0));
              const signature = item.b64_json;
              const [mime, extension] = signature.startsWith("/9j/") ? ["image/jpeg", "jpg"] : signature.startsWith("R0lGOD") ? ["image/gif", "gif"] : signature.startsWith("UklGR") ? ["image/webp", "webp"] : ["image/png", "png"];
              blobLink(article, new Blob([bytes], { type: mime }), "image." + extension, true);
            } catch (_) {
              const note = document.createElement("p");
              note.textContent = "Returned image could not be decoded for preview.";
              article.append(note);
            }
          }
          if (typeof item.url === "string") {
            try {
              const url = new URL(item.url);
              if (!["https:", "http:"].includes(url.protocol) || url.username || url.password) continue;
              const link = document.createElement("a");
              link.href = url.href;
              link.target = "_blank";
              link.rel = "noopener noreferrer";
              link.referrerPolicy = "no-referrer";
              link.className = "btn";
              link.textContent = "Open remote media";
              article.append(link);
            } catch (_) { /* Never create links from unsafe or relative URLs. */ }
          }
        }
      }
    }
    log.append(article);
    logEmpty.hidden = true;
  }

  async function send() {
    // Capture the raw draft as typed: the field stays editable while the
    // request is in flight, so only this snapshot is sent and only an
    // unchanged field is cleared afterwards.
    const originalDraft = draft.value;
    const text = originalDraft.trim();
    if (!hasInput() || busy) return;
    const evaluating = isEvaluation();
    const media = mediaMode();
    let mediaRequest;
    let payload;
    if (media) {
      try { mediaRequest = mediaPayload(text); }
      catch (error) {
        clearAttempts();
        setNote(statusNote, "");
        setNote(errorNote, error.message);
        return;
      }
    } else if (evaluating) {
      try {
        const state = JSON.parse(stateInput.value);
        const questions = JSON.parse(questionsInput.value);
        if (state === null || !["string", "object"].includes(typeof state)) throw new Error();
        if (questions === null || typeof questions !== "object" || Array.isArray(questions)) throw new Error();
        payload = { _csrf: csrfInput.value, model: "combo/" + comboSelect.value, state, questions };
      } catch (_) {
        clearAttempts();
        setNote(statusNote, "");
        setNote(errorNote, "Enter valid JSON: state must be a string, object, or array; questions must be an object.");
        return;
      }
    } else {
      payload = {
        _csrf: csrfInput.value,
        model: "combo/" + comboSelect.value,
        messages: conversation.concat([{ role: "user", content: text }]),
      };
    }
    const version = ++requestVersion;
    const controller = new AbortController();
    pendingRequest = controller;

    const headers = {
      "Content-Type": "application/json",
      "X-SparkRouter-Session-Id": sessionId,
    };
    if (media) {
      headers["X-SparkRouter-CSRF"] = csrfInput.value;
      if (mediaRequest.multipart) delete headers["Content-Type"];
    }
    if (keyInput) {
      const key = keyInput.value.trim();
      if (key) headers.Authorization = "Bearer " + key;
    }
    setNote(errorNote, "");
    setNote(statusNote, media ? "Generating / processing… This may take several minutes." : "Sending\u2026");
    // Drop the previous trace immediately: a failure that never reaches
    // routing (auth, CSRF, network) must not leave an old trace on screen
    // as if it belonged to this request.
    clearAttempts();
    setBusy(true);
    let data = null;
    try {
      const response = await fetch(media ? mediaRequest.path : evaluating ? "/test/systemone" : "/test/chat", {
        method: "POST",
        credentials: "same-origin",
        headers,
        body: media ? mediaRequest.body : JSON.stringify(payload),
        signal: controller.signal,
      });
      if (version !== requestVersion) return;
      if (media) {
        mediaTrace(response);
        const type = (response.headers.get("content-type") || "").toLowerCase();
        let blob = null;
        let resultText = null;
        if (response.ok && (type.startsWith("audio/") || type.startsWith("application/octet-stream"))) blob = await response.blob();
        else if (response.ok && (type.startsWith("text/") || type.startsWith("application/x-subrip"))) resultText = await response.text();
        else {
          try { data = await response.json(); } catch (_) { data = null; }
        }
        if (version !== requestVersion) return;
        if (response.ok && (blob || resultText !== null || data)) {
          renderMedia(data, blob, resultText);
          setNote(statusNote, "Response received. Playback depends on browser codec support; files can be downloaded.");
          log.lastElementChild.scrollIntoView({ block: "nearest" });
        } else {
          setNote(errorNote, failureMessage(data, response.status));
          setNote(statusNote, "");
        }
        return;
      }
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
        const reply = evaluating ? JSON.stringify(data.completion, null, 2) : replyText(data.completion);
        if (reply === null) {
          setNote(
            errorNote,
            "The combo returned no message text. Tool calls and other non-text completions are not supported here; the message was kept for retry.",
          );
          setNote(statusNote, "");
        } else {
          if (!evaluating) addMessage("user", text);
          addMessage("assistant", reply);
          if (!evaluating) {
            conversation.push({ role: "user", content: text });
            conversation.push({ role: "assistant", content: reply });
            if (draft.value === originalDraft) draft.value = "";
          }
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
      setNote(errorNote, "Unable to reach the gateway or finish the response. The request may already have been billed; retry only deliberately.");
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

  if (stateInput) stateInput.addEventListener("input", updateSendEnabled);
  if (questionsInput) questionsInput.addEventListener("input", updateSendEnabled);
  if (fileInput) fileInput.addEventListener("change", updateSendEnabled);
  if (mediaOptions) mediaOptions.addEventListener("input", updateSendEnabled);
  function filterCombos() {
    if (!comboSelect.options) return;
    const category = ({ chat: "text", systemone: "decision", "image-edit": "image", speech: "tts", transcription: "stt", translation: "stt" })[modeSelect.value] || modeSelect.value;
    const options = Array.from(comboSelect.options);
    for (const option of options) {
      option.hidden = option.dataset.category !== category;
      option.disabled = option.hidden;
    }
    if (!options.some(option => !option.disabled && option.value === comboSelect.value)) {
      const first = options.find(option => !option.disabled);
      comboSelect.value = first ? first.value : "";
    }
  }
  function updateMode() {
    filterCombos();
    const media = mediaMode();
    const upload = Boolean(media && media.upload);
    if (mediaPanel) mediaPanel.hidden = !media;
    if (mediaHint) mediaHint.textContent = media ? media.hint : "";
    if (uploadControls) uploadControls.hidden = !upload;
    if (maskControl) maskControl.hidden = !media || modeSelect.value !== "image-edit";
    if (voiceControl) voiceControl.hidden = !media || modeSelect.value !== "speech";
    if (fileInput) fileInput.accept = modeSelect.value === "image-edit" ? "image/*" : "audio/*,video/*";
    draft.placeholder = media ? modeSelect.value === "speech" ? "Text to speak…" : "Generation / edit prompt…" : "Message your combo…";
    const evaluating = isEvaluation();
    if (evaluation) evaluation.hidden = !evaluating;
    draft.hidden = evaluating || (upload && modeSelect.value !== "image-edit");
    if (modeSelect) {
      sendButton.textContent = media ? upload ? "Process" : "Generate" : evaluating ? "Evaluate" : "Send ↑";
      resetButton.textContent = media || evaluating ? "Clear results" : "New chat";
      if (welcomeText) welcomeText.textContent = !comboSelect.value ? "No combo in this category. Create a matching combo in the Combos menu." : media ? media.hint : evaluating ? "Enter state and questions below, then evaluate with a SystemOne combo." : "Send a message to your combo to start a conversation.";
    }
    updateSendEnabled();
  }
  if (modeSelect) modeSelect.addEventListener("change", () => {
    if (fileInput) fileInput.value = "";
    if (maskInput) maskInput.value = "";
    if (mediaOptions) mediaOptions.value = "{}";
    conversation = [];
    sessionId = newSessionId();
    clearMessages();
    clearAttempts();
    setNote(errorNote, "");
    setNote(statusNote, "");
    updateMode();
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
    filterCombos();
    updateSendEnabled();
    conversation = [];
    sessionId = newSessionId();
    clearMessages();
    clearAttempts();
    setNote(errorNote, "");
    setNote(statusNote, "");
  });

  resetButton.addEventListener("click", () => {
    conversation = [];
    sessionId = newSessionId();
    clearMessages();
    clearAttempts();
    draft.value = "";
    setNote(errorNote, "");
    setNote(statusNote, "");
    updateSendEnabled();
    if (fileInput) fileInput.value = "";
    if (maskInput) maskInput.value = "";
    if (isEvaluation()) stateInput.focus();
    else if (mediaMode() && mediaMode().upload) fileInput.focus();
    else draft.focus();
    updateSendEnabled();
  });

  // Leaving the page (including a BFCache save) must discard the entered
  // gateway key and the page-local chat, so a restored tab shows none of it.
  window.addEventListener("pagehide", () => {
    requestVersion += 1;
    if (pendingRequest) pendingRequest.abort();
    pendingRequest = null;
    setBusy(false);
    if (keyInput) keyInput.value = "";
    if (stateInput) stateInput.value = "";
    if (questionsInput) questionsInput.value = "";
    if (fileInput) fileInput.value = "";
    if (maskInput) maskInput.value = "";
    if (voiceInput) voiceInput.value = "";
    if (mediaOptions) mediaOptions.value = "{}";
    conversation = [];
    sessionId = newSessionId();
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

  updateMode();
})();
