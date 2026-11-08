import { src, type Media } from "./api";
import { icon } from "./icons";
import { esc, fmtDate, fmtDuration, fmtNum, groupLabel } from "./ui";

const PAGE = 240;
const GAP = 14;
const HEAD_H = 58;
const CAP_H = 64;
const OVERSCAN = 700;
const MAX_PAGES = 48;

type Row =
  | { t: "h"; top: number; h: number; start: number; n: 0; label: string; count: number }
  | { t: "r"; top: number; h: number; start: number; n: number };

export type GridHooks = {
  fetch(offset: number, limit: number): Promise<Media[]>;
  isSelected(id: number): boolean;
  isActive(id: number): boolean;
  onClick(i: number, item: Media, e: MouseEvent): void;
  onOpen(i: number, item: Media): void;
  onContext(i: number, item: Media, e: MouseEvent): void;
  onFavorite(item: Media): void;
  onToggleSelect(i: number, item: Media): void;
};

/**
 * Row-virtualised grid. Only the rows near the viewport exist in the DOM and item data is
 * fetched a page at a time, so libraries of 100k+ items scroll as smoothly as small ones.
 */
/** Set at start-up: may video tiles without a rendered preview show the video's own first frame? */
export const fallback = { video: false };

export class VirtualGrid {
  readonly root: HTMLElement;
  private sizer: HTMLElement;
  private layer: HTMLElement;
  total = 0;
  cols = 1;
  rowH = 0;
  private groups: { key: string; count: number }[] = [];
  private grouped = false;
  private tile = 190;
  private captions = true;
  private cw = 0;
  private rows: Row[] = [];
  private pages = new Map<number, Media[]>();
  private inflight = new Set<number>();
  private epoch = 0;
  private version = 0;
  private lastKey = "";
  private raf = 0;
  /** Live row elements keyed by row index, so scrolling only adds/removes rows instead of rebuilding them. */
  private live = new Map<number, { el: HTMLElement; v: number; sig: string; t: string; top: number; h: number }>();

  constructor(root: HTMLElement, private hooks: GridHooks) {
    this.root = root;
    root.innerHTML = `<div class="vg-sizer"><div class="vg-layer"></div></div>`;
    this.sizer = root.firstElementChild as HTMLElement;
    this.layer = this.sizer.firstElementChild as HTMLElement;
    root.addEventListener("scroll", () => this.schedule(), { passive: true });
    new ResizeObserver(() => this.relayout()).observe(root);
    root.addEventListener("click", (e) => this.click(e));
    root.addEventListener("dblclick", (e) => { const c = this.cardAt(e); if (c && !(e.target as HTMLElement).closest("[data-act]")) this.hooks.onOpen(c.i, c.item); });
    root.addEventListener("contextmenu", (e) => { const c = this.cardAt(e); if (c) { e.preventDefault(); this.hooks.onContext(c.i, c.item, e); } });
    // Inline onerror handlers are blocked by the CSP, so listen in the capture phase instead.
    root.addEventListener("error", (e) => {
      const img = e.target as HTMLElement;
      if (img.tagName !== "IMG") return;
      const thumb = img.parentElement!;
      img.remove();
      thumb.insertAdjacentHTML("afterbegin", `<div class="ph">${icon("image", 26)}<small>No preview</small></div>`);
    }, true);
  }

  // ------------------------------------------------------------ data
  setData(total: number, groups: { key: string; count: number }[], keepScroll = false) {
    const anchor = keepScroll ? this.root.scrollTop : 0;
    this.total = total;
    this.groups = groups;
    this.grouped = groups.length > 0;
    this.epoch++;
    this.inflight.clear();
    // On a soft refresh keep showing the old pages until fresh ones arrive (no skeleton flash).
    if (!keepScroll) { this.pages.clear(); this.live.clear(); this.layer.textContent = ""; }
    this.compute();
    this.root.scrollTop = Math.min(anchor, Math.max(0, this.sizer.offsetHeight - this.root.clientHeight));
    this.version++;
    this.render(true);
    if (keepScroll) void this.refreshLoaded();
  }

  /** Re-fetch every loaded page and swap them in together. */
  async refreshLoaded() {
    const epoch = this.epoch;
    // Only the pages around the viewport are re-fetched; every other cached page is dropped and
    // simply loads on demand. Refreshing all of them (up to MAX_PAGES x PAGE items) on each
    // library-changed event during indexing is what made long scrolling sessions lock up.
    const mid = Math.floor(this.firstVisibleIndex() / PAGE);
    const span = Math.ceil((this.root.clientHeight / Math.max(1, this.rowH)) * this.cols / PAGE) + 1;
    const keys = [...this.pages.keys()].filter((p) => p >= mid - 1 && p <= mid + span);
    for (const k of [...this.pages.keys()]) if (!keys.includes(k)) this.pages.delete(k);
    const res = await Promise.all(keys.map((p) => this.hooks.fetch(p * PAGE, PAGE).then((items) => [p, items] as const).catch(() => null)));
    if (epoch !== this.epoch) return;
    for (const r of res) if (r) this.pages.set(r[0], r[1]);
    this.version++;
    this.render(true);
  }

  /** Re-fetch pages that are on screen without throwing away the scroll position. */
  invalidatePages() {
    this.epoch++;
    this.pages.clear();
    this.inflight.clear();
    this.version++;
    this.render(true);
  }

  itemAt(i: number): Media | undefined { return this.pages.get(Math.floor(i / PAGE))?.[i % PAGE]; }

  async loadItem(i: number): Promise<Media | undefined> {
    if (i < 0 || i >= this.total) return undefined;
    const p = Math.floor(i / PAGE);
    if (!this.pages.has(p)) await this.fetchPage(p);
    return this.itemAt(i);
  }

  indexOf(id: number): number {
    for (const [p, items] of this.pages) { const k = items.findIndex((m) => m.id === id); if (k >= 0) return p * PAGE + k; }
    return -1;
  }

  patch(id: number, change: Partial<Media>) {
    for (const items of this.pages.values()) { const m = items.find((x) => x.id === id); if (m) { Object.assign(m, change); this.version++; this.render(true); return; } }
  }

  private async fetchPage(p: number): Promise<void> {
    if (this.inflight.has(p)) return;
    this.inflight.add(p);
    const epoch = this.epoch;
    try {
      const items = await this.hooks.fetch(p * PAGE, PAGE);
      if (epoch !== this.epoch) return;
      this.pages.set(p, items);
      if (this.pages.size > MAX_PAGES) {
        const mid = Math.floor(this.firstVisibleIndex() / PAGE);
        [...this.pages.keys()].sort((a, b) => Math.abs(b - mid) - Math.abs(a - mid)).slice(0, this.pages.size - MAX_PAGES).forEach((k) => this.pages.delete(k));
      }
      this.version++;
      this.schedule();
    } catch (e) { console.error("page fetch failed", e); }
    finally { if (epoch === this.epoch) this.inflight.delete(p); }
  }

  // ------------------------------------------------------------ layout
  setTile(px: number) { this.tile = px; this.relayout(); }
  setCaptions(on: boolean) { this.captions = on; this.relayout(); }
  setFit(contain: boolean) { this.root.classList.toggle("fit-contain", contain); }

  private relayout() {
    if (!this.root.clientWidth) return;
    const anchorIdx = this.firstVisibleIndex();
    this.compute();
    if (this.rows.length) this.scrollToIndex(anchorIdx, "start", false);
    this.render(true);
  }

  private compute() {
    const W = this.root.clientWidth - 12; // 6px gutters keep selection rings from being clipped
    this.cols = Math.max(2, Math.floor((W + GAP) / (this.tile + GAP)));
    this.cw = (W - (this.cols - 1) * GAP) / this.cols;
    const ih = Math.round(this.cw * 0.78);
    this.rowH = ih + (this.captions ? CAP_H : 0) + GAP;
    this.layer.style.setProperty("--cols", String(this.cols));
    this.layer.style.setProperty("--gap", `${GAP}px`);
    this.layer.style.setProperty("--ih", `${ih}px`);
    const rows: Row[] = [];
    let y = 0, idx = 0;
    const groups = this.grouped ? this.groups : [{ key: "", count: this.total }];
    for (const g of groups) {
      if (this.grouped) { rows.push({ t: "h", top: y, h: HEAD_H, start: idx, n: 0, label: groupLabel(g.key), count: g.count }); y += HEAD_H; }
      const nRows = Math.ceil(g.count / this.cols);
      for (let r = 0; r < nRows; r++) {
        const start = idx + r * this.cols;
        rows.push({ t: "r", top: y, h: this.rowH, start, n: Math.min(this.cols, g.count - r * this.cols) });
        y += this.rowH;
      }
      idx += g.count;
    }
    this.rows = rows;
    this.sizer.style.height = `${y + 8}px`;
  }

  private findRow(y: number): number {
    let lo = 0, hi = this.rows.length - 1;
    while (lo < hi) { const mid = (lo + hi + 1) >> 1; if (this.rows[mid].top <= y) lo = mid; else hi = mid - 1; }
    return lo;
  }

  private rowOfIndex(i: number): Row | undefined {
    let lo = 0, hi = this.rows.length - 1, best = -1;
    while (lo <= hi) { const mid = (lo + hi) >> 1; if (this.rows[mid].start <= i) { best = mid; lo = mid + 1; } else hi = mid - 1; }
    return best >= 0 ? this.rows[best] : undefined;
  }

  firstVisibleIndex(): number {
    if (!this.rows.length) return 0;
    const r = this.rows[this.findRow(this.root.scrollTop + 1)];
    return r.start;
  }

  scrollToIndex(i: number, mode: "nearest" | "start", smooth = false) {
    const row = this.rowOfIndex(Math.max(0, Math.min(i, this.total - 1)));
    if (!row) return;
    // Show the group header with its first row.
    let top = row.top;
    const prev = this.rows[this.rows.indexOf(row) - 1];
    if (prev && prev.t === "h") top = prev.top;
    const st = this.root.scrollTop, vh = this.root.clientHeight;
    let target = st;
    if (mode === "start") target = top;
    else if (row.top < st + 4) target = top;
    else if (row.top + row.h > st + vh) target = row.top + row.h - vh + 8;
    if (target !== st) this.root.scrollTo({ top: target, behavior: smooth ? "smooth" : "auto" });
  }

  // ------------------------------------------------------------ rendering
  private schedule() { if (!this.raf) this.raf = requestAnimationFrame(() => { this.raf = 0; this.render(); }); }
  rerender() { this.version++; this.render(true); }

  private render(force = false) {
    if (!this.rows.length) { this.layer.textContent = ""; this.live.clear(); this.lastKey = ""; return; }
    const st = this.root.scrollTop, vh = this.root.clientHeight;
    const lo = this.findRow(Math.max(0, st - OVERSCAN));
    const hi = this.findRow(st + vh + OVERSCAN);
    const key = `${lo}:${hi}:${this.version}:${this.cols}:${this.cw}`;
    if (!force && key === this.lastKey) return;
    this.lastKey = key;

    // Drop rows that scrolled out of range.
    for (const [k, v] of this.live) if (k < lo || k > hi) { v.el.remove(); this.live.delete(k); }

    let first = Infinity, last = -1;
    for (let k = lo; k <= hi; k++) {
      const r = this.rows[k];
      let inner: string;
      if (r.t === "h") inner = `<h3>${esc(r.label)}</h3><span>${fmtNum(r.count)} ${r.count === 1 ? "item" : "items"}</span>`;
      else {
        first = Math.min(first, r.start); last = Math.max(last, r.start + r.n - 1);
        // Rows already on screen are left untouched unless their data changed — re-creating <img> every
        // frame is what made scrolling stall.
        const cur = this.live.get(k);
        if (cur && cur.v === this.version && cur.top === r.top && cur.h === r.h) continue;
        inner = "";
        for (let c = 0; c < r.n; c++) inner += this.card(r.start + c);
      }
      let cur = this.live.get(k);
      if (cur && cur.t !== r.t) { cur.el.remove(); this.live.delete(k); cur = undefined; }
      if (cur) {
        if (cur.top !== r.top || cur.h !== r.h) { cur.el.style.top = `${r.top}px`; cur.el.style.height = `${r.h}px`; cur.top = r.top; cur.h = r.h; }
        if (cur.sig !== inner) { cur.el.innerHTML = inner; cur.sig = inner; }
        cur.v = this.version; continue;
      }
      const el = document.createElement("div");
      el.className = r.t === "h" ? "vg-head" : "vg-row";
      el.style.top = `${r.top}px`; el.style.height = `${r.h}px`;
      el.innerHTML = inner;
      this.layer.appendChild(el);
      this.live.set(k, { el, v: this.version, sig: inner, t: r.t, top: r.top, h: r.h });
    }
    if (last >= 0) for (let p = Math.floor(first / PAGE); p <= Math.floor(last / PAGE); p++) if (!this.pages.has(p)) void this.fetchPage(p);
  }

  private card(i: number): string {
    const m = this.itemAt(i);
    if (!m) return `<div class="card ghost"><div class="thumb"></div>${this.captions ? `<div class="cap"><span class="sk"></span><span class="sk s"></span></div>` : ""}</div>`;
    const sel = this.hooks.isSelected(m.id), act = this.hooks.isActive(m.id);
    let inner: string;
    if (m.thumbnail) inner = `<img src="${esc(src(m.thumbnail))}" alt="" decoding="async" draggable="false">`;
    else if (m.state === 0) inner = `<div class="ph busy"><span class="spin"></span><small>Preparing…</small></div>`;
    else if (m.kind === "image") inner = `<img class="orig" src="${esc(src(m.path))}" alt="" decoding="async" loading="lazy" draggable="false">`;
    else if (fallback.video) inner = `<video class="orig" src="${esc(src(m.path))}#t=0.4" preload="metadata" muted playsinline tabindex="-1"></video>`;
    else inner = `<div class="ph">${icon(m.kind === "video" ? "video" : "image", 26)}<small>No preview</small></div>`;
    const dur = m.kind === "video" ? `<span class="vchip">${icon("play", 9)}${m.duration != null ? fmtDuration(m.duration) : "Video"}</span>` : "";
    return `<div class="card ${sel ? "selected" : ""} ${act ? "active" : ""}" data-i="${i}" role="option" aria-selected="${sel}" aria-label="${esc(m.name)}">
      <div class="thumb">${inner}
        <button class="check" tabindex="-1" data-act="check" aria-label="${sel ? "Deselect" : "Select"} ${esc(m.name)}">${icon("check", 13)}</button>
        <button class="fav ${m.favorite ? "on" : ""}" tabindex="-1" data-act="fav" aria-label="${m.favorite ? "Remove from favorites" : "Add to favorites"}">${icon("heart", 15)}</button>
        ${dur}
      </div>
      ${this.captions ? `<div class="cap"><span class="cap-name" title="${esc(m.name)}">${esc(m.name)}</span><span class="cap-date">${fmtDate(m.date)}</span></div>` : ""}
    </div>`;
  }

  private cardAt(e: Event): { i: number; item: Media } | null {
    const el = (e.target as HTMLElement).closest<HTMLElement>(".card:not(.ghost)");
    if (!el) return null;
    const i = Number(el.dataset.i);
    const item = this.itemAt(i);
    return item ? { i, item } : null;
  }

  private click(e: MouseEvent) {
    const c = this.cardAt(e);
    if (!c) return;
    const act = (e.target as HTMLElement).closest<HTMLElement>("[data-act]")?.dataset.act;
    if (act === "fav") { e.stopPropagation(); this.hooks.onFavorite(c.item); return; }
    if (act === "check") { e.stopPropagation(); this.hooks.onToggleSelect(c.i, c.item); return; }
    this.hooks.onClick(c.i, c.item, e);
  }
}
