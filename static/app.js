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
