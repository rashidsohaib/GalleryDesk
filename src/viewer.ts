import { api, src, errText, type Detail, type Media } from "./api";
import { icon } from "./icons";
import { esc, fmtBytes, fmtDate, fmtDuration, fmtNum, toast } from "./ui";

const SAFE = new Set(["jpg", "jpeg", "jpe", "jfif", "png", "gif", "webp", "bmp", "avif"]);
const ext = (n: string) => n.split(".").pop()?.toLowerCase() ?? "";
const ITEM_W = 96;          // film-strip slot width (thumbnail + gap)
const STRIP_PAD = 14;       // left padding inside the strip track
const INTERVALS = [3000, 5000, 8000];
const RATES = [0.5, 1, 1.25, 1.5, 2];
const pref = {
  get: (k: string, d: string) => { try { return localStorage.getItem(k) ?? d; } catch { return d; } },
  set: (k: string, v: string) => { try { localStorage.setItem(k, v); } catch { /* private mode */ } },
};

export type ViewerHooks = {
  load(i: number): Promise<Media | undefined>;
  total(): number;
  /** Already-loaded item (no fetch) — used to paint the film-strip synchronously. */
  peek(i: number): Media | undefined;
  onFavorite(item: Media): Promise<void>;
  onRotate(item: Media, delta: number): Promise<void>;
  onIndex(i: number, item: Media): void;
  onClose(): void;
};

/** Full-screen viewer: images with zoom/pan, videos with native controls, keyboard navigation. */
export class Viewer {
  private el: HTMLElement | null = null;
  private i = 0;
  private item: Media | null = null;
  private token = 0;
  private scale = 1; private fit = 1; private tx = 0; private ty = 0; private fitMode = true;
  private nat = { w: 0, h: 0 };
  private info = false;
  private stripOn = pref.get("gallerydesk.strip", "1") === "1";
  private playing = false;
  private interval = Number(pref.get("gallerydesk.slide", "5000")) || 5000;
  private slideTimer = 0;
  private stripNodes = new Map<number, HTMLElement>();
  private stripRaf = 0;
  private converting = false;
  private videoCleanup: (() => void) | null = null;
  private key = (e: KeyboardEvent) => this.onKey(e);
  constructor(private hooks: ViewerHooks) {}

  get isOpen() { return !!this.el; }
  get current() { return this.item; }

  async open(i: number) {
    if (!this.el) {
      this.el = document.createElement("div");
      this.el.className = "viewer";
      this.el.setAttribute("role", "dialog");
      this.el.setAttribute("aria-modal", "true");
      this.el.setAttribute("aria-label", "Photo viewer");
      this.el.innerHTML = `
        <div class="v-top">
          <div class="v-title"><strong></strong><span></span></div>
          <div class="v-actions">
            <button class="v-btn" data-a="play" title="Slideshow (S)" aria-label="Start slideshow" aria-pressed="false">${icon("slides", 18)}</button>
            <button class="v-btn v-chip-btn" data-a="interval" title="Seconds per photo" aria-label="Slideshow speed">5s</button>
            <button class="v-btn" data-a="strip" title="Film-strip (T)" aria-label="Toggle film-strip">${icon("film", 18)}</button>
            <button class="v-btn" data-a="fav" title="Favorite (F)" aria-label="Favorite"></button>
            <button class="v-btn" data-a="ccw" title="Rotate left (Shift+R)" aria-label="Rotate left">${icon("ccw", 18)}</button>
            <button class="v-btn" data-a="cw" title="Rotate right (R)" aria-label="Rotate right">${icon("cw", 18)}</button>
            <button class="v-btn" data-a="fit" title="Fit / 100% (0 / 1)" aria-label="Fit to window">${icon("fit", 18)}</button>
            <button class="v-btn" data-a="info" title="Details (I)" aria-label="Toggle details">${icon("info", 18)}</button>
            <button class="v-btn" data-a="ext" title="Open with default app" aria-label="Open with default app">${icon("external", 18)}</button>
            <button class="v-btn" data-a="close" title="Close (Esc)" aria-label="Close viewer">${icon("x", 18)}</button>
          </div>
        </div>
        <button class="v-nav prev" data-a="prev" aria-label="Previous">${icon("left", 26)}</button>
        <button class="v-nav next" data-a="next" aria-label="Next">${icon("right", 26)}</button>
        <div class="v-stage" tabindex="-1"></div>
        <div class="v-strip" role="listbox" aria-label="Film-strip" tabindex="-1"><div class="v-strip-track"></div></div>
        <div class="v-progress" aria-hidden="true"><i></i></div>
        <aside class="v-info" hidden></aside>`;
      document.body.appendChild(this.el);
      this.el.addEventListener("click", (e) => {
        const a = (e.target as HTMLElement).closest<HTMLElement>("[data-a]")?.dataset.a;
        if (a) this.act(a);
      });
      this.bindStage();
      this.bindStrip();
      this.el.classList.toggle("strip-on", this.stripOn);
      this.paintInterval();
      document.addEventListener("keydown", this.key, true);
      window.addEventListener("resize", this.onResize);
      requestAnimationFrame(() => this.el?.classList.add("show"));
    }
    await this.show(i);
  }

  close() {
    if (!this.el) return;
    this.token++;
    this.stopSlideshow();
    this.teardownVideo();
    document.removeEventListener("keydown", this.key, true);
    window.removeEventListener("resize", this.onResize);
    window.removeEventListener("resize", this.onStripResize);
    this.el.remove(); this.el = null; this.item = null;
    this.stripNodes.clear();
    this.hooks.onClose();
  }

  refresh(item: Media) { if (this.item && this.item.id === item.id) { this.item = item; this.paintChrome(); } }

  private onResize = () => { if (this.fitMode && this.nat.w) this.applyFit(); };

  private act(a: string) {
    if (!this.item) return;
    if (a === "close") this.close();
    else if (a === "play") this.toggleSlideshow();
    else if (a === "interval") { this.interval = INTERVALS[(INTERVALS.indexOf(this.interval) + 1) % INTERVALS.length]; pref.set("gallerydesk.slide", String(this.interval)); this.paintInterval(); if (this.playing) this.armSlideshow(); }
    else if (a === "strip") this.toggleStrip();
    else if (a === "prev") void this.show(this.i - 1);
    else if (a === "next") void this.show(this.i + 1);
    else if (a === "fav") void this.hooks.onFavorite(this.item).then(() => this.paintChrome());
    else if (a === "cw" || a === "ccw") void this.rotate(a === "cw" ? 90 : -90);
    else if (a === "fit") this.toggleZoom();
    else if (a === "info") this.toggleInfo();
    else if (a === "ext") api.openExternal(this.item.path).catch((e) => toast(errText(e), "error"));
  }

  private async rotate(delta: number) {
    if (!this.item || this.item.kind === "video") return;
    await this.hooks.onRotate(this.item, delta);
    this.item = { ...this.item, rotation: (this.item.rotation + delta + 360) % 360 };
    await this.render();
  }

  private async show(i: number) {
    const total = this.hooks.total();
    if (i < 0 || i >= total) return;
    const item = await this.hooks.load(i);
    if (!item || !this.el) return;
    this.i = i; this.item = item;
    this.hooks.onIndex(i, item);
    this.paintStrip(true);
    await this.render();
    // Warm the neighbours so stepping through feels instant.
    for (const j of [i + 1, i - 1]) void this.hooks.load(j).then((m) => { if (m && m.kind === "image" && m.rotation === 0 && SAFE.has(ext(m.name))) new Image().src = src(m.path); });
  }

  private paintChrome() {
    if (!this.el || !this.item) return;
    const m = this.item;
    this.el.querySelector(".v-title strong")!.textContent = m.name;
    this.el.querySelector(".v-title span")!.textContent = `${fmtNum(this.i + 1)} of ${fmtNum(this.hooks.total())} · ${fmtDate(m.date)}`;
    const fav = this.el.querySelector<HTMLElement>('[data-a="fav"]')!;
    fav.innerHTML = icon("heart", 18); fav.classList.toggle("on", m.favorite);
    fav.setAttribute("aria-pressed", String(m.favorite));
    for (const a of ["ccw", "cw", "fit"]) this.el.querySelector<HTMLButtonElement>(`[data-a="${a}"]`)!.hidden = m.kind === "video";
    this.el.querySelectorAll(".v-strip .v-th").forEach((n) => n.classList.toggle("on", Number((n as HTMLElement).dataset.i) === this.i));
    this.el.querySelector<HTMLButtonElement>('[data-a="prev"]')!.hidden = this.i <= 0;
    this.el.querySelector<HTMLButtonElement>('[data-a="next"]')!.hidden = this.i >= this.hooks.total() - 1;
  }

  private async render() {
    const el = this.el, m = this.item;
    if (!el || !m) return;
    const my = ++this.token;
    this.paintChrome();
    const stage = el.querySelector<HTMLElement>(".v-stage")!;
    this.teardownVideo();
    this.disarmSlideshow();
    stage.innerHTML = `<div class="v-loading"><span class="spin"></span></div>`;
    if (this.info) void this.paintInfo();

    if (m.kind === "video") {
      void this.startVideo(stage, m, my);
      return;
    }
    const useOriginal = m.rotation === 0 && SAFE.has(ext(m.name));
    const load = (url: string) => new Promise<HTMLImageElement>((res, rej) => {
      const img = new Image(); img.className = "v-img"; img.draggable = false; img.alt = m.name;
      img.onload = () => res(img); img.onerror = () => rej(new Error("decode"));
      img.src = url;
    });
    let img: HTMLImageElement;
    try {
      try { img = useOriginal ? await load(src(m.path)) : await load(src(await api.preview(m.id))); }
      catch { if (!useOriginal) throw new Error("preview"); img = await load(src(await api.preview(m.id))); }
    } catch (e) {
      if (my !== this.token) return;
      stage.innerHTML = `<div class="v-fallback">${icon("alert", 34)}<h3>This image can’t be displayed</h3>
        <p>${esc(e instanceof Error && e.message !== "preview" && e.message !== "decode" ? e.message : "Its format needs a decoder your FFmpeg build doesn’t include.")}</p>
        <button class="btn primary" data-a="ext">${icon("external", 15)} Open with default app</button></div>`;
      return;
    }
    if (my !== this.token) return;
    stage.innerHTML = "";
    stage.appendChild(img);
    this.nat = { w: img.naturalWidth, h: img.naturalHeight };
    this.fitMode = true; this.applyFit();
    if (this.playing) this.armSlideshow();
  }

  // ------------------------------------------------------------ video player
  private teardownVideo() {
    if (this.converting) { this.converting = false; void api.cancelVideoPlayback().catch(() => {}); }
    this.videoCleanup?.(); this.videoCleanup = null;
    this.el?.querySelector("video")?.pause();
  }

  /** Honour the "built-in player" switch before touching the webview's media stack at all. */
  private async startVideo(stage: HTMLElement, m: Media, my: number) {
    const s = await api.settings().catch(() => ({} as Record<string, string>));
    if (my !== this.token || !this.el) return;
    const supported = await api.videoSupported().catch(() => true);
    if (my !== this.token || !this.el) return;
    if (s.video_builtin === "0" || !supported) {
      const why = supported
        ? "The built-in player is switched off (Settings → Library). It is turned off automatically if it ever stops responding, so a freeze can’t repeat."
        : "This system has no GStreamer media plugins that the built-in player needs (install <b>gstreamer1.0-plugins-base</b> and <b>-good</b>, or your distro’s equivalent). Your video is untouched.";
      stage.innerHTML = `<div class="v-fallback">${icon("video", 34)}<h3>Videos open in your default player</h3>
        <p>${why}</p>
        <button class="btn primary" data-a="ext">${icon("external", 15)} Open with default app</button></div>`;
      return;
    }
    this.renderVideo(stage, m, my);
  }

  private renderVideo(stage: HTMLElement, m: Media, my: number) {
    stage.innerHTML = `<div class="vp idle-off" tabindex="-1">
      <video class="v-video" preload="metadata" playsinline></video>
      <div class="vp-busy" hidden><span class="spin"></span><p></p></div>
      <button class="vp-big" aria-label="Play">${icon("play", 34)}</button>
      <div class="vp-bar">
        <button class="vp-btn" data-p="toggle" aria-label="Play">${icon("play", 20)}</button>
        <span class="vp-time" data-p="cur">0:00</span>
        <input class="vp-seek" type="range" min="0" max="1000" step="1" value="0" aria-label="Seek">
        <span class="vp-time" data-p="dur">${m.duration != null ? fmtDuration(m.duration) : "–:––"}</span>
        <button class="vp-btn" data-p="mute" aria-label="Mute">${icon("vol", 20)}</button>
        <input class="vp-vol" type="range" min="0" max="1" step="0.05" value="1" aria-label="Volume">
        <button class="vp-btn txt" data-p="rate" aria-label="Playback speed">1×</button>
      </div></div>`;
    const box = stage.querySelector<HTMLElement>(".vp")!;
    const v = box.querySelector<HTMLVideoElement>("video")!;
    const $ = <T extends HTMLElement>(q: string) => box.querySelector<T>(q)!;
    const toggleBtn = $("[data-p=toggle]"), big = $(".vp-big"), seek = $<HTMLInputElement>(".vp-seek"), vol = $<HTMLInputElement>(".vp-vol");
    const cur = $("[data-p=cur]"), dur = $("[data-p=dur]"), muteBtn = $("[data-p=mute]"), rateBtn = $("[data-p=rate]"), busy = $(".vp-busy");
    const ac = new AbortController(); const sig = { signal: ac.signal };
    let usedFallback = false, scrubbing = false, idleT = 0, watchdog = 0;
    // Crash/freeze guard: raised just before the webview's media pipeline is asked to open the file and
    // lowered once it answers. If the app freezes in between, the flag survives and the next launch
    // switches the built-in player off (see boot()).
    const guard = (on: boolean) => { void api.setSetting("video_guard", on ? "1" : "0").catch(() => {}); };

    v.volume = Math.min(1, Math.max(0, Number(pref.get("gallerydesk.vol", "1")))); v.muted = pref.get("gallerydesk.mute", "0") === "1";
    let rate = 1;
    const paintVol = () => { vol.value = String(v.muted ? 0 : v.volume); muteBtn.innerHTML = icon(v.muted || v.volume === 0 ? "mute" : "vol", 20); };
    const paintPlay = () => { const p = !v.paused && !v.ended; toggleBtn.innerHTML = icon(p ? "pause" : "play", 20); toggleBtn.setAttribute("aria-label", p ? "Pause" : "Play"); box.classList.toggle("playing", p); };
    const wake = () => { box.classList.remove("idle"); clearTimeout(idleT); if (!v.paused) idleT = window.setTimeout(() => box.classList.add("idle"), 2400); };
    paintVol(); paintPlay();

    const play = () => { void v.play().catch(() => { /* needs a gesture or failed — the error handler covers failures */ }); };
    const togglePlay = () => { if (v.paused || v.ended) play(); else v.pause(); };
    const jump = (d: number) => { if (isFinite(v.duration)) v.currentTime = Math.max(0, Math.min(v.duration, v.currentTime + d)); };
    const showFail = (why: string) => {
      if (my !== this.token) return;
      this.converting = false; clearTimeout(watchdog); guard(false);
      stage.innerHTML = `<div class="v-fallback">${icon("video", 34)}<h3>This video can’t be played in the built-in player</h3>
        <p>${esc(why)} GalleryDesk never converts or modifies originals — you can open it in your default video player instead.</p>
        <button class="btn primary" data-a="ext">${icon("external", 15)} Open with default app</button></div>`;
    };
    const fallback = async (why: string) => {
      if (my !== this.token) return;
      if (usedFallback) { showFail(why); return; }
      usedFallback = true; clearTimeout(watchdog);
      busy.hidden = false; busy.querySelector("p")!.textContent = "Preparing this video for playback… (a temporary copy is made in the cache; your original is untouched)";
      big.hidden = true; this.converting = true;
      try {
        const out = await api.videoPlayback(m.id);
        if (my !== this.token || !this.el) return;
        this.converting = false; busy.hidden = true; big.hidden = false;
        guard(true); v.src = out; v.load(); play();
      } catch (e) {
        if (my !== this.token) return;
        const t = errText(e);
        if (t === "cancelled") return;
        showFail(t);
      }
    };

    v.addEventListener("loadedmetadata", () => { clearTimeout(watchdog); guard(false); if (isFinite(v.duration)) dur.textContent = fmtDuration(v.duration); }, sig);
    v.addEventListener("timeupdate", () => { cur.textContent = fmtDuration(v.currentTime); if (!scrubbing && isFinite(v.duration) && v.duration > 0) seek.value = String(Math.round((v.currentTime / v.duration) * 1000)); }, sig);
    v.addEventListener("play", () => { paintPlay(); wake(); }, sig);
    v.addEventListener("pause", () => { paintPlay(); wake(); }, sig);
    v.addEventListener("ended", () => { paintPlay(); wake(); if (this.playing) void this.show(this.i + 1 < this.hooks.total() ? this.i + 1 : 0); }, sig);
    v.addEventListener("volumechange", () => { pref.set("gallerydesk.vol", String(v.volume)); pref.set("gallerydesk.mute", v.muted ? "1" : "0"); paintVol(); }, sig);
    v.addEventListener("waiting", () => { if (!this.converting) busy.hidden = false, busy.querySelector("p")!.textContent = ""; }, sig);
    v.addEventListener("playing", () => { if (!this.converting) busy.hidden = true; }, sig);
    v.addEventListener("canplay", () => { if (!this.converting) busy.hidden = true; }, sig);
    v.addEventListener("error", () => { void fallback("Your system’s media codecs don’t cover this format."); }, sig);

    big.addEventListener("click", togglePlay, sig);
    v.addEventListener("click", togglePlay, sig);
    toggleBtn.addEventListener("click", togglePlay, sig);
    muteBtn.addEventListener("click", () => { v.muted = !v.muted; if (!v.muted && v.volume === 0) v.volume = 0.6; }, sig);
    vol.addEventListener("input", () => { v.volume = Number(vol.value); v.muted = v.volume === 0; }, sig);
    rateBtn.addEventListener("click", () => { rate = RATES[(RATES.indexOf(rate) + 1) % RATES.length]; v.playbackRate = rate; rateBtn.textContent = `${rate}×`; }, sig);
    seek.addEventListener("pointerdown", () => { scrubbing = true; }, sig);
    seek.addEventListener("input", () => { if (isFinite(v.duration)) { v.currentTime = (Number(seek.value) / 1000) * v.duration; cur.textContent = fmtDuration(v.currentTime); } }, sig);
    const endScrub = () => { scrubbing = false; };
    seek.addEventListener("pointerup", endScrub, sig); seek.addEventListener("change", endScrub, sig); seek.addEventListener("blur", endScrub, sig);
    box.addEventListener("mousemove", wake, sig);
    box.addEventListener("keydown", (e) => { if ((e.target as HTMLElement).tagName === "INPUT" && [" ", "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(e.key)) e.stopPropagation(); }, sig);

    // Keyboard hooks used by onKey().
    (box as HTMLElement & { vp?: unknown }).vp = { togglePlay, jump, mute: () => { v.muted = !v.muted; }, volume: (d: number) => { v.muted = false; v.volume = Math.max(0, Math.min(1, v.volume + d)); } };
    this.videoCleanup = () => { ac.abort(); clearTimeout(idleT); clearTimeout(watchdog); guard(false); v.pause(); v.removeAttribute("src"); v.load(); };

    // Some WebKit builds never raise `error` for a codec they can't decode — give it a few seconds.
    watchdog = window.setTimeout(() => { if (v.readyState < 1 && !usedFallback) void fallback("Your system’s media codecs don’t cover this format."); }, 7000);
    api.videoUrl(m.id).then((u) => { if (my !== this.token || !this.el) return; guard(true); v.src = u; play(); }).catch((e) => showFail(errText(e)));
  }

  // ------------------------------------------------------------ slideshow
  private toggleSlideshow() { if (this.playing) this.stopSlideshow(); else this.startSlideshow(); }
  private startSlideshow() {
    this.playing = true; this.paintPlaying();
    if (this.item?.kind === "image") this.armSlideshow();
    else this.vpApi()?.togglePlay();
  }
  private stopSlideshow() { this.playing = false; this.disarmSlideshow(); this.paintPlaying(); }
  private disarmSlideshow() { clearTimeout(this.slideTimer); this.slideTimer = 0; const b = this.el?.querySelector<HTMLElement>(".v-progress i"); if (b) { b.style.transition = "none"; b.style.width = "0"; } }
  private armSlideshow() {
    this.disarmSlideshow();
    if (!this.el || !this.playing || this.item?.kind !== "image") return;
    const bar = this.el.querySelector<HTMLElement>(".v-progress i")!;
    bar.style.transition = "none"; bar.style.width = "0";
    void bar.offsetWidth; bar.style.transition = `width ${this.interval}ms linear`; bar.style.width = "100%";
    this.slideTimer = window.setTimeout(() => { void this.show(this.i + 1 < this.hooks.total() ? this.i + 1 : 0); }, this.interval);
  }
  private paintPlaying() {
    const b = this.el?.querySelector<HTMLElement>('[data-a="play"]'); if (!b) return;
    b.innerHTML = icon(this.playing ? "pause" : "slides", 18); b.classList.toggle("on-accent", this.playing);
    b.setAttribute("aria-pressed", String(this.playing)); b.setAttribute("aria-label", this.playing ? "Pause slideshow" : "Start slideshow");
    b.title = this.playing ? "Pause slideshow (S)" : "Slideshow (S)";
  }
  private paintInterval() { const b = this.el?.querySelector<HTMLElement>('[data-a="interval"]'); if (b) b.textContent = `${this.interval / 1000}s`; }
  private vpApi(): { togglePlay(): void; jump(d: number): void; mute(): void; volume(d: number): void } | null {
    return ((this.el?.querySelector(".vp") as (HTMLElement & { vp?: never }) | null)?.vp as never) ?? null;
  }

  // ------------------------------------------------------------ film-strip
  private toggleStrip() {
    this.stripOn = !this.stripOn; pref.set("gallerydesk.strip", this.stripOn ? "1" : "0");
    this.el!.classList.toggle("strip-on", this.stripOn);
    if (this.stripOn) this.paintStrip(true);
    window.setTimeout(this.onResize, 0);
  }
  private bindStrip() {
    const st = this.el!.querySelector<HTMLElement>(".v-strip")!;
    st.addEventListener("scroll", () => { if (!this.stripRaf) this.stripRaf = requestAnimationFrame(() => { this.stripRaf = 0; this.paintStrip(false); }); }, { passive: true });
    st.addEventListener("wheel", (e) => { if (Math.abs(e.deltaY) > Math.abs(e.deltaX)) { e.preventDefault(); st.scrollLeft += e.deltaY; } }, { passive: false });
    st.addEventListener("click", (e) => {
      const b = (e.target as HTMLElement).closest<HTMLElement>(".v-th");
      if (b) { e.stopPropagation(); b.blur(); void this.show(Number(b.dataset.i)); }
    });
    window.addEventListener("resize", this.onStripResize);
  }
  private onStripResize = () => { if (this.el) this.paintStrip(false); };

  private paintStrip(center: boolean) {
    if (!this.el || !this.stripOn) return;
    const st = this.el.querySelector<HTMLElement>(".v-strip")!;
    const track = st.firstElementChild as HTMLElement;
    const total = this.hooks.total();
    track.style.width = `${total * ITEM_W + STRIP_PAD * 2}px`;
    if (center) {
      const want = Math.max(0, STRIP_PAD + this.i * ITEM_W + ITEM_W / 2 - st.clientWidth / 2);
      const far = Math.abs(want - st.scrollLeft) > st.clientWidth * 1.5;
      st.scrollTo({ left: want, behavior: far ? "auto" : "smooth" });
      if (far) st.scrollLeft = want;
    }
    const left = st.scrollLeft;
    const lo = Math.max(0, Math.floor((left - STRIP_PAD) / ITEM_W) - 5);
    const hi = Math.min(total - 1, Math.ceil((left + st.clientWidth) / ITEM_W) + 5);
    for (const [k, n] of this.stripNodes) if (k < lo || k > hi) { n.remove(); this.stripNodes.delete(k); }
    const missing: number[] = [];
    for (let j = lo; j <= hi; j++) {
      const m = this.hooks.peek(j);
      let n = this.stripNodes.get(j);
      if (!n) {
        n = document.createElement("button");
        n.className = "v-th"; n.dataset.i = String(j); n.setAttribute("role", "option");
        n.style.left = `${STRIP_PAD + j * ITEM_W}px`;
        track.appendChild(n); this.stripNodes.set(j, n);
      }
      n.classList.toggle("on", j === this.i);
      n.setAttribute("aria-selected", String(j === this.i));
      if (m && n.dataset.id !== String(m.id)) {
        n.dataset.id = String(m.id); n.title = m.name; n.setAttribute("aria-label", m.name);
        const inner = m.thumbnail ? `<img src="${esc(src(m.thumbnail))}" alt="" draggable="false" decoding="async">` : `<span class="v-th-ph">${icon(m.kind === "video" ? "video" : "image", 18)}</span>`;
        n.innerHTML = inner + (m.kind === "video" ? `<span class="v-th-badge">${icon("play", 9)}${m.duration != null ? fmtDuration(m.duration) : ""}</span>` : "");
      } else if (!m) missing.push(j);
    }
    if (missing.length) {
      const first = missing[0], last = missing[missing.length - 1];
      void Promise.all([this.hooks.load(first), this.hooks.load(last)]).then(() => { if (this.el) this.paintStrip(false); });
    }
  }

  // ------------------------------------------------------------ zoom / pan
  private stageEl() { return this.el!.querySelector<HTMLElement>(".v-stage")!; }
  private imgEl() { return this.el?.querySelector<HTMLImageElement>(".v-img") ?? null; }

  private applyFit() {
    const st = this.stageEl().getBoundingClientRect();
    const pad = 28; // breathing room so the photo never touches the window edge
    this.fit = Math.min((st.width - pad * 2) / this.nat.w, (st.height - pad * 2) / this.nat.h, 1);
    this.scale = this.fit;
    this.tx = (st.width - this.nat.w * this.scale) / 2; this.ty = (st.height - this.nat.h * this.scale) / 2;
    this.fitMode = true; this.paint();
  }
  private paint() {
    const img = this.imgEl(); if (!img) return;
    img.style.transform = `translate(${this.tx}px,${this.ty}px) scale(${this.scale})`;
    this.stageEl().classList.toggle("zoomed", this.scale > this.fit + 0.001);
  }
  private zoomAt(factor: number, cx: number, cy: number) {
    const ns = Math.max(this.fit * 0.999, Math.min(this.scale * factor, 8));
    const k = ns / this.scale;
    this.tx = cx - (cx - this.tx) * k; this.ty = cy - (cy - this.ty) * k; this.scale = ns;
    this.fitMode = Math.abs(ns - this.fit) < 0.001;
    this.clamp(); this.paint();
  }
  private clamp() {
    const st = this.stageEl().getBoundingClientRect();
    const w = this.nat.w * this.scale, h = this.nat.h * this.scale;
    this.tx = w <= st.width ? (st.width - w) / 2 : Math.min(0, Math.max(st.width - w, this.tx));
    this.ty = h <= st.height ? (st.height - h) / 2 : Math.min(0, Math.max(st.height - h, this.ty));
  }
  private toggleZoom(cx?: number, cy?: number) {
    if (!this.imgEl()) return;
    const st = this.stageEl().getBoundingClientRect();
    if (this.scale > this.fit + 0.001) this.applyFit();
    else this.zoomAt(1 / this.scale, cx ?? st.width / 2, cy ?? st.height / 2);
  }
  private bindStage() {
    const st = this.stageEl();
    st.addEventListener("wheel", (e) => {
      if (!this.imgEl()) return;
      e.preventDefault();
      const r = st.getBoundingClientRect();
      this.zoomAt(Math.exp(-e.deltaY * 0.0015), e.clientX - r.left, e.clientY - r.top);
    }, { passive: false });
    let drag: { x: number; y: number; tx: number; ty: number; moved: boolean } | null = null;
    st.addEventListener("pointerdown", (e) => {
      if (!this.imgEl() || e.button !== 0) return;
      drag = { x: e.clientX, y: e.clientY, tx: this.tx, ty: this.ty, moved: false };
      st.setPointerCapture(e.pointerId); st.classList.add("dragging");
    });
    st.addEventListener("pointermove", (e) => {
      if (!drag) return;
      const dx = e.clientX - drag.x, dy = e.clientY - drag.y;
      if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
      this.tx = drag.tx + dx; this.ty = drag.ty + dy; this.clamp(); this.paint();
    });
    const end = () => { drag = null; st.classList.remove("dragging"); };
    st.addEventListener("pointerup", end); st.addEventListener("pointercancel", end);
    st.addEventListener("dblclick", (e) => { const r = st.getBoundingClientRect(); this.toggleZoom(e.clientX - r.left, e.clientY - r.top); });
    st.addEventListener("click", (e) => { if (e.target === st && !this.imgEl()?.contains(e.target as Node)) { /* background click: keep open */ } });
  }

  // ------------------------------------------------------------ info panel
  private toggleInfo() {
    this.info = !this.info;
    const p = this.el!.querySelector<HTMLElement>(".v-info")!;
    p.hidden = !this.info;
    this.el!.classList.toggle("with-info", this.info);
    this.el!.querySelector('[data-a="info"]')!.classList.toggle("on", this.info);
    if (this.info) void this.paintInfo();
    window.setTimeout(this.onResize, 0);
  }
  private async paintInfo() {
    const m = this.item; if (!m || !this.el) return;
    try {
      const d: Detail = await api.detail(m.id);
      if (this.item?.id !== m.id || !this.el) return;
      const rows: [string, string][] = [
        ["Size", fmtBytes(d.bytes)], ["Dimensions", d.width && d.height ? `${d.width} × ${d.height}` : "—"],
        ["Taken", d.taken ? fmtDate(d.taken, { year: "numeric", month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }) : "—"],
        ["Camera", d.camera ?? "—"], ["Lens", d.lens ?? "—"],
        ["Exposure", [d.exposure, d.aperture ? `f/${d.aperture}` : null, d.iso ? `ISO ${d.iso}` : null].filter(Boolean).join(" · ") || "—"],
        ["Focal length", d.focal ? `${Math.round(d.focal)} mm` : "—"],
        ["Location", d.lat != null && d.lon != null ? `${d.lat.toFixed(4)}, ${d.lon.toFixed(4)}` : "—"],
      ];
      this.el.querySelector(".v-info")!.innerHTML = `<h3>${esc(d.name)}</h3><dl>${rows.map(([k, v]) => `<dt>${k}</dt><dd>${esc(v)}</dd>`).join("")}</dl><p class="v-path">${esc(d.path)}</p>`;
    } catch { /* item vanished */ }
  }

  // ------------------------------------------------------------ keyboard
  private onKey(e: KeyboardEvent) {
    if (!this.el || e.ctrlKey || e.metaKey || e.altKey) return;
    // Let open menus/dialogs above the viewer handle their own keys.
    if (document.querySelector(".modal-back,.menu")) return;
    const k = e.key;
    let handled = true;
    const vp = this.item?.kind === "video" ? this.vpApi() : null;
    const typing = (e.target as HTMLElement | null)?.tagName === "INPUT" && (e.target as HTMLInputElement).type !== "range";
    if (typing) return;
    if (k === "Escape") this.close();
    else if (k === " ") { if (vp) vp.togglePlay(); else this.toggleSlideshow(); }
    else if (k.toLowerCase() === "s") this.toggleSlideshow();
    else if (k.toLowerCase() === "t") this.toggleStrip();
    else if (vp && (k.toLowerCase() === "j" || (e.shiftKey && k === "ArrowLeft"))) vp.jump(e.shiftKey ? -5 : -10);
    else if (vp && (k.toLowerCase() === "l" || (e.shiftKey && k === "ArrowRight"))) vp.jump(e.shiftKey ? 5 : 10);
    else if (vp && k.toLowerCase() === "m") vp.mute();
    else if (vp && k === "ArrowUp") vp.volume(0.1);
    else if (vp && k === "ArrowDown") vp.volume(-0.1);
    else if (k === "ArrowRight" || k === "PageDown") void this.show(this.i + 1);
    else if (k === "ArrowLeft" || k === "PageUp") void this.show(this.i - 1);
    else if (k === "Home") void this.show(0);
    else if (k === "End") void this.show(this.hooks.total() - 1);
    else if (k === "+" || k === "=") { const r = this.stageEl().getBoundingClientRect(); this.zoomAt(1.25, r.width / 2, r.height / 2); }
    else if (k === "-") { const r = this.stageEl().getBoundingClientRect(); this.zoomAt(0.8, r.width / 2, r.height / 2); }
    else if (k === "0") this.applyFit();
    else if (k === "1") { const r = this.stageEl().getBoundingClientRect(); if (this.imgEl()) this.zoomAt(1 / this.scale, r.width / 2, r.height / 2); }
    else if (k.toLowerCase() === "f") this.act("fav");
    else if (k.toLowerCase() === "r") this.act(e.shiftKey ? "ccw" : "cw");
    else if (k.toLowerCase() === "i") this.toggleInfo();
    else handled = false;
    if (handled) { e.preventDefault(); e.stopPropagation(); }
  }
}
