import { icon } from "./icons";

export const APP_NAME = "GalleryDesk - Design By Rashid";
export const $ = <T extends HTMLElement = HTMLElement>(sel: string, root: ParentNode = document) => root.querySelector<T>(sel)!;
export const $$ = <T extends HTMLElement = HTMLElement>(sel: string, root: ParentNode = document) => Array.from(root.querySelectorAll<T>(sel));

export const esc = (s: unknown) =>
  String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]!));

export const fmtNum = (n: number) => n.toLocaleString();
export function fmtBytes(n: number): string {
  if (n >= 1e9) return `${(n / 1e9).toFixed(n >= 1e10 ? 0 : 1)} GB`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(n >= 1e7 ? 0 : 1)} MB`;
  if (n >= 1e3) return `${Math.round(n / 1e3)} KB`;
  return `${n} B`;
}
export function fmtDuration(s: number | null): string {
  if (s == null) return "";
  const t = Math.round(s), h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), sec = t % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}` : `${m}:${String(sec).padStart(2, "0")}`;
}
/** Capture dates are stored as wall-clock-as-UTC, so they are formatted in UTC. */
export function fmtDate(secs: number, opts: Intl.DateTimeFormatOptions = { year: "numeric", month: "short", day: "numeric" }, utc = true): string {
  if (!secs) return "—";
  return new Date(secs * 1000).toLocaleDateString(undefined, { ...opts, ...(utc ? { timeZone: "UTC" } : {}) });
}
export function fmtDateTime(secs: number, utc: boolean): string {
  if (!secs) return "—";
  return new Date(secs * 1000).toLocaleString(undefined, { year: "numeric", month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", ...(utc ? { timeZone: "UTC" } : {}) });
}
export function groupLabel(key: string): string {
  const [y, m, d] = key.split("-").map(Number);
  if (d) return new Date(Date.UTC(y, m - 1, d)).toLocaleDateString(undefined, { weekday: "long", year: "numeric", month: "long", day: "numeric", timeZone: "UTC" });
  if (m) return new Date(Date.UTC(y, m - 1, 1)).toLocaleDateString(undefined, { year: "numeric", month: "long", timeZone: "UTC" });
  return String(y);
}
export const debounce = <A extends unknown[]>(fn: (...a: A) => void, ms: number) => {
  let t: number | undefined;
  return (...a: A) => { window.clearTimeout(t); t = window.setTimeout(() => fn(...a), ms); };
};

// ---------------------------------------------------------------- toast
let toastHost: HTMLElement | null = null;
export function toast(message: string, kind: "info" | "error" | "success" = "info", action?: { label: string; run: () => void }) {
  if (!toastHost) {
    toastHost = document.createElement("div");
    toastHost.className = "toasts";
    toastHost.setAttribute("role", "status");
    toastHost.setAttribute("aria-live", "polite");
    document.body.appendChild(toastHost);
  }
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.innerHTML = `<span>${esc(message)}</span>${action ? `<button class="toast-action">${esc(action.label)}</button>` : ""}`;
  action && el.querySelector("button")!.addEventListener("click", () => { action.run(); el.remove(); });
  toastHost.appendChild(el);
  requestAnimationFrame(() => el.classList.add("show"));
  const ttl = kind === "error" ? 7000 : action ? 6000 : 3200;
  window.setTimeout(() => { el.classList.remove("show"); window.setTimeout(() => el.remove(), 250); }, ttl);
  while (toastHost.children.length > 4) toastHost.firstElementChild!.remove();
}

// ---------------------------------------------------------------- overlays (menus + modals)
type Closer = { close: () => void; kind: "menu" | "modal" };
const stack: Closer[] = [];
/** Closes the top-most menu/modal. Returns true if something was closed. */
export function closeTopOverlay(): boolean {
  const top = stack[stack.length - 1];
  if (!top) return false;
  top.close();
  return true;
}
export const hasModal = () => stack.some((s) => s.kind === "modal");

export type MenuItem =
  | { label: string; icon?: string; run: () => void; danger?: boolean; disabled?: boolean; hint?: string; checked?: boolean }
  | { separator: true }
  | { header: string };

export function showMenu(x: number, y: number, items: MenuItem[], anchor?: HTMLElement) {
  closeMenus();
  const el = document.createElement("div");
  el.className = "menu";
  el.setAttribute("role", "menu");
  el.innerHTML = items.map((it, i) => {
    if ("separator" in it) return `<div class="menu-sep" role="separator"></div>`;
    if ("header" in it) return `<div class="menu-head">${esc(it.header)}</div>`;
    return `<button role="menuitem" class="menu-item ${it.danger ? "danger" : ""}" data-i="${i}" ${it.disabled ? "disabled" : ""}>
      <span class="menu-ic">${it.checked ? icon("check", 14) : it.icon ? icon(it.icon, 15) : ""}</span><span class="menu-label">${esc(it.label)}</span>${it.hint ? `<kbd>${esc(it.hint)}</kbd>` : ""}</button>`;
  }).join("");
  document.body.appendChild(el);
  const r = el.getBoundingClientRect();
  const left = Math.max(8, Math.min(x, window.innerWidth - r.width - 8));
  const top = y + r.height + 8 > window.innerHeight ? Math.max(8, (anchor ? anchor.getBoundingClientRect().top - r.height - 6 : y - r.height)) : y;
  el.style.left = `${left}px`; el.style.top = `${top}px`;
  const buttons = () => Array.from(el.querySelectorAll<HTMLButtonElement>(".menu-item:not([disabled])"));
  const close = () => { el.remove(); document.removeEventListener("pointerdown", outside, true); const i = stack.findIndex((s) => s.close === close); if (i >= 0) stack.splice(i, 1); anchor?.focus?.(); };
  const outside = (e: Event) => { if (!el.contains(e.target as Node)) close(); };
  stack.push({ close, kind: "menu" });
  document.addEventListener("pointerdown", outside, true);
  el.addEventListener("click", (e) => {
    const b = (e.target as HTMLElement).closest<HTMLButtonElement>(".menu-item");
    if (!b) return;
    const it = items[Number(b.dataset.i)] as Extract<MenuItem, { run: () => void }>;
    close(); it.run();
  });
  el.addEventListener("keydown", (e) => {
    const bs = buttons(); const i = bs.indexOf(document.activeElement as HTMLButtonElement);
    if (e.key === "ArrowDown") { e.preventDefault(); bs[(i + 1) % bs.length]?.focus(); }
    else if (e.key === "ArrowUp") { e.preventDefault(); bs[(i - 1 + bs.length) % bs.length]?.focus(); }
    else if (e.key === "Tab") { e.preventDefault(); close(); }
  });
  buttons()[0]?.focus({ preventScroll: true });
  return close;
}
export function closeMenus() { stack.filter((s) => s.kind === "menu").forEach((s) => s.close()); }

export function modal(html: string, opts: { cls?: string; label: string; onClose?: () => void }) {
  const back = document.createElement("div");
  back.className = "modal-back";
  back.innerHTML = `<div class="modal ${opts.cls ?? ""}" role="dialog" aria-modal="true" aria-label="${esc(opts.label)}">${html}</div>`;
  const prev = document.activeElement as HTMLElement | null;
  document.body.appendChild(back);
  const box = back.firstElementChild as HTMLElement;
  requestAnimationFrame(() => back.classList.add("show"));
  const close = () => {
    back.classList.remove("show");
    window.setTimeout(() => back.remove(), 140);
    const i = stack.findIndex((s) => s.close === close); if (i >= 0) stack.splice(i, 1);
    opts.onClose?.(); prev?.focus?.();
  };
  stack.push({ close, kind: "modal" });
  back.addEventListener("pointerdown", (e) => { if (e.target === back) close(); });
  box.addEventListener("keydown", (e) => {
    if (e.key !== "Tab") return;
    const f = Array.from(box.querySelectorAll<HTMLElement>('button:not([disabled]),input:not([disabled]),select,[tabindex]:not([tabindex="-1"])')).filter((n) => n.offsetParent !== null);
    if (!f.length) return;
    const first = f[0], last = f[f.length - 1];
    if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
    else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
  });
  (box.querySelector<HTMLElement>("[autofocus]") ?? box.querySelector<HTMLElement>("input,button.primary,button"))?.focus();
  return { el: box, close };
}

export function promptDialog(o: { title: string; label: string; value?: string; confirm: string; validate?: (v: string) => string | null }): Promise<string | null> {
  return new Promise((resolve) => {
    let done = false;
    const finish = (v: string | null) => { if (!done) { done = true; resolve(v); } };
    const m = modal(`
      <form class="dlg-body"><h2>${esc(o.title)}</h2>
        <label class="field"><span>${esc(o.label)}</span><input type="text" autofocus maxlength="80" value="${esc(o.value ?? "")}" spellcheck="false"></label>
        <div class="field-err" role="alert"></div>
        <div class="dlg-actions"><button type="button" class="btn" data-x>Cancel</button><button type="submit" class="btn primary">${esc(o.confirm)}</button></div>
      </form>`, { label: o.title, cls: "small", onClose: () => finish(null) });
    const input = m.el.querySelector("input")!; input.select();
    m.el.querySelector("[data-x]")!.addEventListener("click", () => m.close());
    m.el.querySelector("form")!.addEventListener("submit", (e) => {
      e.preventDefault();
      const v = input.value.trim();
      const err = !v ? "Please enter a name" : o.validate?.(v) ?? null;
      if (err) { m.el.querySelector(".field-err")!.textContent = err; input.focus(); return; }
      finish(v); m.close();
    });
  });
}

export function confirmDialog(o: { title: string; body: string; confirm: string; danger?: boolean }): Promise<boolean> {
  return new Promise((resolve) => {
    let done = false;
    const finish = (v: boolean) => { if (!done) { done = true; resolve(v); } };
    const m = modal(`
      <div class="dlg-body"><h2>${esc(o.title)}</h2><p class="dlg-text">${o.body}</p>
        <div class="dlg-actions"><button class="btn" data-x>Cancel</button><button class="btn ${o.danger ? "danger" : "primary"}" data-ok autofocus>${esc(o.confirm)}</button></div>
      </div>`, { label: o.title, cls: "small", onClose: () => finish(false) });
    m.el.querySelector("[data-x]")!.addEventListener("click", () => m.close());
    m.el.querySelector("[data-ok]")!.addEventListener("click", () => { finish(true); m.close(); });
  });
}

export const emptyState = (iconName: string, title: string, body: string, actions = "") =>
  `<div class="empty"><div class="empty-ic">${icon(iconName, 26)}</div><h2>${esc(title)}</h2><p>${body}</p>${actions}</div>`;
