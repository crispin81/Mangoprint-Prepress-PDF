// Plain script (no bundler): Tauri's APIs come from the global
// `window.__TAURI__` object, enabled by `app.withGlobalTauri` in
// tauri.conf.json. The dialog plugin adds `window.__TAURI__.dialog`.
const { invoke } = window.__TAURI__.core;
const { open, save, ask } = window.__TAURI__.dialog;
const { getCurrentWindow } = window.__TAURI__.window;

const appWindow = getCurrentWindow();

const els = {
  exportBtn: document.getElementById("exportBtn"),
  emptyOpenBtn: document.getElementById("emptyOpenBtn"),
  fileName: document.getElementById("fileName"),
  prevPage: document.getElementById("prevPage"),
  nextPage: document.getElementById("nextPage"),
  pageLabel: document.getElementById("pageLabel"),
  rasterDpi: document.getElementById("rasterDpi"),
  gsWarning: document.getElementById("gsWarning"),
  preview: document.getElementById("preview"),
  loadingOverlay: document.getElementById("loadingOverlay"),
  overprintToggle: document.getElementById("overprintToggle"),
  separationsList: document.getElementById("separationsList"),
  selectAllSep: document.getElementById("selectAllSep"),
  selectNoneSep: document.getElementById("selectNoneSep"),
  boxOverlay: document.getElementById("boxOverlay"),
  pageBoxes: document.getElementById("pageBoxes"),
  pageRotation: document.getElementById("pageRotation"),
  rotationCurrent: document.getElementById("rotationCurrent"),
  winMin: document.getElementById("winMin"),
  winMax: document.getElementById("winMax"),
  winClose: document.getElementById("winClose"),
  viewport: document.getElementById("viewport"),
  imgWrap: document.getElementById("imgWrap"),
  vectorLayer: document.getElementById("vectorLayer"),
  zoomOut: document.getElementById("zoomOut"),
  zoomIn: document.getElementById("zoomIn"),
  zoomFit: document.getElementById("zoomFit"),
  zoomActual: document.getElementById("zoomActual"),
  zoomLabel: document.getElementById("zoomLabel"),
};

const ROTATION_OPTIONS = [0, 90, 180, 270];

const SVG_NS = "http://www.w3.org/2000/svg";

// Only the boxes that matter for print checking: Trim (red) and Bleed (blue).
const BOX_DEFS = [
  { key: "trim", name: "TrimBox", label: "Trim Box", color: "#ff2d2d", dash: "0", resettable: true, fallback: "defaults to Crop Box" },
  { key: "bleed", name: "BleedBox", label: "Bleed Box", color: "#2d7dff", dash: "0", resettable: true, fallback: "defaults to Crop Box" },
];

const SWATCHES = {
  Cyan: "#00AEEF",
  Magenta: "#EC008C",
  Yellow: "#FFF200",
  Black: "#231F20",
};

function spotSwatch(name) {
  // Deterministic-but-arbitrary hue from the name so distinct spot plates
  // get visually distinct (not color-accurate) swatches.
  let hash = 0;
  for (const ch of name) hash = (hash * 31 + ch.charCodeAt(0)) >>> 0;
  const hue = hash % 360;
  return `hsl(${hue}, 70%, 55%)`;
}

// Works for both "/" (macOS, Linux) and "\" (Windows) separators.
function baseName(path) {
  return path.split(/[\\/]/).pop();
}

const state = {
  path: null, // working copy in a temp folder — all rendering and edits use this
  sourcePath: null, // the file the user opened (never modified)
  edited: false, // geometry changed since open → Export enabled
  page: 1,
  pageCount: 1,
  simulateOverprint: false,
  separationNames: [],
  activeSeparations: new Set(),
  renderToken: 0,
  pageBoxes: null,
  boxVisible: new Set(["trim", "bleed"]),
  textMode: true, // Select text on by default; the eyedropper still reads on hover
  applyAll: true, // page box / rotation edits apply to every page by default
  boxPreview: {}, // key → displayed norm rect while a box is being typed in (unsaved)
};

// Eyedropper request state (see the eyedropper section below).
const eyedrop = { inFlight: false, pending: null, held: false, inside: false };

const MM_PER_PT = 25.4 / 72;
// The PDF stores geometry in points; everything the user sees or types is mm.
const toMm = (pt) => pt * MM_PER_PT;
const fromMm = (mm) => mm / MM_PER_PT;
const fmtMm = (pt) => toMm(pt).toFixed(2);

function setLoading(isLoading) {
  els.loadingOverlay.classList.toggle("hidden", !isLoading);
}

async function checkGhostscript() {
  try {
    await invoke("check_ghostscript");
    els.gsWarning.classList.add("hidden");
    return true;
  } catch (err) {
    els.gsWarning.textContent = String(err);
    els.gsWarning.classList.remove("hidden");
    return false;
  }
}

// Loading progress card. Steps are real stages of the load (copying,
// reading, parsing, drawing), so the bar moves as each one finishes.
const progress = { active: false };

function setProgress(pct, label) {
  const box = document.getElementById("progress");
  if (!progress.active) {
    // Only show the card if loading takes more than a moment, so quick
    // page flips don't flash it.
    clearTimeout(progress.timer);
    progress.timer = setTimeout(() => progress.active && box.classList.remove("hidden"), 150);
  }
  progress.active = true;
  box.setAttribute("aria-valuenow", String(pct));
  document.getElementById("progressFill").style.width = `${pct}%`;
  if (label) document.getElementById("progressLabel").textContent = label;
}

function hideProgress() {
  if (!progress.active) return;
  progress.active = false;
  document.getElementById("progressFill").style.width = "100%";
  setTimeout(() => {
    if (progress.active) return;
    document.getElementById("progress").classList.add("hidden");
    document.getElementById("progressFill").style.width = "0";
  }, 250);
}

async function openPdf() {
  if (state.edited) {
    const discard = await ask("You have page changes that haven't been exported. Open another PDF and discard them?", {
      title: "Unexported changes",
      kind: "warning",
    });
    if (!discard) return;
  }
  const selected = await open({
    multiple: false,
    filters: [{ name: "PDF", extensions: ["pdf", "PDF"] }],
  });
  if (!selected) return;

  const source = Array.isArray(selected) ? selected[0] : selected;
  setProgress(5, `Opening ${baseName(source)}…`);
  try {
    const path = await invoke("create_working_copy", { path: source });
    setProgress(15, "Counting pages…");
    const pageCount = await invoke("open_pdf", { path });
    setProgress(30, "Reading page boxes…");
    state.path = path;
    state.sourcePath = source;
    setEdited(false);
    els.fileName.disabled = false;
    state.page = 1;
    state.pageCount = pageCount;
    state.separationNames = [];
    state.activeSeparations = new Set();
    els.fileName.textContent = baseName(source);
    els.fileName.title = `${source}\nClick to open a different PDF`;
    updatePageControls();
    updateRasterDpi();
    state.separationNames = [];
    loadSeparationsList(); // in the background; the page shows straight away
    await loadPageBoxes();
    view.zoom = "fit";
    await showPage({ reloadDoc: true });
    enableFixes();
    checkOverprintOnOpen(); // pop-up warning if any non-black colour overprints
  } catch (err) {
    alert(`Could not open PDF:\n${err}`);
  } finally {
    hideProgress();
  }
}

// Shows the effective resolution of the raster images on the current page.
async function updateRasterDpi() {
  if (!state.path) {
    els.rasterDpi.textContent = "–";
    return;
  }
  let info;
  try {
    info = await invoke("page_image_dpi", { path: state.path, page: state.page });
  } catch (err) {
    console.error(err);
    els.rasterDpi.textContent = "?";
    return;
  }
  const parts = [];
  if (info.ppi) {
    const [lo, hi] = info.ppi;
    parts.push(lo === hi ? `${lo}` : `${lo}–${hi}`);
  }
  if (info.vector) parts.push("Vector");
  els.rasterDpi.textContent = parts.length ? parts.join(" + ") : "–";
}

function setEdited(edited) {
  state.edited = edited;
  els.exportBtn.disabled = !edited;
}

async function exportPdf() {
  if (!state.path || !state.edited) return;
  const suggested = state.sourcePath.replace(/(\.pdf)?$/i, "_edited.pdf");
  const dest = await save({
    defaultPath: suggested,
    filters: [{ name: "PDF", extensions: ["pdf"] }],
  });
  if (!dest) return;
  try {
    await invoke("export_pdf", { from: state.path, dest });
    setEdited(false);
  } catch (err) {
    alert(`Export failed:\n${err}`);
  }
}

function updatePageControls() {
  // A held eyedropper reading belongs to the previous page/file.
  eyedrop.held = false;
  clearInks();
  els.pageLabel.textContent = state.path ? `Page ${state.page} / ${state.pageCount}` : "–";
  els.prevPage.disabled = !state.path || state.page <= 1;
  els.nextPage.disabled = !state.path || state.page >= state.pageCount;
  for (const id of ["scopeAll", "scopePage"]) document.getElementById(id).disabled = !state.path;
  markCurrentThumb();
}

// Plate list for the current page (runs Ghostscript's tiffsep once per
// page; the eyedropper reads the same cached plates).
async function loadSeparationsList() {
  if (!state.path) return;
  const forPage = state.page;
  const forPath = state.path;
  if (!state.separationNames.length) {
    els.separationsList.innerHTML = '<li class="hint">Reading separations…</li>';
  }
  let names;
  try {
    names = await invoke("list_separations", { path: forPath, page: forPage, dpi: SAMPLE_DPI });
  } catch (err) {
    els.separationsList.innerHTML = "";
    const li = document.createElement("li");
    li.className = "hint";
    li.textContent = String(err);
    els.separationsList.appendChild(li);
    return;
  }
  if (forPage !== state.page || forPath !== state.path) return; // user moved on
  state.separationNames = names;
  if (state.activeSeparations.size === 0) {
    state.activeSeparations = new Set(names);
  } else {
    // Drop selections for plates that no longer exist on this page.
    state.activeSeparations = new Set([...state.activeSeparations].filter((n) => names.includes(n)));
  }
  renderSeparationsList();
}

function renderSeparationsList() {
  els.separationsList.innerHTML = "";
  for (const name of state.separationNames) {
    const li = document.createElement("li");
    const id = `sep-${name}`;

    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.id = id;
    checkbox.checked = state.activeSeparations.has(name);
    checkbox.addEventListener("change", () => {
      if (checkbox.checked) state.activeSeparations.add(name);
      else state.activeSeparations.delete(name);
      renderCurrent();
    });

    const swatch = document.createElement("span");
    swatch.className = "swatch";
    swatch.style.background = SWATCHES[name] || spotSwatch(name);

    const label = document.createElement("label");
    label.htmlFor = id;
    label.textContent = name;
    label.style.flex = "1";

    // Eyedropper reading for this plate.
    const value = document.createElement("span");
    value.className = "ink-value";
    value.dataset.plate = name;

    li.append(checkbox, swatch, label, value);
    els.separationsList.appendChild(li);
  }

  const total = document.createElement("li");
  total.className = "ink-total";
  total.innerHTML = '<span class="ink-name">Total ink</span><span class="ink-value" data-total="1"></span>';
  els.separationsList.appendChild(total);
}

// True when every plate is ticked (the normal, full-colour view).
const allPlatesOn = () => state.separationNames.every((n) => state.activeSeparations.has(n));

// --- page view ---
//
// Normal view is drawn by pdf.js straight from the PDF: vector paths and
// text are drawn as vectors at the screen's real resolution for the
// current zoom (only the visible area, so it stays sharp at any zoom),
// and images are drawn from their own pixels. Nothing is pre-rasterised.
//
// Overprint simulation and Separations can't be shown that way — mixing
// inks is a per-pixel calculation (the browser has no CMYK), just as in
// Acrobat — so those views come from Ghostscript at screen resolution.

// Ghostscript resolution used by the eyedropper and the separations list.
const SAMPLE_DPI = 300;
const RASTER_DPI_STEPS = [72, 100, 150, 200, 300, 400, 600];
const ZOOM_STEPS = [0.1, 0.25, 0.5, 0.75, 1, 1.25, 1.5, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64];
const CSS_PX_PER_PT = 96 / 72; // 100% zoom = real size on a standard display

const view = {
  lib: null, // pdf.js module
  loadingTask: null,
  doc: null,
  page: null,
  zoom: "fit", // "fit" | factor (1 = 100%)
  scale: 1, // CSS px per PDF point, resolved from zoom
  renderTask: null,
  timer: null,
};

// Vector view unless overprint is being simulated or a plate is switched off.
const isVectorMode = () => !state.simulateOverprint && allPlatesOn();

async function loadPdfjs() {
  if (!view.lib) {
    view.lib = await import("./vendor/pdfjs/pdf.min.mjs");
    view.lib.GlobalWorkerOptions.workerSrc = new URL("vendor/pdfjs/pdf.worker.min.mjs", location.href).href;
  }
  return view.lib;
}

// (Re)loads the working copy into pdf.js — on open and after every edit.
async function loadVectorDoc() {
  const lib = await loadPdfjs();
  // pdf.js v6 frees a document through its loading task, not the document.
  if (view.loadingTask) {
    const old = view.loadingTask;
    view.loadingTask = null;
    view.doc = null;
    view.page = null;
    old.destroy().catch(() => {});
  }
  if (progress.active) setProgress(45, "Reading PDF…");
  const buf = await invoke("read_pdf", { path: state.path });
  if (progress.active) setProgress(60, "Parsing PDF…");
  const url = (p) => new URL(p, location.href).href;
  view.loadingTask = lib.getDocument({
    data: new Uint8Array(buf),
    cMapUrl: url("vendor/pdfjs/cmaps/"),
    cMapPacked: true,
    standardFontDataUrl: url("vendor/pdfjs/standard_fonts/"),
    wasmUrl: url("vendor/pdfjs/wasm/"),
    iccUrl: url("vendor/pdfjs/iccs/"),
    isEvalSupported: false,
    enableXfa: false,
  });
  view.doc = await view.loadingTask.promise;
  if (progress.active) setProgress(80, "Drawing page…");
  buildThumbs();
}

async function loadVectorPage() {
  view.page = await view.doc.getPage(state.page);
  // pdf.js shows the CropBox by default; show the whole MediaBox instead so
  // bleed is visible and the view lines up with Ghostscript and the box
  // overlay (both MediaBox based).
  const media = state.pageBoxes && state.pageBoxes.media.rect;
  if (media && view.page._pageInfo) view.page._pageInfo.view = [...media];
}

// --- page thumbnails (left strip, like Acrobat's Page Thumbnails) ---
// Rebuilt whenever the document is (re)loaded, e.g. after a rotation.
// Thumbnails render lazily as they scroll into view.

const THUMB_SIZE = 120; // CSS px, longest side

function buildThumbs() {
  const box = document.getElementById("thumbs");
  const scroll = box.scrollTop;
  if (view.thumbObserver) view.thumbObserver.disconnect();
  box.replaceChildren();
  box.classList.toggle("hidden", !view.doc);
  if (!view.doc) return;
  const doc = view.doc;
  const observer = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (!e.isIntersecting) continue;
        observer.unobserve(e.target);
        renderThumb(doc, e.target).catch((err) => console.error(err));
      }
    },
    { root: box, rootMargin: "300px" },
  );
  view.thumbObserver = observer;

  for (let n = 1; n <= doc.numPages; n++) {
    const item = document.createElement("div");
    item.className = "thumb";
    item.dataset.page = String(n);
    item.innerHTML =
      '<div class="thumb__img"></div>' +
      '<div class="thumb__bar">' +
      '<button type="button" class="thumb__rot" data-dir="-1" title="Rotate page anticlockwise">↺</button>' +
      `<span>${n}</span>` +
      '<button type="button" class="thumb__rot" data-dir="1" title="Rotate page clockwise">↻</button>' +
      "</div>";
    item.addEventListener("click", (ev) => {
      const rot = ev.target.closest(".thumb__rot");
      if (rot) rotatePageBy(n, Number(rot.dataset.dir) * 90);
      else goToPageNumber(n);
    });
    box.appendChild(item);
    observer.observe(item);
  }
  box.scrollTop = scroll;
  markCurrentThumb();
}

async function renderThumb(doc, item) {
  const page = await doc.getPage(Number(item.dataset.page));
  if (doc !== view.doc) return; // document reloaded meanwhile
  const base = page.getViewport({ scale: 1 });
  const scale = THUMB_SIZE / Math.max(base.width, base.height);
  const dpr = window.devicePixelRatio || 1;
  const viewport = page.getViewport({ scale: scale * dpr });
  const canvas = document.createElement("canvas");
  canvas.width = Math.ceil(viewport.width);
  canvas.height = Math.ceil(viewport.height);
  canvas.style.width = `${viewport.width / dpr}px`;
  canvas.style.height = `${viewport.height / dpr}px`;
  await page.render({ canvasContext: canvas.getContext("2d"), viewport, background: "white" }).promise;
  item.querySelector(".thumb__img").replaceChildren(canvas);
}

function markCurrentThumb() {
  const box = document.getElementById("thumbs");
  for (const el of box.querySelectorAll(".thumb")) {
    const current = Number(el.dataset.page) === state.page;
    el.classList.toggle("current", current);
    if (current) el.scrollIntoView({ block: "nearest" });
  }
}

// --- text layer ---
// pdf.js places invisible copies of the page's real text over the preview
// so it can be selected (Select text mode). Text that has been converted
// to outlines is just shapes, so there's nothing to select. Built once per
// page; zooming only updates the scale variable.

async function buildTextLayer() {
  const layer = document.getElementById("textLayer");
  layer.replaceChildren();
  const status = document.getElementById("textStatus");
  if (!view.page) {
    status.textContent = "";
    return;
  }
  const page = view.page;
  const content = await page.getTextContent();
  if (page !== view.page) return; // page changed meanwhile
  const runs = content.items.filter((i) => i.str && i.str.trim()).length;
  status.className = `text-status ${runs ? "live" : "none"}`;
  status.textContent = runs
    ? `This page has live text (${runs} text run${runs > 1 ? "s" : ""}) — try Select text.`
    : "No live text on this page (outlined or none).";
  setTextLayerScale();
  const viewport = page.getViewport({ scale: view.scale });
  await new view.lib.TextLayer({ textContentSource: content, container: layer, viewport }).render();
}

function setTextLayerScale() {
  const layer = document.getElementById("textLayer");
  layer.style.setProperty("--scale-factor", String(view.scale));
  layer.style.setProperty("--total-scale-factor", String(view.scale));
}

function setTextMode(on) {
  state.textMode = on;
  document.getElementById("textModeBtn").classList.toggle("active", on);
  els.imgWrap.classList.toggle("text-mode", on);
  if (!on) window.getSelection()?.removeAllRanges();
}

// Displayed page size in PDF points, after /Rotate.
function pageSizePt() {
  const vp = view.page.getViewport({ scale: 1 });
  return [vp.width, vp.height];
}

function zoomPercent() {
  return Math.round((view.scale / CSS_PX_PER_PT) * 100);
}

function applyLayout() {
  if (!view.page) return;
  const [w, h] = pageSizePt();
  if (view.zoom === "fit") {
    const pad = 48; // #viewport padding, both sides
    view.scale = Math.max(0.02, Math.min((els.viewport.clientWidth - pad) / w, (els.viewport.clientHeight - pad) / h));
  } else {
    view.scale = view.zoom * CSS_PX_PER_PT;
  }
  els.imgWrap.style.width = `${w * view.scale}px`;
  els.imgWrap.style.height = `${h * view.scale}px`;
  els.zoomLabel.textContent = `${zoomPercent()}%`;
  for (const b of [els.zoomIn, els.zoomOut, els.zoomFit, els.zoomActual]) b.disabled = false;
  document.getElementById("textModeBtn").disabled = false;
  document.getElementById("zoomToolBtn").disabled = false;
  setTextLayerScale();
}

// Draws the visible part of the page (plus a margin, so small scrolls
// don't show blank edges) at device resolution into a fresh canvas, then
// swaps it in — so there's never a blank or half-drawn frame.
async function renderVector() {
  if (!view.page || !isVectorMode()) return;
  if (view.renderTask) {
    view.renderTask.cancel();
    view.renderTask = null;
  }
  const dpr = window.devicePixelRatio || 1;
  const wrap = els.imgWrap.getBoundingClientRect();
  const port = els.viewport.getBoundingClientRect();
  const mx = port.width * 0.25;
  const my = port.height * 0.25;
  const x0 = Math.floor(Math.max(0, port.left - wrap.left - mx));
  const y0 = Math.floor(Math.max(0, port.top - wrap.top - my));
  const x1 = Math.ceil(Math.min(wrap.width, port.right - wrap.left + mx));
  const y1 = Math.ceil(Math.min(wrap.height, port.bottom - wrap.top + my));
  if (x1 <= x0 || y1 <= y0) return;

  const canvas = document.createElement("canvas");
  canvas.width = Math.ceil((x1 - x0) * dpr);
  canvas.height = Math.ceil((y1 - y0) * dpr);
  canvas.style.left = `${x0}px`;
  canvas.style.top = `${y0}px`;
  canvas.style.width = `${x1 - x0}px`;
  canvas.style.height = `${y1 - y0}px`;

  const task = view.page.render({
    canvasContext: canvas.getContext("2d"),
    viewport: view.page.getViewport({ scale: view.scale * dpr }),
    transform: [1, 0, 0, 1, -x0 * dpr, -y0 * dpr],
    background: "white",
  });
  view.renderTask = task;
  try {
    await task.promise;
  } catch (err) {
    if (err && err.name === "RenderingCancelledException") return;
    console.error(err);
    return;
  }
  if (view.renderTask === task) view.renderTask = null;
  els.vectorLayer.replaceChildren(canvas);
}

function scheduleRender(delay = 60) {
  clearTimeout(view.timer);
  view.timer = setTimeout(() => {
    if (isVectorMode()) renderVector();
    else renderRaster();
  }, delay);
}

// Ghostscript resolution for the current zoom: the screen's resolution,
// rounded up to a step so small zoom changes reuse the same render.
function rasterDpi() {
  const needed = view.scale * 72 * (window.devicePixelRatio || 1);
  return RASTER_DPI_STEPS.find((d) => d >= needed) || RASTER_DPI_STEPS[RASTER_DPI_STEPS.length - 1];
}

async function renderRaster() {
  if (!state.path || isVectorMode()) return;
  const dpi = rasterDpi();
  const key = `${state.path}|${state.page}|${dpi}|${state.simulateOverprint}|${[...state.activeSeparations].sort()}`;
  if (key === state.rasterKey) return;
  const token = ++state.renderToken;
  setLoading(true);
  try {
    let dataUri;
    if (allPlatesOn()) {
      dataUri = await invoke("render_overprint", { path: state.path, page: state.page, dpi, simulate: true });
    } else {
      dataUri = await invoke("render_separation_composite", {
        path: state.path,
        page: state.page,
        dpi,
        active: [...state.activeSeparations],
      });
    }
    if (token !== state.renderToken) return; // a newer render superseded this one
    els.preview.src = dataUri;
    state.rasterKey = key;
  } catch (err) {
    if (token === state.renderToken) alert(`Render failed:\n${err}`);
  } finally {
    if (token === state.renderToken) setLoading(false);
  }
}

async function renderCurrent() {
  if (!state.path || !view.page) return;
  els.imgWrap.classList.remove("empty");
  document.getElementById("emptyState").classList.add("hidden");
  const vector = isVectorMode();
  els.vectorLayer.classList.toggle("hidden", !vector);
  els.preview.classList.toggle("hidden", vector);
  if (vector) {
    state.renderToken++; // abandon any Ghostscript render still in flight
    setLoading(false);
    await renderVector();
  } else {
    await renderRaster();
  }
}

// Page content changed (new file, page, or an edit): reload and redraw.
async function showPage({ reloadDoc = false } = {}) {
  state.rasterKey = null;
  els.vectorLayer.replaceChildren();
  if (reloadDoc || !view.doc) await loadVectorDoc();
  await loadVectorPage();
  applyLayout();
  buildTextLayer().catch((err) => console.error(err));
  await renderCurrent();
}

function setZoom(zoom, anchor) {
  if (!view.page) return;
  // Keep the point under `anchor` (client coords; default: viewport
  // centre) in place while zooming.
  const port = els.viewport.getBoundingClientRect();
  const ax = anchor ? anchor[0] : port.left + port.width / 2;
  const ay = anchor ? anchor[1] : port.top + port.height / 2;
  const before = els.imgWrap.getBoundingClientRect();
  const fx = (ax - before.left) / before.width;
  const fy = (ay - before.top) / before.height;

  view.zoom = zoom;
  applyLayout();

  const after = els.imgWrap.getBoundingClientRect();
  els.viewport.scrollLeft += after.left + fx * after.width - ax;
  els.viewport.scrollTop += after.top + fy * after.height - ay;
  scheduleRender(isVectorMode() ? 60 : 250);
}

function stepZoom(dir, anchor) {
  const cur = view.scale / CSS_PX_PER_PT;
  const next =
    dir > 0
      ? ZOOM_STEPS.find((z) => z > cur * 1.01) || ZOOM_STEPS[ZOOM_STEPS.length - 1]
      : [...ZOOM_STEPS].reverse().find((z) => z < cur * 0.99) || ZOOM_STEPS[0];
  setZoom(next, anchor);
}

async function goToPageNumber(n) {
  if (n !== state.page) await goToPage(n - state.page);
}

async function goToPage(delta) {
  const next = state.page + delta;
  if (next < 1 || next > state.pageCount) return;
  state.page = next;
  updatePageControls();
  updateRasterDpi();
  loadSeparationsList();
  setProgress(40, `Loading page ${next}…`);
  try {
    await loadPageBoxes();
    setProgress(70, "Drawing page…");
    await showPage();
  } finally {
    hideProgress();
  }
}

// --- page boxes (MediaBox / CropBox / TrimBox / ArtBox / BleedBox) ---

async function loadPageBoxes() {
  if (!state.path) {
    state.pageBoxes = null;
    renderPageBoxesPanel();
    drawBoxOverlay();
    return;
  }
  try {
    state.pageBoxes = await invoke("get_page_boxes", { path: state.path, page: state.page });
  } catch (err) {
    state.pageBoxes = null;
    console.error(err);
  }
  renderPageBoxesPanel();
  renderRotationPanel();
  drawBoxOverlay();
}

function renderRotationPanel() {
  const container = els.pageRotation;
  container.innerHTML = "";
  const current = state.pageBoxes ? state.pageBoxes.rotate : null;
  els.rotationCurrent.textContent = current === null ? "" : `${current}°`;

  for (const deg of ROTATION_OPTIONS) {
    const btn = document.createElement("button");
    btn.textContent = `${deg}°`;
    btn.disabled = !state.path;
    if (deg === current) btn.classList.add("active");
    btn.addEventListener("click", () => setRotation(deg));
    container.appendChild(btn);
  }
}

// Rotates any page (from its thumbnail) by +/-90 degrees.
async function rotatePageBy(n, delta) {
  if (!state.path || !view.doc) return;
  try {
    const page = await view.doc.getPage(n);
    const degrees = (((page.rotate + delta) % 360) + 360) % 360;
    const boxes = await invoke("set_page_rotation", { path: state.path, page: n, degrees });
    if (n === state.page) state.pageBoxes = boxes;
    await afterBoxEdit();
  } catch (err) {
    alert(`Could not rotate page ${n}:\n${err}`);
  }
}

// Every edit (colour & font conversions, rotation, page boxes) goes to all
// pages unless "Apply edits to" is switched to This page. Thumbnail arrows
// always rotate just their own page.
const editAllPages = () => state.applyAll && state.pageCount > 1;

function setApplyScope(all) {
  state.applyAll = all;
  document.getElementById("scopeAll").classList.toggle("active", all);
  document.getElementById("scopePage").classList.toggle("active", !all);
}

async function setRotation(degrees) {
  if (!state.path) return;
  try {
    state.pageBoxes = editAllPages()
      ? await invoke("set_rotation_all", { path: state.path, page: state.page, degrees })
      : await invoke("set_page_rotation", { path: state.path, page: state.page, degrees });
    await afterBoxEdit();
  } catch (err) {
    alert(`Could not set rotation:\n${err}`);
  }
}

function setBoxError(key, message) {
  const el = document.getElementById(`box-err-${key}`);
  if (el) el.textContent = message || "";
}

// --- box geometry as seen on screen ---
// Boxes are shown as distances in from each edge of the page (MediaBox)
// as it's displayed, i.e. after /Rotate — so "Top" is always the edge at
// the top of the preview. The PDF stores unrotated corner coordinates.

const INSET_SIDES = ["top", "bottom", "left", "right"];
const INSET_LABELS = { top: "Top", bottom: "Bottom", left: "Left", right: "Right" };

// Displayed page size in points.
function displayedMediaPt() {
  const [x0, y0, x1, y1] = state.pageBoxes.media.rect;
  const r = state.pageBoxes.rotate;
  return r === 90 || r === 270 ? [y1 - y0, x1 - x0] : [x1 - x0, y1 - y0];
}

// Box → { top, bottom, left, right } insets in points from the page edges.
function boxInsets(info) {
  const [W, H] = displayedMediaPt();
  const [l, t, w, h] = info.norm;
  return { top: t * H, bottom: (1 - t - h) * H, left: l * W, right: (1 - l - w) * W };
}

// Inverse of boxInsets: insets (points) → unrotated PDF rect [x0,y0,x1,y1].
function rectFromInsets(ins) {
  const [W, H] = displayedMediaPt();
  const [mx0, my0, mx1, my1] = state.pageBoxes.media.rect;
  const rot = state.pageBoxes.rotate;
  // Displayed unit coords (y down) → unrotated unit coords (y down).
  const unrotate = (x, y) => {
    if (rot === 90) return [y, 1 - x];
    if (rot === 180) return [1 - x, 1 - y];
    if (rot === 270) return [1 - y, x];
    return [x, y];
  };
  const a = unrotate(ins.left / W, ins.top / H);
  const b = unrotate(1 - ins.right / W, 1 - ins.bottom / H);
  const toPdf = ([x, y]) => [mx0 + x * (mx1 - mx0), my0 + (1 - y) * (my1 - my0)];
  const [ax, ay] = toPdf(a);
  const [bx, by] = toPdf(b);
  return [Math.min(ax, bx), Math.min(ay, by), Math.max(ax, bx), Math.max(ay, by)];
}

async function applyBox(def, inputs, allPages = false) {
  setBoxError(def.key, "");
  const ins = {};
  for (const side of INSET_SIDES) ins[side] = fromMm(Number.parseFloat(inputs[side].value));
  if (Object.values(ins).some((v) => !Number.isFinite(v))) {
    setBoxError(def.key, "Enter a number for all four sides.");
    return;
  }
  const [W, H] = displayedMediaPt();
  if (ins.left + ins.right >= W || ins.top + ins.bottom >= H) {
    setBoxError(def.key, "Those distances leave no room — opposite sides overlap.");
    return;
  }
  const rect = rectFromInsets(ins);
  try {
    state.pageBoxes = allPages
      ? await invoke("set_page_box_all", {
          path: state.path,
          page: state.page,
          name: def.name,
          insets: [ins.top, ins.bottom, ins.left, ins.right],
        })
      : await invoke("set_page_box", { path: state.path, page: state.page, name: def.name, rect });
    await afterBoxEdit();
  } catch (err) {
    setBoxError(def.key, String(err));
  }
}

async function resetBox(def) {
  setBoxError(def.key, "");
  try {
    state.pageBoxes = editAllPages()
      ? await invoke("reset_page_box_all", { path: state.path, page: state.page, name: def.name })
      : await invoke("reset_page_box", { path: state.path, page: state.page, name: def.name });
    await afterBoxEdit();
  } catch (err) {
    setBoxError(def.key, String(err));
  }
}

async function afterBoxEdit() {
  setEdited(true);
  renderPageBoxesPanel();
  renderRotationPanel();
  drawBoxOverlay();
  // Editing a box (especially MediaBox) or the rotation can change the
  // page's rendered dimensions/orientation, so refresh whatever's
  // currently on screen.
  loadSeparationsList();
  setProgress(40, "Applying change…");
  try {
    await showPage({ reloadDoc: true });
  } finally {
    hideProgress();
  }
}

function renderPageBoxesPanel() {
  state.boxPreview = {};
  const container = els.pageBoxes;
  container.innerHTML = "";
  if (!state.pageBoxes) {
    const p = document.createElement("p");
    p.className = "hint";
    p.textContent = "Open a PDF to see its page boxes.";
    container.appendChild(p);
    return;
  }

  for (const def of BOX_DEFS) {
    const info = state.pageBoxes[def.key];
    if (!info) continue;

    const row = document.createElement("div");
    row.className = "box-row";

    const header = document.createElement("div");
    header.className = "box-row-header";

    const visCheckbox = document.createElement("input");
    visCheckbox.type = "checkbox";
    visCheckbox.title = "Show on preview";
    visCheckbox.checked = state.boxVisible.has(def.key);
    visCheckbox.addEventListener("change", () => {
      if (visCheckbox.checked) state.boxVisible.add(def.key);
      else state.boxVisible.delete(def.key);
      drawBoxOverlay();
    });

    const sample = document.createElement("span");
    sample.className = "line-sample";
    sample.style.borderTopColor = def.color;
    sample.style.borderTopStyle = def.dash === "0" ? "solid" : "dashed";

    const title = document.createElement("span");
    title.className = "box-title";
    title.textContent = def.label;

    const status = document.createElement("span");
    status.className = "box-status";
    status.textContent = info.explicit ? "explicit" : def.fallback;

    header.append(visCheckbox, sample, title, status);

    const [W, H] = displayedMediaPt();
    const ins = boxInsets(info);
    const size = document.createElement("div");
    size.className = "box-size";
    // First text node = finished size (updated live while typing).
    size.append(`${fmtMm(W - ins.left - ins.right)} × ${fmtMm(H - ins.top - ins.bottom)} mm`);

    // How far the bleed extends past the trim, per side.
    if (def.key === "bleed" && state.pageBoxes.trim) {
      const t = boxInsets(state.pageBoxes.trim);
      const amounts = INSET_SIDES.map((s) => toMm(t[s] - ins[s]));
      const same = amounts.every((a) => Math.abs(a - amounts[0]) < 0.005);
      size.append(same
        ? `  ·  ${amounts[0].toFixed(2)} mm bleed each side`
        : `  ·  bleed T ${amounts[0].toFixed(2)} / B ${amounts[1].toFixed(2)} / L ${amounts[2].toFixed(2)} / R ${amounts[3].toFixed(2)} mm`);
    }

    const fields = document.createElement("div");
    fields.className = "box-fields";
    const inputs = {};
    for (const side of INSET_SIDES) {
      const label = document.createElement("label");
      label.textContent = `${INSET_LABELS[side]} (mm)`;
      label.title = `Distance in from the ${side} edge of the page`;
      const input = document.createElement("input");
      input.type = "number";
      input.step = "0.01";
      input.value = fmtMm(Math.max(0, ins[side]));
      label.appendChild(input);
      fields.appendChild(label);
      inputs[side] = input;
    }

    // Move the box's line on the preview live while typing (not saved
    // until Apply).
    const preview = () => {
      const v = {};
      for (const side of INSET_SIDES) v[side] = fromMm(Number.parseFloat(inputs[side].value));
      if (Object.values(v).some((x) => !Number.isFinite(x)) || v.left + v.right >= W || v.top + v.bottom >= H) return;
      state.boxPreview[def.key] = [v.left / W, v.top / H, (W - v.left - v.right) / W, (H - v.top - v.bottom) / H];
      size.firstChild.textContent = `${fmtMm(W - v.left - v.right)} × ${fmtMm(H - v.top - v.bottom)} mm`;
      drawBoxOverlay();
    };
    for (const input of Object.values(inputs)) {
      input.addEventListener("input", preview);
      input.addEventListener("keydown", (ev) => {
        if (ev.key === "Enter") applyBox(def, inputs, editAllPages());
      });
    }

    const actions = document.createElement("div");
    actions.className = "box-row-actions";
    const applyBtn = document.createElement("button");
    applyBtn.textContent = "Apply";
    applyBtn.title = "Uses the Apply edits to setting (all pages or this page)";
    applyBtn.addEventListener("click", () => applyBox(def, inputs, editAllPages()));
    actions.appendChild(applyBtn);

    if (def.resettable) {
      const resetBtn = document.createElement("button");
      resetBtn.textContent = "Reset to default";
      resetBtn.addEventListener("click", () => resetBox(def));
      actions.appendChild(resetBtn);
    }

    const errorDiv = document.createElement("div");
    errorDiv.className = "box-error";
    errorDiv.id = `box-err-${def.key}`;

    row.append(header, size, fields, actions, errorDiv);
    container.appendChild(row);
  }
}

function drawBoxOverlay() {
  const svg = els.boxOverlay;
  svg.innerHTML = "";
  if (!state.pageBoxes) return;
  svg.setAttribute("viewBox", "0 0 1 1");

  for (const def of BOX_DEFS) {
    if (!state.boxVisible.has(def.key)) continue;
    const info = state.pageBoxes[def.key];
    if (!info) continue;
    // While a box is being typed in, show the unsaved position dashed.
    const pending = state.boxPreview[def.key];
    const [x, y, w, h] = pending || info.norm;

    const rect = document.createElementNS(SVG_NS, "rect");
    rect.setAttribute("x", x);
    rect.setAttribute("y", y);
    rect.setAttribute("width", w);
    rect.setAttribute("height", h);
    rect.setAttribute("stroke", def.color);
    rect.setAttribute("stroke-width", "2");
    if (pending) rect.setAttribute("stroke-dasharray", "6,4");
    else if (def.dash !== "0") rect.setAttribute("stroke-dasharray", def.dash);

    const titleEl = document.createElementNS(SVG_NS, "title");
    const [bx0, by0, bx1, by1] = info.rect;
    titleEl.textContent = `${def.label}: ${fmtMm(bx1 - bx0)} × ${fmtMm(by1 - by0)} mm`;
    rect.appendChild(titleEl);

    svg.appendChild(rect);
  }
}

// --- wiring ---

els.winMin.addEventListener("click", () => appWindow.minimize());
els.winMax.addEventListener("click", () => appWindow.toggleMaximize());
els.winClose.addEventListener("click", () => appWindow.close());


// --- eyedropper ---
// Hovering the page fills in each plate's ink % (and the total) in the
// Separations list, read from Ghostscript's tiffsep plates so overprint
// is taken into account. Only one request is in flight at a time; the
// latest pointer position is sent once it returns.

// Gold padlock that follows the cursor while a reading is held (locked).
const lockBadge = document.getElementById("lockBadge");

function moveLockBadge(ev) {
  lockBadge.style.left = `${ev.clientX + 14}px`;
  lockBadge.style.top = `${ev.clientY + 12}px`;
}

function showLockBadge(show, ev) {
  if (show && ev) moveLockBadge(ev);
  lockBadge.classList.toggle("hidden", !show);
}

function clearInks() {
  showLockBadge(false);
  els.separationsList.classList.remove("held");
  for (const el of els.separationsList.querySelectorAll(".ink-value")) el.textContent = "";
}

function showInks(samples) {
  if (!eyedrop.held && !eyedrop.inside) return; // pointer has left the page
  els.separationsList.classList.toggle("held", eyedrop.held);
  let total = 0;
  for (const { name, percent } of samples) {
    total += percent;
    const el = [...els.separationsList.querySelectorAll(".ink-value[data-plate]")].find((e) => e.dataset.plate === name);
    if (el) el.textContent = `${Math.round(percent)}%`;
  }
  const t = els.separationsList.querySelector(".ink-value[data-total]");
  if (t) t.textContent = `${Math.round(total)}%`;
}

async function sampleAt(x, y) {
  if (eyedrop.inFlight) {
    eyedrop.pending = [x, y];
    return;
  }
  eyedrop.inFlight = true;
  try {
    const samples = await invoke("sample_inks", { path: state.path, page: state.page, dpi: SAMPLE_DPI, x, y });
    if (eyedrop.pending === null) showInks(samples); // skip if a newer point is queued
  } catch (err) {
    console.error(err);
  } finally {
    eyedrop.inFlight = false;
    if (eyedrop.pending) {
      const [px, py] = eyedrop.pending;
      eyedrop.pending = null;
      sampleAt(px, py);
    }
  }
}

function pointerFraction(ev) {
  const r = els.imgWrap.getBoundingClientRect();
  return [(ev.clientX - r.left) / r.width, (ev.clientY - r.top) / r.height];
}

els.imgWrap.addEventListener("mousemove", (ev) => {
  eyedrop.inside = true;
  if (eyedrop.held) showLockBadge(true, ev);
  if (!state.path || eyedrop.held || !state.separationNames.length) return;
  sampleAt(...pointerFraction(ev));
});

els.imgWrap.addEventListener("mouseleave", () => {
  eyedrop.inside = false;
  eyedrop.pending = null;
  showLockBadge(false); // reappears when the cursor comes back over the page
  if (!eyedrop.held) clearInks();
});

els.imgWrap.addEventListener("click", (ev) => {
  // In Select text mode a click on text (or finishing a selection) is for
  // the text, not for holding an ink reading.
  if (state.zoomTool) return; // clicks belong to the zoom tool
  if (state.textMode && (ev.target.closest("#textLayer span") || String(window.getSelection() || "").trim())) return;
  if (!state.path || !state.separationNames.length) return;
  eyedrop.held = !eyedrop.held;
  els.separationsList.classList.toggle("held", eyedrop.held);
  showLockBadge(eyedrop.held, ev);
  sampleAt(...pointerFraction(ev));
});

els.emptyOpenBtn.addEventListener("click", openPdf);
els.fileName.addEventListener("click", openPdf);
els.exportBtn.addEventListener("click", exportPdf);
els.prevPage.addEventListener("click", () => goToPage(-1));
els.nextPage.addEventListener("click", () => goToPage(1));

els.overprintToggle.addEventListener("change", () => {
  state.simulateOverprint = els.overprintToggle.checked;
  renderCurrent();
});

// --- overprint warning on open ---
// Black overprinting is normal; any other colour overprinting (white,
// CMYK colours, RGB, spots) is usually a mistake, so warn straight away.

const overprintDialog = document.getElementById("overprintDialog");

async function checkOverprintOnOpen() {
  const forPath = state.path;
  let hits;
  try {
    hits = await invoke("check_overprint", { path: forPath });
  } catch (err) {
    console.error(err);
    return;
  }
  if (!hits.length || forPath !== state.path) return;
  const list = document.getElementById("overprintList");
  list.innerHTML = "";
  for (const h of hits) {
    const li = document.createElement("li");
    const page = document.createElement("span");
    page.className = "op-warning__page";
    page.textContent = `Page ${h.page}`;
    const colour = document.createElement("span");
    colour.className = "op-warning__colour";
    colour.textContent = h.colour;
    const kind = document.createElement("span");
    kind.className = "op-warning__kind";
    kind.textContent = h.kind;
    li.append(page, colour, kind);
    list.appendChild(li);
  }
  overprintDialog.dataset.firstPage = String(hits[0].page);
  overprintDialog.classList.remove("hidden");
}

document.getElementById("overprintOk").addEventListener("click", () => overprintDialog.classList.add("hidden"));
document.getElementById("overprintShow").addEventListener("click", async () => {
  overprintDialog.classList.add("hidden");
  const first = Number(overprintDialog.dataset.firstPage || state.page);
  if (first !== state.page) await goToPageNumber(first);
  els.overprintToggle.checked = true;
  state.simulateOverprint = true;
  renderCurrent();
});
window.addEventListener("keydown", (ev) => {
  if (ev.key === "Escape" && !overprintDialog.classList.contains("hidden")) overprintDialog.classList.add("hidden");
});

els.selectAllSep.addEventListener("click", () => {
  state.activeSeparations = new Set(state.separationNames);
  renderSeparationsList();
  renderCurrent();
});

els.selectNoneSep.addEventListener("click", () => {
  state.activeSeparations = new Set();
  renderSeparationsList();
  renderCurrent();
});


// --- colour & fonts fixes ---

const fixBtns = ["checkRgbBtn", "convertRgbBtn", "convertSpotsBtn", "outlineBtn"].map((id) => document.getElementById(id));
const rgbResult = document.getElementById("rgbResult");

function enableFixes() {
  for (const b of fixBtns) b.disabled = !state.path;
  document.getElementById("convertRgbBtn").disabled = true; // until a check finds RGB
  rgbResult.innerHTML = "";
  updateSpotButton();
}

// Greys out "Convert spot colours" when the PDF defines no spot colours.
async function updateSpotButton() {
  const btn = document.getElementById("convertSpotsBtn");
  if (!state.path) return;
  btn.disabled = true;
  btn.title = "Checking for spot colours…";
  try {
    const spots = await invoke("spot_colours", { path: state.path });
    btn.disabled = spots.length === 0;
    btn.title = spots.length ? `Spot colours in this PDF: ${spots.join(", ")}` : "No spot colours in this PDF";
    btn.textContent = spots.length
      ? `Convert ${spots.length} spot colour${spots.length > 1 ? "s" : ""} to CMYK`
      : "No spot colours to convert";
  } catch (err) {
    console.error(err);
    btn.disabled = false;
    btn.title = "";
    btn.textContent = "Convert spot colours to CMYK";
  }
}

function showRgbResult(pages) {
  rgbResult.innerHTML = "";
  const li = (cls, text) => {
    const el = document.createElement("li");
    el.className = cls;
    el.textContent = text;
    rgbResult.appendChild(el);
  };
  if (!pages.length) {
    li("ok", "✓ No RGB found");
    document.getElementById("convertRgbBtn").disabled = true;
    return;
  }
  for (const p of pages) {
    const parts = [];
    if (p.text) parts.push(`${p.text} text`);
    if (p.vector) parts.push(`${p.vector} vector`);
    if (p.images) parts.push(`${p.images} image${p.images > 1 ? "s" : ""}`);
    if (p.shadings) parts.push(`${p.shadings} gradient${p.shadings > 1 ? "s" : ""}`);
    li("warn", `Page ${p.page}: RGB ${parts.join(", ")}`);
  }
  document.getElementById("convertRgbBtn").disabled = false;
}

async function checkRgb() {
  if (!state.path) return;
  rgbResult.innerHTML = '<li class="hint">Checking…</li>';
  try {
    showRgbResult(await invoke("check_rgb", { path: state.path }));
  } catch (err) {
    rgbResult.innerHTML = "";
    alert(`RGB check failed:\n${err}`);
  }
}

const CONVERSIONS = {
  rgb: { label: "Converting RGB to CMYK…", done: "RGB converted to CMYK" },
  spots: { label: "Converting spot colours to CMYK…", done: "Spot colours converted to CMYK" },
  outlines: { label: "Converting text to outlines…", done: "Text converted to outlines" },
};

async function runConversion(kind) {
  if (!state.path) return;
  for (const b of fixBtns) b.disabled = true;
  setProgress(30, CONVERSIONS[kind].label);
  try {
    await invoke("convert_pdf", { path: state.path, kind, page: editAllPages() ? null : state.page });
    setProgress(60, "Reloading…");
    state.separationNames = [];
    state.activeSeparations = new Set();
    await loadPageBoxes();
    updateRasterDpi();
    loadSeparationsList();
    setEdited(true);
    await showPage({ reloadDoc: true });
    enableFixes();
    // Re-check so the result reflects the converted file.
    await checkRgb();
    const note = document.createElement("li");
    note.className = "ok";
    note.textContent = `✓ ${CONVERSIONS[kind].done}`;
    rgbResult.prepend(note);
  } catch (err) {
    enableFixes();
    alert(`Conversion failed:\n${err}`);
  } finally {
    hideProgress();
  }
}

document.getElementById("checkRgbBtn").addEventListener("click", checkRgb);
document.getElementById("convertRgbBtn").addEventListener("click", () => runConversion("rgb"));
document.getElementById("convertSpotsBtn").addEventListener("click", () => runConversion("spots"));
document.getElementById("outlineBtn").addEventListener("click", () => runConversion("outlines"));

// --- tutorial link & footer (same as RapidCulling) ---

// TODO before launch: set the Mangoprint Prepress PDF tutorial video URL.
const TUTORIAL_VIDEO_URL = "";
const TUTORIAL_DISMISSED_KEY = "mangoprint.tutorialDismissed";
const COFFEE_URL = "https://buymeacoffee.com/chriscorkphotography";
const SITE_URL = "https://mangoprint.co.uk";

function openExternal(url) {
  if (url) window.__TAURI__.opener.openUrl(url).catch((err) => console.error(err));
}

{
  const link = document.getElementById("videoLink");
  let dismissed = false;
  try {
    dismissed = localStorage.getItem(TUTORIAL_DISMISSED_KEY) === "1";
  } catch {
    // localStorage unavailable - show the link.
  }
  link.classList.toggle("hidden", dismissed);
  document.getElementById("videoLinkCta").addEventListener("click", (ev) => {
    ev.preventDefault();
    openExternal(TUTORIAL_VIDEO_URL);
  });
  document.getElementById("videoLinkClose").addEventListener("click", () => {
    link.classList.add("hidden");
    try {
      localStorage.setItem(TUTORIAL_DISMISSED_KEY, "1");
    } catch {
      // Dismissal just won't persist across restarts.
    }
  });
  document.getElementById("coffeeLink").addEventListener("click", (ev) => {
    ev.preventDefault();
    openExternal(COFFEE_URL);
  });
  document.getElementById("siteLink").addEventListener("click", (ev) => {
    ev.preventDefault();
    openExternal(SITE_URL);
  });

  // About dialog: opened from the footer; closes on ×, Esc or a click outside.
  const about = document.getElementById("aboutDialog");
  const showAbout = (show) => about.classList.toggle("hidden", !show);
  document.getElementById("aboutLink").addEventListener("click", (ev) => {
    ev.preventDefault();
    showAbout(true);
  });
  document.getElementById("aboutClose").addEventListener("click", () => showAbout(false));
  about.addEventListener("click", (ev) => {
    if (ev.target === about) showAbout(false);
    const link = ev.target.closest("a[data-url]");
    if (link) {
      ev.preventDefault();
      openExternal(link.dataset.url);
    }
  });
  window.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape" && !about.classList.contains("hidden")) showAbout(false);
  });
}

document.getElementById("textModeBtn").addEventListener("click", () => {
  if (state.zoomTool) {
    setZoomTool(false, true);
    return;
  }
  setTextMode(!state.textMode);
});
els.imgWrap.classList.toggle("text-mode", state.textMode);
window.addEventListener("keydown", (ev) => {
  if (ev.key !== "Escape") return;
  if (state.zoomTool) setZoomTool(false);
  else if (state.textMode) setTextMode(false);
});

document.getElementById("scopeAll").addEventListener("click", () => setApplyScope(true));
document.getElementById("scopePage").addEventListener("click", () => setApplyScope(false));

// --- page navigation: Page Up/Down, Home/End, and the mouse wheel ---
// The wheel scrolls within a page as normal; once you hit the bottom (or
// top) of the page, the next wheel turn moves to the next (or previous)
// page, like Acrobat's single-page view.

let wheelPageLock = 0;
els.viewport.addEventListener(
  "wheel",
  (ev) => {
    if (ev.ctrlKey || !view.page || state.pageCount < 2 || Math.abs(ev.deltaY) < Math.abs(ev.deltaX)) return;
    const v = els.viewport;
    const atTop = v.scrollTop <= 0;
    const atBottom = v.scrollTop + v.clientHeight >= v.scrollHeight - 1;
    const down = ev.deltaY > 0;
    if ((down && !atBottom) || (!down && !atTop)) return;
    if ((down && state.page >= state.pageCount) || (!down && state.page <= 1)) return;
    ev.preventDefault();
    const now = Date.now();
    if (now < wheelPageLock) return;
    wheelPageLock = now + 400; // one page per flick, not dozens
    goToPage(down ? 1 : -1).then(() => {
      v.scrollTop = down ? 0 : v.scrollHeight;
    });
  },
  { passive: false },
);

window.addEventListener("keydown", (ev) => {
  if (!view.page || ev.ctrlKey || ev.altKey) return;
  if (ev.target.closest && ev.target.closest("input, textarea, select")) return;
  if (ev.key === "PageDown") goToPage(1);
  else if (ev.key === "PageUp") goToPage(-1);
  else if (ev.key === "Home") goToPageNumber(1);
  else if (ev.key === "End") goToPageNumber(state.pageCount);
  else return;
  ev.preventDefault();
});

// --- zoom area (marquee) tool ---
// While on: drag a box over the page to zoom so that box fills the view;
// a plain click zooms in one step there, Alt+click zooms out. Esc or the
// button turns it off and goes back to Select text.

const marquee = document.getElementById("marquee");
const zoomDrag = { active: false, x0: 0, y0: 0 };

function setZoomTool(on, textAfter) {
  state.zoomTool = on;
  document.getElementById("zoomToolBtn").classList.toggle("active", on);
  els.viewport.classList.toggle("zoom-tool", on);
  els.imgWrap.classList.toggle("zoom-tool", on);
  if (on) {
    state.textModeBeforeZoom = state.textMode;
    setTextMode(false);
    eyedrop.held = false;
    clearInks();
  } else {
    setTextMode(textAfter ?? state.textModeBeforeZoom ?? true);
  }
}

function drawMarquee(x1, y1) {
  marquee.style.left = `${Math.min(zoomDrag.x0, x1)}px`;
  marquee.style.top = `${Math.min(zoomDrag.y0, y1)}px`;
  marquee.style.width = `${Math.abs(x1 - zoomDrag.x0)}px`;
  marquee.style.height = `${Math.abs(y1 - zoomDrag.y0)}px`;
}

// Zooms so the client-space box (x0,y0)-(x1,y1) fills the view, centred.
function zoomToBox(x0, y0, x1, y1) {
  const wrap = els.imgWrap.getBoundingClientRect();
  const port = els.viewport.getBoundingClientRect();
  // Box as fractions of the page, clipped to the page.
  const fx0 = Math.max(0, (Math.min(x0, x1) - wrap.left) / wrap.width);
  const fx1 = Math.min(1, (Math.max(x0, x1) - wrap.left) / wrap.width);
  const fy0 = Math.max(0, (Math.min(y0, y1) - wrap.top) / wrap.height);
  const fy1 = Math.min(1, (Math.max(y0, y1) - wrap.top) / wrap.height);
  if (fx1 <= fx0 || fy1 <= fy0) return;
  const boxW = (fx1 - fx0) * wrap.width;
  const boxH = (fy1 - fy0) * wrap.height;
  const pad = 48;
  const k = Math.min((port.width - pad) / boxW, (port.height - pad) / boxH);
  view.zoom = Math.min(ZOOM_STEPS[ZOOM_STEPS.length - 1], Math.max(ZOOM_STEPS[0], (view.scale * k) / CSS_PX_PER_PT));
  applyLayout();
  const after = els.imgWrap.getBoundingClientRect();
  els.viewport.scrollLeft += after.left + ((fx0 + fx1) / 2) * after.width - (port.left + port.width / 2);
  els.viewport.scrollTop += after.top + ((fy0 + fy1) / 2) * after.height - (port.top + port.height / 2);
  scheduleRender(isVectorMode() ? 60 : 250);
}

els.viewport.addEventListener("mousedown", (ev) => {
  if (!state.zoomTool || !view.page || ev.button !== 0) return;
  ev.preventDefault();
  zoomDrag.active = true;
  zoomDrag.x0 = ev.clientX;
  zoomDrag.y0 = ev.clientY;
  drawMarquee(ev.clientX, ev.clientY);
});

window.addEventListener("mousemove", (ev) => {
  if (state.zoomTool) els.viewport.classList.toggle("zoom-out", ev.altKey);
  if (!zoomDrag.active) return;
  drawMarquee(ev.clientX, ev.clientY);
  marquee.classList.toggle("hidden", Math.abs(ev.clientX - zoomDrag.x0) + Math.abs(ev.clientY - zoomDrag.y0) < 6);
});

window.addEventListener("mouseup", (ev) => {
  if (!zoomDrag.active) return;
  zoomDrag.active = false;
  marquee.classList.add("hidden");
  const moved = Math.abs(ev.clientX - zoomDrag.x0) > 6 || Math.abs(ev.clientY - zoomDrag.y0) > 6;
  if (moved) zoomToBox(zoomDrag.x0, zoomDrag.y0, ev.clientX, ev.clientY);
  else stepZoom(ev.altKey ? -1 : 1, [ev.clientX, ev.clientY]);
});

document.getElementById("zoomToolBtn").addEventListener("click", () => setZoomTool(!state.zoomTool));

// Z toggles the zoom area tool (ignored while typing in a field).
window.addEventListener("keydown", (ev) => {
  if (ev.key.toLowerCase() !== "z" || ev.ctrlKey || ev.altKey || ev.metaKey || !view.page) return;
  if (ev.target.closest && ev.target.closest("input, textarea, select")) return;
  ev.preventDefault();
  setZoomTool(!state.zoomTool);
});

// --- zoom & scrolling ---

els.zoomIn.addEventListener("click", () => stepZoom(1));
els.zoomOut.addEventListener("click", () => stepZoom(-1));
els.zoomFit.addEventListener("click", () => setZoom("fit"));
els.zoomActual.addEventListener("click", () => setZoom(1));
els.zoomLabel.addEventListener("dblclick", () => setZoom("fit"));

// Ctrl + mouse wheel / trackpad pinch zooms around the cursor.
els.viewport.addEventListener(
  "wheel",
  (ev) => {
    if (!ev.ctrlKey || !view.page) return;
    ev.preventDefault();
    stepZoom(ev.deltaY < 0 ? 1 : -1, [ev.clientX, ev.clientY]);
  },
  { passive: false },
);

window.addEventListener("keydown", (ev) => {
  if (!ev.ctrlKey || !view.page) return;
  if (ev.key === "=" || ev.key === "+") stepZoom(1);
  else if (ev.key === "-") stepZoom(-1);
  else if (ev.key === "0") setZoom("fit");
  else if (ev.key === "1") setZoom(1);
  else return;
  ev.preventDefault();
});

// The vector canvas only covers the visible area, so redraw after scrolling.
els.viewport.addEventListener("scroll", () => {
  if (isVectorMode()) scheduleRender(80);
});

new ResizeObserver(() => {
  if (!view.page) return;
  if (view.zoom === "fit") applyLayout();
  scheduleRender(isVectorMode() ? 80 : 250);
}).observe(els.viewport);

checkGhostscript();
renderPageBoxesPanel();
renderRotationPanel();
