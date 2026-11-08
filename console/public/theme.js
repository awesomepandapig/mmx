(function () {
  var saved = null;
  try {
    saved = localStorage.getItem("mmx-theme");
  } catch (_) {
    // Storage can be blocked, and the default is fine then.
  }
  document.documentElement.classList.toggle("dark", saved !== "light");
})();
