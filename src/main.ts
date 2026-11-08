import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import "./style.css";
import { api, on, errText, src, type Detail, type DupGroup, type Filter, type Folder, type Group, type Media, type Named, type Progress, type Settings, type Sort, type Stats } from "./api";
import { icon } from "./icons";
import { VirtualGrid, fallback } from "./grid";
import { Viewer } from "./viewer";
import { ACCENTS, applyTheme, themePref, type ThemePref } from "./theme";
import { APP_NAME, $, $$, closeMenus, closeTopOverlay, confirmDialog, debounce, emptyState, esc, fmtBytes, fmtDate, fmtDateTime, fmtDuration, fmtNum, hasModal, modal, promptDialog, showMenu, toast, type MenuItem } from "./ui";

// ------------------------------------------------------------------ state
type Nav =
  | { type: "all" | "photos" | "videos" | "favorites" | "timeline" | "duplicates" }
  | { type: "folder"; id: number; dir?: string }
  | { type: "album"; id: number }
  | { type: "tag"; tag: string }
  | { type: "smart"; smart: "recent" | "untagged" | "gps" };

let nav: Nav = { type: "all" };
let kind: Filter["kind"] = "all";
let search = "";
let sort: Sort = { key: "date", desc: true };
let groupBy: Group = "none";
let S: Settings = {};
let folders: Folder[] = [];
let albums: Named[] = [];
let tags: Named[] = [];
let stats: Stats = { total: 0, images: 0, videos: 0, favorites: 0, bytes: 0, pending: 0, failed: 0 };
let dirsByFolder = new Map<number, { dir: string; count: number }[]>();
let expanded = new Set<number>();
let sel = new Set<number>();
let activeId: number | null = null;
let anchor = 0;
let cursor = 0;
let scan: Progress = { running: false, cancelled: false, phase: "idle", folder: "", discovered: 0, processed: 0, total: 0, failed: 0 };
let inspectorOpen = true;
let layoutSig = "";
let layoutTotal = 0;
let dupes: DupGroup[] | null = null;
let tile = 190;
let dupeRun: { done: number; total: number } | null = null;

const store = {
  get: (k: string, d: string) => { try { return localStorage.getItem(k) ?? d; } catch { return d; } },
  set: (k: string, v: string) => { try { localStorage.setItem(k, v); } catch { /* ignore */ } },
};
const collapsed = new Set<string>(JSON.parse(store.get("gallerydesk.collapsed", "[]")));

// ------------------------------------------------------------------ shell
const app = $("#app");
app.innerHTML = `
<div class="shell" id="shell">
  <aside class="sidebar" aria-label="Library">
    <div class="brand"><img class="brand-mark" src="/icon.svg" alt="" width="32" height="32" draggable="false"><span class="brand-txt">GalleryDesk - Design By Rashid</span></div>
    <nav class="side-scroll" id="side-nav" aria-label="Views"></nav>
    <div class="side-foot" id="side-foot"></div>
  </aside>
  <main class="main">
    <header class="topbar">
      <div class="titles"><div class="eyebrow" id="eyebrow">YOUR PERSONAL LIBRARY</div><h1 id="page-title">All media</h1><div class="subtitle" id="subtitle"></div></div>
      <div class="top-actions">
        <label class="search-wrap">${icon("search", 16)}<input id="search" type="search" placeholder="Search name, camera or tag…" aria-label="Search library" autocomplete="off" spellcheck="false"><kbd>Ctrl K</kbd></label>
        <button class="icon-btn" id="toggle-inspector" title="Details panel (I)" aria-label="Toggle details panel">${icon("panel", 17)}</button>
        <button class="btn primary" id="add-top">${icon("plus", 15)}<span>Add folder</span></button>
      </div>
    </header>
    <div class="toolbar" id="toolbar"></div>
    <div class="gallery-wrap" id="gallery-wrap">
      <section class="gallery" id="gallery" tabindex="0" role="listbox" aria-multiselectable="true" aria-label="Photos and videos"></section>
      <section class="dupes" id="dupes" hidden></section>
      <div class="overlay-state" id="empty" hidden></div>
    </div>
    <footer class="statusbar"><div id="status" class="status-left" aria-live="polite">Ready</div><div id="status-right" class="status-right"></div></footer>
  </main>
  <aside class="inspector" id="inspector" aria-label="Details"></aside>
</div>`;

const grid = new VirtualGrid($("#gallery"), {
  fetch: (o, l) => api.media(filter(), sort, o, l),
  isSelected: (id) => sel.has(id),
  isActive: (id) => id === activeId,
  onClick: (i, item, e) => clickItem(i, item, e),
  onOpen: (i) => void viewer.open(i),
  onContext: (i, item, e) => contextMenu(i, item, e),
  onFavorite: (item) => void toggleFavorite([item.id], !item.favorite),
  onToggleSelect: (i, item) => { toggleSelect(item.id, i); },
});

const viewer = new Viewer({
  load: (i) => grid.loadItem(i),
  total: () => grid.total,
  peek: (i) => grid.itemAt(i),
  onFavorite: async (item) => { await toggleFavorite([item.id], !item.favorite); const m = grid.itemAt(grid.indexOf(item.id)); if (m) viewer.refresh(m); },
  onRotate: async (item, d) => { await api.rotate([item.id], d); grid.patch(item.id, { rotation: (item.rotation + d + 360) % 360 }); },
  onIndex: (i, item) => { sel = new Set([item.id]); activeId = item.id; anchor = cursor = i; grid.scrollToIndex(i, "nearest"); grid.rerender(); renderInspector(); renderToolbar(); },
  onClose: () => { $("#gallery").focus({ preventScroll: true }); },
});

// ------------------------------------------------------------------ filters
function filter(): Filter {
  const f: Filter = { search, kind, scope: "all" };
  switch (nav.type) {
    case "favorites": f.scope = "favorites"; break;
    case "folder": f.scope = "folder"; f.folderId = nav.id; if (nav.dir) f.dir = nav.dir; break;
    case "album": f.scope = "album"; f.albumId = nav.id; break;
    case "tag": f.scope = "tag"; f.tag = nav.tag; break;
    case "smart": f.scope = "smart"; f.smart = nav.smart; break;
  }
  return f;
}
const navKey = (n: Nav) => n.type === "folder" ? `folder:${n.id}:${n.dir ?? ""}` : n.type === "album" ? `album:${n.id}` : n.type === "tag" ? `tag:${n.tag}` : n.type === "smart" ? `smart:${n.smart}` : n.type;
const SMART: Record<string, string> = { recent: "Recently added", untagged: "Untagged", gps: "With location" };

function titleFor(): { title: string; eyebrow: string } {
  const n = nav;
  switch (n.type) {
    case "all": return { title: "All media", eyebrow: "YOUR PERSONAL LIBRARY" };
    case "photos": return { title: "Photos", eyebrow: "YOUR PERSONAL LIBRARY" };
    case "videos": return { title: "Videos", eyebrow: "YOUR PERSONAL LIBRARY" };
    case "favorites": return { title: "Favorites", eyebrow: "YOUR PERSONAL LIBRARY" };
    case "timeline": return { title: "Timeline", eyebrow: "BY DATE" };
    case "duplicates": return { title: "Duplicates", eyebrow: "TOOLS" };
    case "folder": { const f = folders.find((x) => x.id === n.id); const name = n.dir ? n.dir.split("/").filter(Boolean).pop()! : f?.name ?? "Folder"; return { title: name, eyebrow: "FOLDER" }; }
    case "album": return { title: albums.find((a) => a.id === n.id)?.name ?? "Album", eyebrow: "ALBUM" };
    case "tag": return { title: `#${n.tag}`, eyebrow: "TAG" };
    case "smart": return { title: SMART[n.smart], eyebrow: "SMART COLLECTION" };
  }
}

function go(next: Nav) {
  const prev = navKey(nav);
  nav = next;
  kind = next.type === "photos" ? "image" : next.type === "videos" ? "video" : "all";
  if (next.type === "timeline") { sort = { key: "date", desc: true }; groupBy = "month"; }
  else if (navKey(next) !== prev || sort.key !== "date") groupBy = "none";
  clearSelection(false);
  closeMenus();
  renderSidebar(); renderChrome(); renderStatus();
  if (next.type === "duplicates") { showDupes(); return; }
  $("#dupes").hidden = true; $("#gallery").hidden = false;
  void reload(false);
}

// ------------------------------------------------------------------ data loading
async function loadSidebarData() {
  [folders, albums, tags, stats] = await Promise.all([api.folders(), api.albums(), api.tags(), api.stats()]);
  for (const id of expanded) if (!folders.some((f) => f.id === id)) expanded.delete(id);
  await Promise.all([...expanded].map(async (id) => dirsByFolder.set(id, await api.dirs(id))));
  const cur = nav;
  if (cur.type === "folder" && !folders.some((f) => f.id === cur.id)) nav = { type: "all" };
  if (cur.type === "album" && !albums.some((a) => a.id === cur.id)) nav = { type: "all" };
  renderSidebar(); renderChrome();
}

let reloading = false, again = false;
async function reload(keepScroll = true) {
  if (nav.type === "duplicates") return;
  if (reloading) { again = true; return; }
  reloading = true;
  try {
    const eff: Group = sort.key === "date" ? groupBy : "none";
    const lay = await api.layout(filter(), eff, sort.desc);
    const sig = `${lay.total}|${JSON.stringify(lay.groups)}`;
    layoutTotal = lay.total;
    if (sig !== layoutSig || !keepScroll) { layoutSig = sig; grid.setData(lay.total, lay.groups, keepScroll); }
    else void grid.refreshLoaded();
    renderEmpty(); renderChrome(); renderToolbar(); renderStatus();
    // drop selections that no longer exist in this view
    if (activeId != null && grid.indexOf(activeId) < 0 && !keepScroll) { activeId = null; }
    renderInspector();
  } catch (e) { setStatus(`Could not load library: ${errText(e)}`); }
  finally { reloading = false; if (again) { again = false; void reload(true); } }
}
const softReload = debounce(() => { void loadSidebarData(); void reload(true); }, 900);

// ------------------------------------------------------------------ sidebar
function secHead(id: string, label: string, action = "") {
  const c = collapsed.has(id);
  return `<div class="side-label"><button class="sec-toggle" data-sec="${id}" aria-expanded="${!c}">${icon("right", 11, c ? "" : "rot")}<span>${label}</span></button>${action}</div>`;
}
function navBtn(n: Nav, ic: string, label: string, count?: number, kbd?: string, extra = "") {
  const active = navKey(n) === navKey(nav);
  const attrs = Object.entries(n).map(([k, v]) => `data-${k}="${esc(v)}"`).join(" ");
  return `<button class="nav ${active ? "active" : ""}" ${attrs} ${extra} ${active ? 'aria-current="page"' : ""}>${icon(ic, 17)}<span class="nav-label">${esc(label)}</span>${count != null ? `<span class="count">${fmtNum(count)}</span>` : kbd ? `<kbd>${kbd}</kbd>` : ""}</button>`;
}

function dirTree(f: Folder) {
  const rows = dirsByFolder.get(f.id) ?? [];
  const cum = new Map<string, number>();
  for (const r of rows) {
    let p = r.dir;
    while (p.startsWith(f.path) && p.length >= f.path.length) { cum.set(p, (cum.get(p) ?? 0) + r.count); if (p === f.path) break; p = p.slice(0, p.lastIndexOf("/")); }
  }
  return [...cum.keys()].filter((d) => d !== f.path).sort((a, b) => a.localeCompare(b)).map((d) => {
    const depth = d.slice(f.path.length).split("/").filter(Boolean).length;
    const active = nav.type === "folder" && nav.id === f.id && nav.dir === d;
    return `<button class="nav sub ${active ? "active" : ""}" data-type="folder" data-id="${f.id}" data-dir="${esc(d)}" style="--depth:${depth}" title="${esc(d)}">${icon("folder", 14)}<span class="nav-label">${esc(d.split("/").pop()!)}</span><span class="count">${fmtNum(cum.get(d)!)}</span></button>`;
  }).join("");
}

function renderSidebar() {
  const maxTags = 10;
  const h: string[] = [];
  h.push(navBtn({ type: "all" }, "grid", "All media", stats.total, undefined));
  h.push(navBtn({ type: "photos" }, "image", "Photos", stats.images));
  h.push(navBtn({ type: "videos" }, "video", "Videos", stats.videos));
  h.push(navBtn({ type: "favorites" }, "heart", "Favorites", stats.favorites));
  h.push(navBtn({ type: "timeline" }, "calendar", "Timeline"));

  h.push(secHead("albums", "ALBUMS", `<button class="mini" id="new-album" title="New album" aria-label="New album">${icon("plus", 14)}</button>`));
  if (!collapsed.has("albums")) {
    h.push(albums.length ? albums.map((a) => navBtn({ type: "album", id: a.id }, "album", a.name, a.count)).join("") : `<div class="side-hint">Group photos without moving them.</div>`);
  }
  h.push(secHead("smart", "SMART"));
  if (!collapsed.has("smart")) {
    h.push(navBtn({ type: "smart", smart: "recent" }, "spark", "Recently added"), navBtn({ type: "smart", smart: "untagged" }, "tag", "Untagged"), navBtn({ type: "smart", smart: "gps" }, "pin", "With location"));
  }
  if (tags.length) {
    h.push(secHead("tags", "TAGS"));
    if (!collapsed.has("tags")) h.push(tags.slice(0, maxTags).map((t) => navBtn({ type: "tag", tag: t.name }, "tag", t.name, t.count)).join(""));
  }
  h.push(secHead("folders", "FOLDERS", `<button class="mini" id="add-folder" title="Add folder" aria-label="Add folder">${icon("plus", 14)}</button>`));
  if (!collapsed.has("folders")) {
    h.push(folders.length ? folders.map((f) => {
      const active = nav.type === "folder" && nav.id === f.id && !nav.dir;
      const open = expanded.has(f.id);
      return `<div class="folder-row ${active ? "active" : ""} ${f.available ? "" : "offline"}">
        <button class="chev" data-expand="${f.id}" aria-label="${open ? "Collapse" : "Expand"} ${esc(f.name)}" aria-expanded="${open}">${icon("right", 12, open ? "rot" : "")}</button>
        <button class="nav folder" data-type="folder" data-id="${f.id}" title="${esc(f.path)}${f.available ? "" : " — not available (drive disconnected?)"}">${icon("folder", 16)}<span class="nav-label">${esc(f.name)}</span>${f.available ? `<span class="count">${fmtNum(f.count)}</span>` : `<span class="badge-off">offline</span>`}</button>
        <button class="mini more" data-folder-menu="${f.id}" aria-label="Folder options">${icon("more", 14)}</button></div>${open ? dirTree(f) : ""}`;
    }).join("") : `<div class="side-hint">Your folders will appear here.</div>`);
  }
  h.push(`<div class="side-label static">TOOLS</div>`, navBtn({ type: "duplicates" }, "copy", "Find duplicates"));
  $("#side-nav").innerHTML = h.join("");
  renderSideFoot();
}

function renderSideFoot() {
  const running = scan.running;
  const pct = scan.phase === "process" && scan.total ? Math.round((scan.processed / scan.total) * 100) : null;
  $("#side-foot").innerHTML = `
    ${running ? `<div class="scan-card" role="status"><div class="scan-line"><span class="spin sm"></span><strong>${scan.phase === "discover" ? "Looking for media" : "Preparing previews"}</strong><button class="link" id="cancel-scan">Cancel</button></div>
      <div class="bar ${pct == null ? "indeterminate" : ""}"><i style="width:${pct ?? 40}%"></i></div>
      <small>${scan.phase === "discover" ? `${fmtNum(scan.discovered)} files found${scan.folder ? ` in ${esc(scan.folder)}` : ""}` : `${fmtNum(scan.processed)} of ${fmtNum(scan.total)}`}</small></div>` : ""}
    <div class="foot-row"><span class="offline-dot" aria-hidden="true"></span><span>Offline · local only</span><button class="icon-btn sm" id="settings" title="Settings (Ctrl+,)" aria-label="Settings">${icon("sliders", 16)}</button></div>`;
}

// ------------------------------------------------------------------ header / toolbar / status
const SORT_LABEL: Record<string, string> = { date: "Date taken", name: "Name", added: "Date added", size: "File size", kind: "Type" };

function renderChrome() {
  const t = titleFor();
  $("#page-title").textContent = t.title;
  $("#eyebrow").textContent = t.eyebrow;
  document.title = `${t.title} — ${APP_NAME}`;
  const sub: string[] = [];
  if (nav.type === "duplicates") sub.push(dupes ? `${dupes.length} group${dupes.length === 1 ? "" : "s"}` : "Exact copies, found by content");
  else {
    sub.push(`${fmtNum(layoutTotal)} ${layoutTotal === 1 ? "item" : "items"}`);
    if (nav.type === "all" && stats.total) sub.push(`${fmtNum(stats.images)} ${stats.images === 1 ? "photo" : "photos"}`, `${fmtNum(stats.videos)} ${stats.videos === 1 ? "video" : "videos"}`, fmtBytes(stats.bytes));
  }
  $("#subtitle").textContent = sub.join(" · ");
  const dup = nav.type === "duplicates";
  $("#toolbar").hidden = dup;
  $(".search-wrap").classList.toggle("hidden", dup);
  renderToolbar();
  $("#shell").classList.toggle("no-inspector", !inspectorOpen || dup);
  $("#toggle-inspector").classList.toggle("on", inspectorOpen);
}

function renderToolbar() {
  const n = sel.size;
  if (n > 1) {
    $("#toolbar").innerHTML = `<div class="selbar"><strong>${fmtNum(n)} selected</strong>
      <button class="btn sm" data-bulk="fav">${icon("heart", 14)} Favorite</button>
      <button class="btn sm" data-bulk="album">${icon("album", 14)} Add to album</button>
      <button class="btn sm" data-bulk="tag">${icon("tag", 14)} Tag</button>
      <button class="btn sm" data-bulk="cw">${icon("cw", 14)} Rotate</button>
      ${nav.type === "album" ? `<button class="btn sm" data-bulk="unalbum">Remove from album</button>` : ""}
      <button class="link" data-bulk="clear">Clear selection</button></div>`;
    return;
  }
  const showGroup = sort.key === "date";
  $("#toolbar").innerHTML = `<div class="filters" role="group" aria-label="Media type">${(["all", "image", "video"] as const).map((k) => `<button class="chip ${kind === k ? "active" : ""}" data-kind="${k}" aria-pressed="${kind === k}">${k === "all" ? "Everything" : k === "image" ? "Photos" : "Videos"}</button>`).join("")}</div>
    <div class="view-controls">
      ${nav.type === "album" ? `<button class="link" id="rename-album">Rename</button><button class="link danger" id="delete-album">Delete</button>` : ""}
      <button class="btn sm" id="sort-btn" aria-haspopup="menu" title="Sort">${icon("sort", 14)}<span>${SORT_LABEL[sort.key]} ${sort.desc ? "↓" : "↑"}</span></button>
      ${showGroup ? `<div class="seg" id="group-seg" role="group" aria-label="Group by">${(["none", "day", "month", "year"] as Group[]).map((g) => `<button data-group="${g}" class="${groupBy === g ? "on" : ""}" aria-pressed="${groupBy === g}">${g === "none" ? "Flat" : g[0].toUpperCase() + g.slice(1)}</button>`).join("")}</div>` : ""}
      <label class="density" title="Thumbnail size">${icon("grid", 14)}<input id="density" type="range" min="120" max="320" step="10" value="${tile}" aria-label="Thumbnail size"></label>
    </div>`;
}

function setStatus(t: string) { $("#status").textContent = t; }
function renderStatus() {
  if (nav.type === "duplicates") { setStatus(dupes ? `${dupes.length} duplicate group${dupes.length === 1 ? "" : "s"}` : "Ready"); $("#status-right").innerHTML = `Local catalog <i class="dot"></i>`; return; }
  if (scan.running) setStatus(scan.phase === "discover" ? `Scanning… ${fmtNum(scan.discovered)} files found` : `Preparing previews… ${fmtNum(scan.processed)} / ${fmtNum(scan.total)}`);
  else if (stats.failed) setStatus(`${fmtNum(stats.failed)} file${stats.failed === 1 ? "" : "s"} couldn’t be previewed`);
  else setStatus(layoutTotal ? `${fmtNum(layoutTotal)} shown` : "Ready");
  $("#status-right").innerHTML = `${sel.size ? `${fmtNum(sel.size)} selected · ` : ""}Local catalog <i class="dot"></i>`;
}

function renderEmpty() {
  const el = $("#empty");
  const add = `<button class="btn primary lg" data-act="add">${icon("plus", 16)} Add your first folder</button>`;
  let html = "";
  if (!folders.length) html = emptyState("folder", "Your library starts here", "Add a folder to browse photos and videos stored on this computer. Nothing is uploaded, moved or changed.", add + `<div class="trust"><span>${icon("shield", 14)} Offline</span><span>No account</span><span>No telemetry</span></div>`);
  else if (layoutTotal === 0 && scan.running && stats.total === 0) html = emptyState("refresh", "Looking through your folders…", `${fmtNum(scan.discovered)} files found so far. Photos appear as soon as they’re ready.`);
  else if (layoutTotal === 0 && nav.type === "favorites") html = emptyState("heart", "No favorites yet", "Click the heart on any photo, or select it and press F.");
  else if (layoutTotal === 0 && nav.type === "album") html = emptyState("album", "This album is empty", "Select photos in your library and choose <b>Add to album</b>.");
  else if (layoutTotal === 0 && (search || kind !== "all")) html = emptyState("search", "No matching media", "Try a different search or clear the filters.", `<button class="btn" data-act="clear">Clear search &amp; filters</button>`);
  else if (layoutTotal === 0) html = emptyState("image", "Nothing here yet", nav.type === "smart" ? "Nothing matches this collection right now." : "No photos or videos in this view.");
  el.hidden = !html; el.innerHTML = html;
  $("#gallery").classList.toggle("is-empty", !!html);
}

// ------------------------------------------------------------------ selection
function clearSelection(render = true) {
  sel = new Set(); activeId = null;
  if (render) { grid.rerender(); renderInspector(); renderToolbar(); renderStatus(); }
}
function selectOnly(i: number, item: Media) {
  sel = new Set([item.id]); activeId = item.id; anchor = cursor = i;
  grid.rerender(); renderInspector(); renderToolbar(); renderStatus();
}
function toggleSelect(id: number, i: number) {
  if (sel.has(id)) sel.delete(id); else sel.add(id);
  activeId = sel.has(id) ? id : (sel.size ? [...sel][sel.size - 1] : null);
  anchor = cursor = i;
  grid.rerender(); renderInspector(); renderToolbar(); renderStatus();
}
async function clickItem(i: number, item: Media, e: MouseEvent) {
  if (e.shiftKey && sel.size) {
    const [a, b] = [Math.min(anchor, i), Math.max(anchor, i)];
    const ids = await api.ids(filter(), sort, a, b - a + 1);
    sel = new Set(ids); activeId = item.id; cursor = i;
    grid.rerender(); renderInspector(); renderToolbar(); renderStatus();
  } else if (e.ctrlKey || e.metaKey) toggleSelect(item.id, i);
  else selectOnly(i, item);
}
const targetIds = () => (sel.size ? [...sel] : activeId != null ? [activeId] : []);

// ------------------------------------------------------------------ actions
async function toggleFavorite(ids: number[], value: boolean) {
  if (!ids.length) return;
  ids.forEach((id) => grid.patch(id, { favorite: value }));
  try {
    await api.favorite(ids, value);
    stats = await api.stats(); renderSidebar();
    if (activeId != null && ids.includes(activeId)) renderInspector();
    if (nav.type === "favorites" && !value && !viewer.isOpen) { setTimeout(() => void reload(true), 250); }
  } catch (e) { ids.forEach((id) => grid.patch(id, { favorite: !value })); toast(errText(e), "error"); }
}
async function bulkFavorite() {
  const ids = targetIds();
  const loaded = ids.map((id) => grid.itemAt(grid.indexOf(id))).filter(Boolean) as Media[];
  const allFav = loaded.length === ids.length && loaded.every((m) => m.favorite);
  await toggleFavorite(ids, !allFav);
  toast(allFav ? `Removed ${ids.length} from favorites` : `Added ${ids.length} to favorites`, "success");
}
function albumPicker(ids: number[], anchorEl?: HTMLElement) {
  if (!ids.length) return;
  const r = anchorEl?.getBoundingClientRect();
  const x = r ? r.left : window.innerWidth / 2 - 100, y = r ? r.bottom + 6 : window.innerHeight / 3;
  const items: MenuItem[] = [{ header: "Add to album" }, ...albums.map<MenuItem>((a) => ({ label: a.name, icon: "album", hint: String(a.count), run: async () => {
    try { const n = await api.albumAdd(a.id, ids); toast(n ? `Added ${n} to “${a.name}”` : `Already in “${a.name}”`, "success"); await loadSidebarData(); if (activeId != null) renderInspector(); } catch (e) { toast(errText(e), "error"); }
  } })), ...(albums.length ? [{ separator: true } as MenuItem] : []), { label: "New album…", icon: "plus", run: async () => {
    const name = await promptDialog({ title: "New album", label: "Album name", confirm: "Create & add" });
    if (!name) return;
    try { const id = await api.createAlbum(name); await api.albumAdd(id, ids); toast(`Created “${name}”`, "success"); await loadSidebarData(); renderInspector(); } catch (e) { toast(errText(e), "error"); }
  } }];
  showMenu(x, y, items, anchorEl);
}
async function addTags(ids: number[]) {
  if (!ids.length) return;
  const t = await promptDialog({ title: ids.length > 1 ? `Tag ${ids.length} items` : "Add tag", label: "Tag", confirm: "Add tag" });
  if (!t) return;
  try { await api.addTag(ids, t); await loadSidebarData(); renderInspector(); toast(`Tagged “${t.replace(/^#/, "")}”`, "success"); } catch (e) { toast(errText(e), "error"); }
}
async function rotateIds(ids: number[], d: number) {
  if (!ids.length) return;
  try { await api.rotate(ids, d); ids.forEach((id) => grid.patch(id, { thumbnail: null, state: 0 })); toast("Rotated — original file unchanged", "success"); } catch (e) { toast(errText(e), "error"); }
}
async function addFolder() {
  try {
    const dir = await openDialog({ directory: true, multiple: false, title: "Choose a photo or video folder" });
    if (typeof dir !== "string") return;
    const id = await api.addFolder(dir);
    expanded.add(id);
    await loadSidebarData();
    toast("Folder added — indexing in the background", "success");
  } catch (e) { toast(errText(e), "error"); }
}
function folderMenu(id: number, el: HTMLElement) {
  const f = folders.find((x) => x.id === id); if (!f) return;
  const r = el.getBoundingClientRect();
  showMenu(r.right - 4, r.bottom + 4, [
    { header: f.name },
    { label: "Rescan this folder", icon: "refresh", run: () => { void api.scan(id); } },
    { label: "Remove from library…", icon: "x", danger: true, run: async () => {
      if (await confirmDialog({ title: "Remove folder from GalleryDesk?", body: `“${esc(f.name)}” will disappear from the library along with its favorites, tags and album entries. <b>Your photos and videos on disk are not touched.</b>`, confirm: "Remove from library", danger: true })) {
        try { await api.removeFolder(id); if (nav.type === "folder" && nav.id === id) nav = { type: "all" }; await loadSidebarData(); await reload(false); toast("Folder removed from library"); } catch (e) { toast(errText(e), "error"); }
      }
    } },
  ], el);
}
function contextMenu(i: number, item: Media, e: MouseEvent) {
  if (!sel.has(item.id)) selectOnly(i, item);
  const ids = targetIds(); const n = ids.length;
  const one = n === 1;
  showMenu(e.clientX, e.clientY, [
    ...(one ? [{ label: "Open", icon: "image", hint: "Enter", run: () => void viewer.open(i) } as MenuItem] : []),
    { label: item.favorite && one ? "Remove from favorites" : "Add to favorites", icon: "heart", hint: "F", run: () => void bulkFavorite() },
    { label: "Add to album…", icon: "album", run: () => albumPicker(ids) },
    ...(nav.type === "album" ? [{ label: "Remove from this album", icon: "x", run: async () => { await api.albumRemove(nav.type === "album" ? nav.id : 0, ids); await loadSidebarData(); await reload(true); } } as MenuItem] : []),
    { label: "Add tag…", icon: "tag", run: () => void addTags(ids) },
    { separator: true },
    { label: "Rotate right", icon: "cw", hint: "R", run: () => void rotateIds(ids, 90), disabled: item.kind === "video" && one },
    { label: "Rotate left", icon: "ccw", hint: "⇧R", run: () => void rotateIds(ids, -90), disabled: item.kind === "video" && one },
    ...(one ? [{ separator: true } as MenuItem, { label: "Show in folder", icon: "folder", run: () => api.reveal(item.path).catch((er) => toast(errText(er), "error")) } as MenuItem, { label: "Open with default app", icon: "external", run: () => api.openExternal(item.path).catch((er) => toast(errText(er), "error")) } as MenuItem] : []),
  ]);
}

// ------------------------------------------------------------------ inspector
const row = (k: string, v: string) => (v && v !== "—" ? `<div><span>${k}</span><b>${esc(v)}</b></div>` : "");
let inspectToken = 0;
async function renderInspector() {
  const el = $("#inspector");
  const my = ++inspectToken;
  if (sel.size > 1) {
    el.innerHTML = `<div class="insp-head"><span>DETAILS</span></div><div class="insp-body"><div class="multi"><div class="multi-ic">${icon("layers", 26)}</div><strong>${fmtNum(sel.size)} items selected</strong><p>Use the bar above to favorite, tag, rotate or add them to an album.</p><button class="btn" data-bulk="clear">Clear selection</button></div></div>`;
    return;
  }
  if (activeId == null) {
    el.innerHTML = `<div class="insp-head"><span>DETAILS</span></div><div class="insp-empty"><div class="multi-ic">${icon("image", 26)}</div><strong>Select a photo</strong><p>Choose an item to see its details, tags and file information.</p></div>`;
    return;
  }
  let d: Detail;
  try { d = await api.detail(activeId); } catch { activeId = null; renderInspector(); return; }
  if (my !== inspectToken) return;
  const dims = d.width && d.height ? `${d.width} × ${d.height}` : "";
  const expo = [d.exposure, d.aperture ? `f/${d.aperture}` : "", d.iso ? `ISO ${d.iso}` : ""].filter(Boolean).join(" · ");
  const cam = row("Camera", d.camera ?? "") + row("Lens", d.lens ?? "") + row("Exposure", expo) + row("Focal length", d.focal ? `${Math.round(d.focal)} mm` : "");
  el.innerHTML = `
    <div class="insp-head"><span>DETAILS</span><button class="icon-btn sm" id="insp-close" aria-label="Close details">${icon("x", 15)}</button></div>
    <div class="insp-body">
      <button class="insp-preview" data-act="open" aria-label="Open ${esc(d.name)}">${d.thumbnail ? `<img src="${esc(src(d.thumbnail))}" alt="">` : `<div class="ph">${icon(d.kind === "video" ? "video" : "image", 28)}</div>`}${d.kind === "video" ? `<span class="vchip big">${icon("play", 11)}${fmtDuration(d.duration)}</span>` : ""}</button>
      <div class="insp-title"><div class="detail-kind">${d.kind.toUpperCase()}${d.state === 2 ? " · NO PREVIEW" : ""}</div><h3>${esc(d.name)}</h3></div>
      <div class="insp-actions">
        <button class="btn sm ${d.favorite ? "on" : ""}" data-ia="fav" aria-pressed="${d.favorite}">${icon("heart", 14)} ${d.favorite ? "Favorited" : "Favorite"}</button>
        <button class="btn sm" data-ia="rotl" title="Rotate left" aria-label="Rotate left" ${d.kind === "video" ? "disabled" : ""}>${icon("ccw", 14)}</button>
        <button class="btn sm" data-ia="rotr" title="Rotate right" aria-label="Rotate right" ${d.kind === "video" ? "disabled" : ""}>${icon("cw", 14)}</button>
      </div>
      <div class="detail-table">${row("Dimensions", dims)}${row("File size", fmtBytes(d.bytes))}${row("Duration", d.duration != null ? fmtDuration(d.duration) : "")}${row("Format", d.name.split(".").pop()!.toUpperCase())}${row("Taken", d.taken ? fmtDateTime(d.taken, true) : "")}${row("Modified", fmtDateTime(d.modified, false))}${row("Added", fmtDateTime(d.added, false))}</div>
      ${cam ? `<div class="insp-sec">Camera</div><div class="detail-table">${cam}</div>` : ""}
      ${d.lat != null && d.lon != null ? `<div class="insp-sec">Location</div><div class="detail-table"><div><span>Coordinates</span><b>${d.lat.toFixed(5)}, ${d.lon.toFixed(5)}</b></div></div><button class="link" data-ia="copygps">Copy coordinates</button>` : ""}
      <div class="insp-sec">Albums</div>
      <div class="chips">${d.albums.map((a) => `<span class="chip-tag"><button data-album="${a.id}">${esc(a.name)}</button><button class="x" data-unalbum="${a.id}" aria-label="Remove from ${esc(a.name)}">${icon("x", 11)}</button></span>`).join("")}<button class="chip-add" data-ia="album">${icon("plus", 12)} Add</button></div>
      <div class="insp-sec">Tags</div>
      <div class="chips">${d.tags.map((t) => `<span class="chip-tag"><button data-tag="${esc(t)}">#${esc(t)}</button><button class="x" data-untag="${esc(t)}" aria-label="Remove tag ${esc(t)}">${icon("x", 11)}</button></span>`).join("")}
        <form class="tag-form" id="tag-form"><input list="tag-list" id="tag-input" placeholder="Add tag…" maxlength="80" aria-label="Add tag" autocomplete="off"><datalist id="tag-list">${tags.map((t) => `<option value="${esc(t.name)}">`).join("")}</datalist></form></div>
      <div class="insp-actions col"><button class="btn sm" data-ia="reveal">${icon("folder", 14)} Show in folder</button><button class="btn sm" data-ia="external">${icon("external", 14)} Open with default app</button></div>
      <p class="path-label" title="${esc(d.path)}">${esc(d.path)}</p>
    </div>`;
}

// ------------------------------------------------------------------ duplicates
async function showDupes() {
  $("#gallery").hidden = true; $("#empty").hidden = true;
  const el = $("#dupes"); el.hidden = false;
  renderChrome(); renderDupes();
}
function renderDupes() {
  const el = $("#dupes");
  const head = `<div class="dupe-head"><div><h2>Exact duplicates</h2><p>Files with identical content are grouped here. GalleryDesk only <b>reads</b> files to compare them — it never deletes or changes anything. Use “Show in folder” to clean up with your file manager.</p></div>
    ${dupeRun ? `<div class="dupe-run"><div class="bar"><i style="width:${dupeRun.total ? Math.round((dupeRun.done / dupeRun.total) * 100) : 5}%"></i></div><small>Comparing ${fmtNum(dupeRun.done)} of ${fmtNum(dupeRun.total)} files</small><button class="link" id="dupe-cancel">Cancel</button></div>` : `<button class="btn primary" id="dupe-run">${icon("refresh", 15)} ${dupes ? "Scan again" : "Find duplicates"}</button>`}</div>`;
  let body = "";
  if (dupes) {
    const waste = dupes.reduce((a, g) => a + g.bytes * (g.items.length - 1), 0);
    body = dupes.length ? `<div class="dupe-sum">${fmtNum(dupes.length)} group${dupes.length === 1 ? "" : "s"} · ${fmtBytes(waste)} used by extra copies</div>` + dupes.map((g) => `<div class="dupe-group"><div class="dupe-meta"><strong>${g.items.length} identical files</strong><span>${fmtBytes(g.bytes)} each</span></div>
      <div class="dupe-items">${g.items.map((m) => `<div class="dupe-item"><div class="dupe-thumb">${m.thumbnail ? `<img src="${esc(src(m.thumbnail))}" alt="" loading="lazy">` : icon("image", 22)}</div>
        <div class="dupe-info"><b title="${esc(m.name)}">${esc(m.name)}</b><span title="${esc(m.path)}">${esc(m.path.replace(/\/[^/]*$/, ""))}</span><span>${fmtDate(m.date)}</span></div>
        <div class="dupe-btns"><button class="btn sm" data-reveal="${esc(m.path)}">${icon("folder", 13)} Show</button></div></div>`).join("")}</div></div>`).join("")
      : emptyState("check", "No duplicates found", "No two files in your library have identical content.");
  } else if (!dupeRun) body = emptyState("copy", "Find copies taking up space", "GalleryDesk compares file sizes first, then reads only files that could match. Results stay on this computer.");
  el.innerHTML = head + `<div class="dupe-body">${body}</div>`;
}
async function runDupes() {
  dupeRun = { done: 0, total: 0 }; renderDupes();
  try { dupes = await api.duplicates(); } catch (e) { const m = errText(e); if (m !== "Cancelled") toast(m, "error"); }
  dupeRun = null; renderChrome(); if (nav.type === "duplicates") renderDupes();
}

// ------------------------------------------------------------------ settings
function seg(name: string, opts: [string, string][], cur: string) {
  return `<div class="seg" role="radiogroup">${opts.map(([v, l]) => `<button role="radio" aria-checked="${v === cur}" class="${v === cur ? "on" : ""}" data-set="${name}" data-v="${v}">${l}</button>`).join("")}</div>`;
}
function toggle(name: string, on: boolean, label: string, help = "") {
  return `<div class="set-row"><div><b>${label}</b>${help ? `<p>${help}</p>` : ""}</div><button class="switch ${on ? "on" : ""}" role="switch" aria-checked="${on}" aria-label="${label}" data-toggle="${name}"><i></i></button></div>`;
}
async function openSettings(tab = "appearance") {
  const info = await api.info().catch(() => null);
  const vsup = await api.videoSupported().catch(() => false);
  const m = modal(`<div class="settings"><nav class="set-tabs" role="tablist">${[["appearance", "Appearance", "sun"], ["library", "Library", "folder"], ["catalog", "Catalog", "layers"], ["keys", "Shortcuts", "keyboard"], ["about", "About", "info"]].map(([id, l, ic]) => `<button role="tab" data-tab="${id}">${icon(ic, 16)}${l}</button>`).join("")}</nav><div class="set-pane" id="set-pane"></div><button class="icon-btn set-x" aria-label="Close settings" data-close>${icon("x", 17)}</button></div>`, { label: "Settings", cls: "wide" });
  const pane = m.el.querySelector<HTMLElement>("#set-pane")!;
  let excl: string[] = await api.excluded().catch(() => []);
  const addExcl = async (entry: string) => {
    const n = await api.addExcluded(entry);
    excl = await api.excluded();
    toast(n ? `Excluded — ${fmtNum(n)} ${n === 1 ? "item" : "items"} removed from the library (files on disk untouched)` : "Folder excluded", "success");
    draw(tab);
  };
  const draw = (t: string) => {
    $$("[data-tab]", m.el).forEach((b) => { b.classList.toggle("on", b.dataset.tab === t); b.setAttribute("aria-selected", String(b.dataset.tab === t)); });
    const pref = themePref();
    if (t === "appearance") pane.innerHTML = `<h2>Appearance</h2>
      <div class="set-row col"><div><b>Theme</b><p>Follow your system, or pick one.</p></div>${seg("theme", [["system", "System"], ["light", "Light"], ["dark", "Dark"]], pref)}</div>
      <div class="set-row col"><div><b>Accent colour</b></div><div class="swatches" role="radiogroup">${ACCENTS.map((a) => `<button class="swatch ${S.accent === a.id ? "on" : ""}" role="radio" aria-checked="${S.accent === a.id}" data-accent="${a.id}" title="${a.label}" aria-label="${a.label}" style="--sw:${a.dark};--sw2:${a.light}"></button>`).join("")}</div></div>
      <div class="set-row col"><div><b>Thumbnails</b><p>How photos fill their tile.</p></div>${seg("fit", [["fill", "Fill (crop)"], ["contain", "Fit (whole photo)"]], S.fit)}</div>
      ${toggle("captions", S.captions === "1", "Show file names and dates", "Under each thumbnail in the grid.")}`;
    else if (t === "library") pane.innerHTML = `<h2>Library</h2>
      <div class="set-row col"><div><b>Exclude folders</b><p>Excluded folders are skipped by every scan and by the folder watcher, and anything already indexed inside them is removed from the library. Your files are never touched. Enter a full path, or just a name such as <code>node_modules</code> to skip every folder with that name.</p></div>
        <div class="excl"><div class="excl-add"><input id="excl-in" type="text" spellcheck="false" autocomplete="off" placeholder="/home/me/Pictures/Screenshots  or  node_modules" aria-label="Folder path or name to exclude"><button class="btn sm" id="excl-browse">Browse…</button><button class="btn sm primary" id="excl-add">Exclude</button></div>
        ${excl.length ? `<ul class="excl-list">${excl.map((x, i) => `<li><span class="excl-ic">${icon("folder", 15)}</span><span class="excl-path" title="${esc(x)}">${esc(x)}</span><button class="btn sm" data-excl-rm="${i}" aria-label="Stop excluding ${esc(x)}">Remove</button></li>`).join("")}</ul>` : `<p class="excl-none">No folders excluded.</p>`}</div></div>
      ${toggle("watch", S.watch === "1", "Watch folders for changes", "Pick up new and removed files automatically. Uses the system’s file-change notifications and nothing while idle.")}
      ${toggle("video_builtin", S.video_builtin !== "0", "Play videos in the built-in player", "Turn off to open videos in your system’s default player instead. This switches itself off if the built-in player ever stops responding.")}
      ${toggle("scan_on_launch", S.scan_on_launch === "1", "Check for changes at launch", "A quick comparison of file sizes and dates — originals are only read, never modified.")}
      <div class="set-row"><div><b>Indexing workers</b><p>Parallel preview jobs. Fewer = quieter and lighter.</p></div><select id="workers" aria-label="Indexing workers">${[0, 1, 2, 3, 4, 6, 8].map((n) => `<option value="${n}" ${S.workers === String(n) ? "selected" : ""}>${n === 0 ? "Automatic" : n}</option>`).join("")}</select></div>
      <div class="set-row"><div><b>Thumbnail resolution</b><p>Applies to newly built previews. Rebuild to apply to all.</p></div><select id="thumb_size" aria-label="Thumbnail resolution">${[320, 480, 640, 800].map((n) => `<option value="${n}" ${S.thumb_size === String(n) ? "selected" : ""}>${n} px</option>`).join("")}</select></div>
      <div class="set-row"><div><b>Preview cache</b><p>${info ? `${fmtNum(info.thumbnails.files)} thumbnails (${fmtBytes(info.thumbnails.bytes)}) · ${fmtNum(info.previews.files)} large previews (${fmtBytes(info.previews.bytes)})` : ""} Safe to delete; rebuilt on demand.</p></div><div class="btn-row"><button class="btn sm" id="rebuild">Rebuild thumbnails</button><button class="btn sm" id="clear-prev">Clear large previews</button></div></div>`;
    else if (t === "catalog") pane.innerHTML = `<h2>Catalog</h2><p class="lead">Your catalog holds folders, favorites, albums, tags and settings. Photos are never copied into it.</p>
      <div class="set-row"><div><b>Back up catalog</b><p>Save a copy of your favorites, albums and tags.</p></div><button class="btn sm" id="backup">Save backup…</button></div>
      <div class="set-row"><div><b>Restore from backup</b><p>Replaces the current catalog the next time GalleryDesk starts.</p></div><button class="btn sm" id="restore">Choose backup…</button></div>
      <div class="set-row"><div><b>Rescan everything</b><p>Re-check all folders and retry files that failed before.</p></div><button class="btn sm" id="rescan">Rescan all</button></div>`;
    else if (t === "keys") pane.innerHTML = `<h2>Keyboard shortcuts</h2><div class="keys">${[["Ctrl K", "Search"], ["Ctrl 1–5", "All · Photos · Videos · Favorites · Timeline"], ["Arrows", "Move through the grid"], ["Enter / Space", "Open in viewer"], ["F", "Favorite"], ["R / Shift R", "Rotate right / left"], ["I", "Details panel"], ["Ctrl A", "Select all"], ["Esc", "Close · clear selection"], ["← → in viewer", "Previous / next"], ["+ − 0 1 in viewer", "Zoom · fit · 100%"], ["Ctrl ,", "Settings"]].map(([k, v]) => `<div><kbd>${k}</kbd><span>${v}</span></div>`).join("")}</div>`;
    else pane.innerHTML = `<h2>About ${APP_NAME}</h2><p class="lead">An offline-first photo and video manager for Linux. Version ${esc(info?.version ?? "")}.</p>
      <div class="detail-table wide">${row("FFmpeg", info ? (info.ffmpeg ? "Found" : "Not found — previews unavailable") : "")}${row("Built-in video player", vsup ? (S.video_builtin !== "0" ? "Ready" : "Switched off in Settings → Library") : "System GStreamer plugins missing — install gstreamer1.0-plugins-base and -good")}${row("ffprobe", info ? (info.ffprobe ? "Found" : "Not found — video details limited") : "")}${row("Data folder", info?.dataDir ?? "")}</div>
      <div class="trust big"><span>${icon("shield", 15)} Local only</span><span>No accounts</span><span>No telemetry</span><span>Originals never modified</span></div>`;
  };
  draw(tab);
  m.el.addEventListener("click", async (e) => {
    const t = e.target as HTMLElement;
    const tabBtn = t.closest<HTMLElement>("[data-tab]"); if (tabBtn) { tab = tabBtn.dataset.tab!; draw(tab); return; }
    if (t.closest("[data-close]")) { m.close(); return; }
    const s = t.closest<HTMLElement>("[data-set]");
    if (s) { const name = s.dataset.set!, v = s.dataset.v!; await setSetting(name, v); draw(tab); return; }
    const a = t.closest<HTMLElement>("button[data-accent]"); // not bare [data-accent]: <html> carries that attribute too if (a) { await setSetting("accent", a.dataset.accent!); draw(tab); return; }
    const tg = t.closest<HTMLElement>("[data-toggle]"); if (tg) { const n = tg.dataset.toggle!; await setSetting(n, (n === "video_builtin" ? S[n] !== "0" : S[n] === "1") ? "0" : "1"); draw(tab); return; }
    const rm = t.closest<HTMLElement>("[data-excl-rm]");
    if (rm) { try { await api.removeExcluded(excl[Number(rm.dataset.exclRm)]); excl = await api.excluded(); toast("No longer excluded — rescanning to bring its files back", "success"); draw(tab); } catch (er) { toast(errText(er), "error"); } return; }
    const id = t.closest<HTMLElement>("button[id]")?.id;
    try {
      if (id === "excl-add") { const inp = m.el.querySelector<HTMLInputElement>("#excl-in")!; if (inp.value.trim()) await addExcl(inp.value); else inp.focus(); }
      else if (id === "excl-browse") { const f = await openDialog({ directory: true, multiple: false, title: "Choose a folder to exclude" }); if (typeof f === "string") await addExcl(f); }
      else
      if (id === "rebuild") { if (await confirmDialog({ title: "Rebuild all thumbnails?", body: "Thumbnails are regenerated in the background. Your originals are not touched.", confirm: "Rebuild" })) { await api.rebuildThumbs(); toast("Rebuilding thumbnails…"); m.close(); } }
      else if (id === "clear-prev") { const n = await api.clearPreviews(); toast(`Freed ${fmtBytes(n)}`, "success"); draw(tab); }
      else if (id === "backup") { const dest = await saveDialog({ defaultPath: `gallerydesk-catalog-${new Date().toISOString().slice(0, 10)}.sqlite3`, filters: [{ name: "GalleryDesk catalog", extensions: ["sqlite3"] }] }); if (dest) { await api.backup(dest); toast("Backup saved", "success"); } }
      else if (id === "restore") { const f = await openDialog({ multiple: false, directory: false, filters: [{ name: "GalleryDesk catalog", extensions: ["sqlite3"] }] }); if (typeof f === "string") { await api.restore(f); toast("Backup staged — restart GalleryDesk to finish restoring", "success"); } }
      else if (id === "rescan") { await api.scan(); m.close(); }
    } catch (er) { toast(errText(er), "error"); }
  });
  m.el.addEventListener("keydown", (e) => { const t = e.target as HTMLInputElement; if (t.id === "excl-in" && e.key === "Enter") { e.preventDefault(); m.el.querySelector<HTMLElement>("#excl-add")?.click(); } });
  m.el.addEventListener("change", async (e) => { const t = e.target as HTMLSelectElement; if (t.id === "workers" || t.id === "thumb_size") await setSetting(t.id, t.value); });
  m.el.addEventListener("keydown", (e) => { if (e.key === "ArrowDown" || e.key === "ArrowUp") { const tabs = $$("[data-tab]", m.el); const i = tabs.indexOf(document.activeElement as HTMLElement); if (i >= 0) { e.preventDefault(); tabs[(i + (e.key === "ArrowDown" ? 1 : tabs.length - 1)) % tabs.length].focus(); } } });
}

async function setSetting(name: string, value: string) {
  S[name] = value;
  if (name === "theme" || name === "accent") applyTheme((S.theme as ThemePref) ?? "system", S.accent);
  if (name === "fit") grid.setFit(value === "contain");
  if (name === "captions") grid.setCaptions(value === "1");
  try { await api.setSetting(name, value); } catch (e) { toast(errText(e), "error"); }
}

// ------------------------------------------------------------------ events
document.addEventListener("click", async (e) => {
  const t = e.target as HTMLElement;
  const n = t.closest<HTMLElement>(".nav[data-type]");
  if (n && !t.closest("[data-folder-menu]")) {
    const d = n.dataset, type = d.type as Nav["type"];
    if (type === "folder") go({ type, id: Number(d.id), dir: d.dir });
    else if (type === "album") go({ type, id: Number(d.id) });
    else if (type === "tag") go({ type, tag: d.tag! });
    else if (type === "smart") go({ type, smart: d.smart as "recent" });
    else go({ type } as Nav);
    return;
  }
  const sec = t.closest<HTMLElement>("[data-sec]");
  if (sec) { const id = sec.dataset.sec!; collapsed.has(id) ? collapsed.delete(id) : collapsed.add(id); store.set("gallerydesk.collapsed", JSON.stringify([...collapsed])); renderSidebar(); return; }
  const ex = t.closest<HTMLElement>("[data-expand]");
  if (ex) { const id = Number(ex.dataset.expand); if (expanded.has(id)) expanded.delete(id); else { expanded.add(id); dirsByFolder.set(id, await api.dirs(id)); } renderSidebar(); return; }
  const fm = t.closest<HTMLElement>("[data-folder-menu]"); if (fm) { folderMenu(Number(fm.dataset.folderMenu), fm); return; }
  if (t.closest("#add-folder,#add-top") || t.closest('[data-act="add"]')) { void addFolder(); return; }
  if (t.closest("#new-album")) { const name = await promptDialog({ title: "New album", label: "Album name", confirm: "Create" }); if (name) { try { const id = await api.createAlbum(name); await loadSidebarData(); go({ type: "album", id }); } catch (er) { toast(errText(er), "error"); } } return; }
  if (t.closest("#settings")) { void openSettings(); return; }
  if (t.closest("#cancel-scan")) { void api.cancelScan(); return; }
  if (t.closest("#toggle-inspector, #insp-close")) { toggleInspector(); return; }
  if (t.closest("#sort-btn")) { sortMenu(t.closest("#sort-btn") as HTMLElement); return; }
  const g = t.closest<HTMLElement>("[data-group]"); if (g) { groupBy = g.dataset.group as Group; renderChrome(); void reload(false); return; }
  const k = t.closest<HTMLElement>("[data-kind]"); if (k) { kind = k.dataset.kind as Filter["kind"]; renderToolbar(); void reload(false); return; }
  if (t.closest('[data-act="clear"]')) { search = ""; ($("#search") as HTMLInputElement).value = ""; kind = "all"; renderToolbar(); void reload(false); return; }
  const b = t.closest<HTMLElement>("[data-bulk]");
  if (b) { const a = b.dataset.bulk!; const ids = targetIds();
    if (a === "clear") clearSelection(); else if (a === "fav") void bulkFavorite(); else if (a === "album") albumPicker(ids, b);
    else if (a === "tag") void addTags(ids); else if (a === "cw") void rotateIds(ids, 90);
    else if (a === "unalbum" && nav.type === "album") { await api.albumRemove(nav.id, ids); clearSelection(); await loadSidebarData(); await reload(true); }
    return; }
  if (t.closest("#rename-album") && nav.type === "album") { const cur = albums.find((a) => a.id === (nav as { id: number }).id); const name = await promptDialog({ title: "Rename album", label: "Album name", value: cur?.name, confirm: "Rename" }); if (name) { try { await api.renameAlbum((nav as { id: number }).id, name); await loadSidebarData(); } catch (er) { toast(errText(er), "error"); } } return; }
  if (t.closest("#delete-album") && nav.type === "album") { const cur = albums.find((a) => a.id === (nav as { id: number }).id); if (await confirmDialog({ title: "Delete album?", body: `Delete “${esc(cur?.name ?? "")}”? The photos stay in your library.`, confirm: "Delete album", danger: true })) { await api.deleteAlbum((nav as { id: number }).id); go({ type: "all" }); await loadSidebarData(); } return; }
  // inspector
  const ia = t.closest<HTMLElement>("[data-ia]")?.dataset.ia;
  if (ia && activeId != null) {
    const item = grid.itemAt(grid.indexOf(activeId));
    if (ia === "fav") { const d = await api.detail(activeId); await toggleFavorite([activeId], !d.favorite); }
    else if (ia === "rotl") void rotateIds([activeId], -90); else if (ia === "rotr") void rotateIds([activeId], 90);
    else if (ia === "album") albumPicker([activeId], t.closest("[data-ia]") as HTMLElement);
    else if (ia === "reveal" || ia === "external") { const d = await api.detail(activeId); (ia === "reveal" ? api.reveal(d.path) : api.openExternal(d.path)).catch((er) => toast(errText(er), "error")); }
    else if (ia === "copygps") { const d = await api.detail(activeId); await navigator.clipboard?.writeText(`${d.lat}, ${d.lon}`).then(() => toast("Coordinates copied"), () => toast("Couldn’t access the clipboard", "error")); }
    void item; return;
  }
  const un = t.closest<HTMLElement>("[data-untag]"); if (un && activeId != null) { await api.removeTag([activeId], un.dataset.untag!); await loadSidebarData(); renderInspector(); return; }
  const ua = t.closest<HTMLElement>("[data-unalbum]"); if (ua && activeId != null) { await api.albumRemove(Number(ua.dataset.unalbum), [activeId]); await loadSidebarData(); renderInspector(); if (nav.type === "album") void reload(true); return; }
  const tl = t.closest<HTMLElement>("[data-tag]"); if (tl) { go({ type: "tag", tag: tl.dataset.tag! }); return; }
  const al = t.closest<HTMLElement>("[data-album]"); if (al) { go({ type: "album", id: Number(al.dataset.album) }); return; }
  if (t.closest('[data-act="open"]') && activeId != null) { const i = grid.indexOf(activeId); if (i >= 0) void viewer.open(i); return; }
  // duplicates
  if (t.closest("#dupe-run")) { void runDupes(); return; }
  if (t.closest("#dupe-cancel")) { void api.cancelDuplicates(); return; }
  const rv = t.closest<HTMLElement>("[data-reveal]"); if (rv) { api.reveal(rv.dataset.reveal!).catch((er) => toast(errText(er), "error")); return; }
});
document.addEventListener("submit", async (e) => {
  if ((e.target as HTMLElement).id !== "tag-form") return;
  e.preventDefault();
  const input = $("#tag-input") as HTMLInputElement; const v = input.value.trim();
  if (!v || activeId == null) return;
  try { await api.addTag([activeId], v); await loadSidebarData(); renderInspector(); } catch (er) { toast(errText(er), "error"); }
});

function sortMenu(btn: HTMLElement) {
  const r = btn.getBoundingClientRect();
  const keys: Sort["key"][] = ["date", "name", "added", "size", "kind"];
  const apply = (s: Sort) => { sort = s; if (s.key !== "date") groupBy = "none"; void api.setSetting("sort_key", s.key); void api.setSetting("sort_desc", s.desc ? "1" : "0"); renderChrome(); void reload(false); };
  showMenu(r.left, r.bottom + 6, [
    { header: "Sort by" }, ...keys.map<MenuItem>((k) => ({ label: SORT_LABEL[k], checked: sort.key === k, run: () => apply({ key: k, desc: k === sort.key ? sort.desc : k === "date" || k === "added" || k === "size" }) })),
    { separator: true }, { label: "Ascending", checked: !sort.desc, run: () => apply({ ...sort, desc: false }) }, { label: "Descending", checked: sort.desc, run: () => apply({ ...sort, desc: true }) },
  ], btn);
}
function toggleInspector() { inspectorOpen = !inspectorOpen; store.set("gallerydesk.inspector", inspectorOpen ? "1" : "0"); renderChrome(); }

$("#search").addEventListener("input", debounce(() => { search = ($("#search") as HTMLInputElement).value; void reload(false); }, 180));
const saveTile = debounce(() => void api.setSetting("tile", String(tile)), 400);
document.addEventListener("input", (e) => { const t = e.target as HTMLInputElement; if (t.id === "density") { tile = Number(t.value); grid.setTile(tile); saveTile(); } });

// keyboard
document.addEventListener("keydown", (e) => {
  const tgt = e.target as HTMLElement;
  const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(tgt.tagName) || tgt.isContentEditable;
  const mod = e.ctrlKey || e.metaKey;
  if (e.key === "Escape") {
    if (viewer.isOpen && !hasModal()) return; // viewer handles its own Esc
    if (closeTopOverlay()) { e.preventDefault(); return; }
    if (typing && tgt.id === "search") { (tgt as HTMLInputElement).value = ""; search = ""; (tgt as HTMLInputElement).blur(); void reload(false); return; }
    if (sel.size) clearSelection(); return;
  }
  if (viewer.isOpen || hasModal()) return;
  if (mod && e.key.toLowerCase() === "k") { e.preventDefault(); ($("#search") as HTMLInputElement).focus(); ($("#search") as HTMLInputElement).select(); return; }
  if (mod && e.key === ",") { e.preventDefault(); void openSettings(); return; }
  if (mod && /^[1-5]$/.test(e.key)) { e.preventDefault(); go({ type: (["all", "photos", "videos", "favorites", "timeline"] as const)[Number(e.key) - 1] }); return; }
  if (typing || nav.type === "duplicates") return;
  if (mod && e.key.toLowerCase() === "a") { e.preventDefault(); void api.ids(filter(), sort, 0, 100000).then((ids) => { sel = new Set(ids); grid.rerender(); renderToolbar(); renderInspector(); renderStatus(); }); return; }
  if (mod || e.altKey) return;
  const k = e.key;
  const move = async (to: number) => {
    to = Math.max(0, Math.min(grid.total - 1, to));
    const item = await grid.loadItem(to); if (!item) return;
    if (e.shiftKey) { const [a, b] = [Math.min(anchor, to), Math.max(anchor, to)]; sel = new Set(await api.ids(filter(), sort, a, b - a + 1)); activeId = item.id; cursor = to; grid.rerender(); renderToolbar(); renderInspector(); renderStatus(); }
    else selectOnly(to, item);
    grid.scrollToIndex(to, "nearest");
  };
  const rows = Math.max(1, Math.floor(grid.root.clientHeight / Math.max(1, grid.rowH)));
  if (!grid.total) return;
  if (k === "ArrowRight") { e.preventDefault(); void move(cursor + 1); }
  else if (k === "ArrowLeft") { e.preventDefault(); void move(cursor - 1); }
  else if (k === "ArrowDown") { e.preventDefault(); void move(cursor + grid.cols); }
  else if (k === "ArrowUp") { e.preventDefault(); void move(cursor - grid.cols); }
  else if (k === "PageDown") { e.preventDefault(); void move(cursor + grid.cols * rows); }
  else if (k === "PageUp") { e.preventDefault(); void move(cursor - grid.cols * rows); }
  else if (k === "Home") { e.preventDefault(); void move(0); }
  else if (k === "End") { e.preventDefault(); void move(grid.total - 1); }
  else if ((k === "Enter" || k === " ") && activeId != null) { e.preventDefault(); void viewer.open(cursor); }
  else if (k.toLowerCase() === "f" && targetIds().length) { void bulkFavorite(); }
  else if (k.toLowerCase() === "r" && targetIds().length) { void rotateIds(targetIds(), e.shiftKey ? -90 : 90); }
  else if (k.toLowerCase() === "i") toggleInspector();
});

// ------------------------------------------------------------------ boot
let wasRunning = false;
let videoGuardTripped = false;
async function boot() {
  try {
    S = await api.settings();
    if (S.video_guard === "1") {
      // The built-in player never reported back last time (the app was frozen or killed while opening a video).
      await api.setSetting("video_builtin", "0").catch(() => {}); await api.setSetting("video_guard", "0").catch(() => {});
      S.video_builtin = "0"; S.video_guard = "0"; videoGuardTripped = true;
    }
    fallback.video = S.video_builtin !== "0" && await api.videoSupported().catch(() => false);
    applyTheme((S.theme as ThemePref) ?? "system", S.accent);
    sort = { key: (S.sort_key as Sort["key"]) || "date", desc: S.sort_desc !== "0" };
    tile = Number(S.tile) || 190;
    grid.setTile(tile); grid.setFit(S.fit === "contain"); grid.setCaptions(S.captions !== "0");
    inspectorOpen = store.get("gallerydesk.inspector", window.innerWidth >= 1280 ? "1" : "0") === "1";

    await on<Progress>("scan-progress", (p) => {
      const was = scan.running; scan = p;
      renderSideFoot(); renderStatus();
      if (nav.type !== "duplicates" && p.running && layoutTotal === 0) renderEmpty();
      if (was && !p.running) {
        wasRunning = false; softReload();
        if (p.cancelled) toast("Indexing cancelled — it will resume next time", "info");
        else if (p.failed) toast(`Indexing finished · ${fmtNum(p.failed)} file${p.failed === 1 ? "" : "s"} couldn’t be previewed`, "info");
        else if (p.processed || p.discovered) toast("Library is up to date", "success");
      } else wasRunning = p.running;
    });
    await on("library-changed", () => softReload());
    await on<[number, number]>("dupes-progress", ([done, total]) => { dupeRun = { done, total }; if (nav.type === "duplicates") renderDupes(); });

    await loadSidebarData();
    await reload(false);
    await api.ready();
    const st = await api.scanStatus(); if (st.running) { scan = st; renderSideFoot(); }
    if (videoGuardTripped) toast("The built-in video player stopped responding last time, so videos now open in your default player. You can switch it back on in Settings → Library.", "info");
  } catch (e) {
    $("#empty").hidden = false;
    $("#empty").innerHTML = emptyState("alert", "Desktop backend unavailable", `GalleryDesk needs its Rust backend. Run it with <code>npm run tauri dev</code> rather than opening the page in a browser.<br><small>${esc(errText(e))}</small>`);
  }
}
void boot();
