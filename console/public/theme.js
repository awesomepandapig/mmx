(function () {
  var saved = null;
  try {
    saved = localStorage.getItem("mmx-theme");
  } catch (_) {
    // Storage can be blocked, and the default is fine then.
  }
  var dark = saved ? saved === "dark" : !matchMedia("(prefers-color-scheme: light)").matches;
  document.documentElement.classList.toggle("dark", dark);
})();
