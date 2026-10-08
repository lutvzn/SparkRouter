// Applies the stored theme preference before first paint and wires the
// topbar toggle between light and dark. Without a manual preference the
// palette follows the OS through the prefers-color-scheme media query.
(function () {
  "use strict";

  var STORAGE_KEY = "sparkrouter-theme";
  var DARK_MEDIA = "(prefers-color-scheme: dark)";

  function readPreference() {
    try {
      var stored = window.localStorage.getItem(STORAGE_KEY);
    } catch (_) {
      return "system"; // Storage is unavailable (blocked or private mode).
    }
    return stored === "light" || stored === "dark" ? stored : "system";
  }

  function storePreference(preference) {
    try {
      window.localStorage.setItem(STORAGE_KEY, preference);
      return true;
    } catch (_) {
      return false; // Without storage the preference lasts for this page only.
    }
  }

  var preference = readPreference();
  // "system" keeps CSS on the prefers-color-scheme media query, so the
  // palette follows OS changes without a JavaScript listener.
  document.documentElement.dataset.theme = preference;

  function resolvedMode() {
    if (preference !== "system") return preference;
    return window.matchMedia(DARK_MEDIA).matches ? "dark" : "light";
  }

  function opposite(mode) {
    return mode === "dark" ? "light" : "dark";
  }

  function render(toggle) {
    var mode = resolvedMode();
    var next = opposite(mode);
    // The button keeps an inline icon plus a label span; only the label text
    // and the sprite reference change so textContent stays "Theme: Light" /
    // "Theme: Dark" and the icon is never clobbered. The icon always shows
    // the mode the toggle switches to, matching the title below.
    var label = toggle.querySelector(".btn-label");
    if (label) {
      label.textContent = "Theme: " + (mode === "dark" ? "Dark" : "Light");
    }
    var iconUse = toggle.querySelector("use");
    if (iconUse) {
      iconUse.setAttribute("href", "/static/icons.svg#" + (next === "dark" ? "moon" : "sun"));
    }
    toggle.title = "Switch to " + next + " mode";
    toggle.setAttribute("aria-label", toggle.title);
  }

  document.addEventListener("DOMContentLoaded", function () {
    var select = document.getElementById("settings-theme");
    var storageStatus = document.getElementById("theme-storage-status");
    if (select) {
      select.value = preference;
      select.disabled = false;
      select.addEventListener("change", function () {
        if (["system", "light", "dark"].indexOf(select.value) === -1) return;
        preference = select.value;
        var saved = storePreference(preference);
        document.documentElement.dataset.theme = preference;
        if (storageStatus) storageStatus.textContent = saved ? "Theme saved in this browser." : "Browser storage unavailable; theme applies to this page only.";
        if (toggle) render(toggle);
      });
    }
    var toggle = document.getElementById("theme-toggle");
    if (!toggle) return;
    toggle.addEventListener("click", function () {
      preference = opposite(resolvedMode());
      storePreference(preference);
      document.documentElement.dataset.theme = preference;
      render(toggle);
      if (select) select.value = preference;
    });
    // CSS already follows the OS while the preference is "system"; only
    // refresh the button label, never a stored explicit choice.
    window.matchMedia(DARK_MEDIA).addEventListener("change", function () {
      if (preference === "system") render(toggle);
    });
    render(toggle);
    toggle.hidden = false;
  });
})();
