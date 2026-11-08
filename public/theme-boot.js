// Runs before first paint (external file because the app CSP forbids inline scripts):
// applies the saved theme so there is no light/dark flash on launch.
(function () {
  try {
    var pref = localStorage.getItem("gallerydesk.theme") || localStorage.getItem("luma.theme") || "system";
    var accent = localStorage.getItem("gallerydesk.accent") || localStorage.getItem("luma.accent") || "sage";
    var dark = pref === "dark" || (pref === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
    var r = document.documentElement;
    r.dataset.theme = dark ? "dark" : "light";
    r.dataset.themePref = pref;
    r.dataset.accent = accent;
    r.style.colorScheme = dark ? "dark" : "light";
  } catch (e) { document.documentElement.dataset.theme = "dark"; }
})();
