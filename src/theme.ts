export type ThemePref = "system" | "light" | "dark";
export const ACCENTS: { id: string; label: string; dark: string; light: string }[] = [
  { id: "sage", label: "Sage", dark: "#c9e7d2", light: "#2f6b4a" },
  { id: "ocean", label: "Ocean", dark: "#a9cdf2", light: "#2563a8" },
  { id: "amber", label: "Amber", dark: "#f0cf8a", light: "#a8660a" },
  { id: "rose", label: "Rose", dark: "#f0b6c4", light: "#b23a5a" },
  { id: "violet", label: "Violet", dark: "#cfc2f5", light: "#6a4bc2" },
];

const mq = window.matchMedia("(prefers-color-scheme: dark)");
let pref: ThemePref = "system";
let listeners: Array<(resolved: "light" | "dark") => void> = [];

const resolve = (): "light" | "dark" => (pref === "system" ? (mq.matches ? "dark" : "light") : pref);

function paint() {
  const r = document.documentElement;
  const t = resolve();
  r.dataset.theme = t;
  r.dataset.themePref = pref;
  r.style.colorScheme = t;
  listeners.forEach((l) => l(t));
}

export function applyTheme(next: ThemePref, accent: string) {
  pref = next;
  document.documentElement.dataset.accent = ACCENTS.some((a) => a.id === accent) ? accent : "sage";
  try { localStorage.setItem("gallerydesk.theme", next); localStorage.setItem("gallerydesk.accent", document.documentElement.dataset.accent!); } catch { /* storage unavailable */ }
  paint();
}

// Follow the OS when the preference is "system".
mq.addEventListener("change", () => { if (pref === "system") paint(); });

export const currentTheme = resolve;
export const themePref = () => pref;
export const onThemeChange = (cb: (resolved: "light" | "dark") => void) => { listeners.push(cb); };
