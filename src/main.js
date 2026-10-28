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
  openBtn: document.getElementById("openBtn"),
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

// Swatch colour for a plate: process inks fixed, spot inks from the PDF's
// own definition of the spot (falls back to a made-up colour if the PDF's
// definition can't be read).
function plateColour(name) {
  return SWATCHES[name] || (state.spotSwatches && state.spotSwatches[name]) || spotSwatch(name);
}

async function loadSpotSwatches() {
  const forPath = state.path;
  state.spotSwatches = {};
  try {
    const found = await invoke("spot_swatches", { path: forPath });
    if (forPath !== state.path) return;
    state.spotSwatches = found;
    // The simulated view draws spot inks in these colours.
    if (Object.keys(found).length && view.page && !isVectorMode()) renderRaster();
    // Recolour any swatches already on screen.
    for (const li of els.separationsList.querySelectorAll("li")) {
      const name = li.querySelector(".ink-value[data-plate]")?.dataset.plate;
      const sw = li.querySelector(".swatch");
      if (name && sw) sw.style.background = plateColour(name);
    }
  } catch (err) {
    console.error(err);
  }
}

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
  edited: false, // changed since opened or last saved
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
const eyedrop = { inFlight: false, pending: null, inside: false };

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

const PDF_FILTER = [{ name: "PDF", extensions: ["pdf", "PDF"] }];

// Each chosen PDF opens in its own tab.
async function openPdf() {
  const selected = await open({ multiple: true, filters: PDF_FILTER });
  if (!selected) return;
  for (const p of Array.isArray(selected) ? selected : [selected]) await loadPdf(p);
}

// Opens a PDF by path in a new tab (or switches to it if it's already
// open): from the Open dialog, a double-clicked file, or a file dropped on
// the window.
async function loadPdf(source) {
  const already = tabs.find((t) => t.sourcePath === source);
  if (already) return switchTab(already);
  saveTabState();
  setProgress(5, `Opening ${baseName(source)}…`);
  let tab = null;
  try {
    const path = await invoke("create_working_copy", { path: source });
    tab = { path, sourcePath: source, edited: false, page: 1, zoom: "fit" };
    tabs.push(tab);
    await showTab(tab, true);
  } catch (err) {
    alert(`Could not open PDF:\n${err}`);
    if (tab) await dropTab(tab);
  } finally {
    hideProgress();
  }
}

// Shows a tab's PDF: its working copy, at the page and zoom it was left
// at. `fresh` = just opened, so run the on-open checks.
async function showTab(tab, fresh) {
  // The mail merge boxes belong to the tab they were drawn on.
  if (activeTab && activeTab !== tab) activeTab.mergeSetup = currentMergeSetup();
  activeTab = tab;
  renderTabs();
  pages.picked = new Set();
  pages.anchor = null;
  toolsOff();
  clearSelection();
  clearMergeBoxes();
  renderMergePanel();
  setProgress(fresh ? 15 : 20, "Counting pages…");
  const pageCount = await invoke("open_pdf", { path: tab.path });
  setProgress(30, "Reading page boxes…");
  state.path = tab.path;
  state.sourcePath = tab.sourcePath;
  state.guides = tab.guides || (tab.guides = []);
  setEdited(tab.edited);
  state.page = Math.min(Math.max(1, tab.page || 1), pageCount);
  state.pageCount = pageCount;
  state.separationNames = [];
  state.activeSeparations = new Set();
  updatePageControls();
  updateRasterDpi();
  resetRecolour();
  loadSpotSwatches(); // real spot colours for the swatches
  loadSeparationsList(); // in the background; the page shows straight away
  await loadPageBoxes();
  view.zoom = tab.zoom || "fit";
  await showPage({ reloadDoc: true });
  enableFixes();
  if (fresh) {
    try {
      tab.mergeSetup = await invoke("merge_setup_read", { path: tab.path });
    } catch (err) {
      console.error(err);
    }
  }
  if (tab.mergeSetup) await restoreMergeSetup(tab.mergeSetup);
  if (fresh) {
    fontWarningQueued = false;
    fontDialog.classList.add("hidden");
    checkOverprintOnOpen(); // pop-up warning if any non-black colour overprints
    loadFonts({ warn: true });
  } else {
    loadFonts();
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
  els.exportBtn.disabled = !state.path;
  document.getElementById("printBtn").disabled = !state.path;
  if (activeTab) {
    activeTab.edited = edited;
    renderTabs();
  }
}

// Save PDF as (Ctrl+S): writes the working copy to a file of the user's
// choosing; the tab then carries that file's name.
async function exportPdf() {
  if (!state.path) return;
  const suggested = state.edited ? state.sourcePath.replace(/(\.pdf)?$/i, "_edited.pdf") : state.sourcePath;
  const dest = await save({
    defaultPath: suggested,
    filters: [{ name: "PDF", extensions: ["pdf"] }],
  });
  if (!dest) return;
  try {
    // Boxes not yet made into pages are kept inside the PDF, so reopening
    // it brings them back.
    const setup = merge.page && merge.page <= state.pageCount ? currentMergeSetup() : null;
    await invoke("export_pdf", { from: state.path, dest, mergeSetup: setup });
    state.sourcePath = dest;
    if (activeTab) {
      activeTab.sourcePath = dest;
      activeTab.saved = true;
    }
    setEdited(false);
  } catch (err) {
    alert(`Save failed:\n${err}`);
  }
}

function updatePageControls() {
  clearInks(); // readings belong to the previous page/file
  hideInkLimit(); // so is the ink limit highlight, until it's worked out again
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
  updateInkLimit();
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
    swatch.style.background = plateColour(name);

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

// --- ink limit ---
// Everywhere the plates add up to more than the limit (280% unless the
// user types another) is shown in bright green over the page. Both the
// tick box and the limit are remembered.

// Looked up when used, as the page controls can call in before this runs.
var inkLimitRequest = 0;
function inkLimitEl(id) {
  return document.getElementById(id);
}

function inkLimitLayer() {
  let img = document.getElementById("inkLimitLayer");
  if (!img) {
    img = document.createElement("img");
    img.id = "inkLimitLayer";
    img.alt = "";
    img.className = "hidden";
    els.imgWrap.appendChild(img);
  }
  return img;
}

function hideInkLimit() {
  inkLimitRequest++;
  inkLimitLayer().classList.add("hidden");
  inkLimitEl("inkLimitInfo").textContent = "";
}

async function updateInkLimit() {
  const limit = Number.parseFloat(inkLimitEl("inkLimit").value);
  if (!inkLimitEl("inkLimitOn").checked || !state.path || !(limit > 0)) {
    hideInkLimit();
    return;
  }
  const req = ++inkLimitRequest;
  const forPath = state.path;
  const forPage = state.page;
  let res;
  try {
    res = await invoke("ink_limit_overlay", { path: forPath, page: forPage, dpi: SAMPLE_DPI, limit });
  } catch (err) {
    if (req === inkLimitRequest) inkLimitEl("inkLimitInfo").textContent = String(err);
    return;
  }
  if (req !== inkLimitRequest || forPath !== state.path || forPage !== state.page) return;
  const img = inkLimitLayer();
  img.src = res.image;
  img.classList.toggle("hidden", res.over === 0);
  const max = Math.round(res.max);
  if (res.over === 0) {
    inkLimitEl("inkLimitInfo").textContent = `Nothing over ${limit}% on this page (highest ${max}%).`;
    inkLimitEl("inkLimitInfo").className = "ink-limit__info ok";
  } else {
    const share = res.over * 100;
    const pct = share < 1 ? "under 1" : String(Math.round(share));
    inkLimitEl("inkLimitInfo").textContent = `Over ${limit}% on ${pct}% of this page (highest ${max}%).`;
    inkLimitEl("inkLimitInfo").className = "ink-limit__info warn";
  }
}

try {
  const saved = JSON.parse(localStorage.getItem("inkLimit") || "null");
  if (saved) {
    inkLimitEl("inkLimitOn").checked = !!saved.on;
    if (saved.limit > 0) inkLimitEl("inkLimit").value = String(saved.limit);
  }
} catch {}

function saveInkLimit() {
  try {
    localStorage.setItem("inkLimit", JSON.stringify({ on: inkLimitEl("inkLimitOn").checked, limit: Number.parseFloat(inkLimitEl("inkLimit").value) }));
  } catch {}
}

inkLimitEl("inkLimitOn").addEventListener("change", () => {
  saveInkLimit();
  updateInkLimit();
});
inkLimitEl("inkLimit").addEventListener("change", () => {
  saveInkLimit();
  updateInkLimit();
});
inkLimitEl("inkLimit").addEventListener("keydown", (ev) => {
  if (ev.key === "Enter") inkLimitEl("inkLimit").blur();
});

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
const isVectorMode = () => !state.simulateOverprint && allPlatesOn() && !proofOn();

async function loadPdfjs() {
  if (!view.lib) {
    view.lib = await import("./vendor/pdfjs/pdf.min.mjs");
    view.lib.GlobalWorkerOptions.workerSrc = new URL("vendor/pdfjs/pdf.worker.min.mjs", location.href).href;
  }
  return view.lib;
}

const RANGE_CHUNK = 256 * 1024;

// Gives pdf.js the file a piece at a time (HTTP range requests to the
// app's own file server), starting with the first chunk.
async function rangeTransport(lib, src) {
  const get = async (begin, end) => {
    const res = await fetch(src, { headers: { Range: `bytes=${begin}-${end - 1}` } });
    if (res.status !== 206 && res.status !== 200) throw new Error(`HTTP ${res.status}`);
    return res;
  };
  const first = await get(0, RANGE_CHUNK);
  const total = first.status === 206 ? Number((first.headers.get("content-range") || "").split("/")[1]) : NaN;
  const initial = new Uint8Array(await first.arrayBuffer());
  const length = Number.isFinite(total) && total > 0 ? total : initial.length;
  const transport = new lib.PDFDataRangeTransport(length, initial);
  transport.length = length;
  // The file server caps each reply (about 1 MB), so larger requests are
  // read in chunk-sized parts and handed over together.
  transport.requestDataRange = (begin, end) => {
    const parts = [];
    for (let at = begin; at < end; at += RANGE_CHUNK) {
      parts.push(get(at, Math.min(end, at + RANGE_CHUNK)).then((res) => res.arrayBuffer()));
    }
    Promise.all(parts)
      .then((bufs) => {
        const out = new Uint8Array(end - begin);
        let pos = 0;
        for (const b of bufs) {
          out.set(new Uint8Array(b), pos);
          pos += b.byteLength;
        }
        transport.onDataRange(begin, out);
      })
      .catch((err) => console.error("Range read failed:", err));
  };
  return transport;
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
  const url = (p) => new URL(p, location.href).href;
  const options = {
    cMapUrl: url("vendor/pdfjs/cmaps/"),
    cMapPacked: true,
    standardFontDataUrl: url("vendor/pdfjs/standard_fonts/"),
    wasmUrl: url("vendor/pdfjs/wasm/"),
    iccUrl: url("vendor/pdfjs/iccs/"),
    isEvalSupported: false,
    enableXfa: false,
  };
  // Read the working copy straight from disk in pieces (only what the
  // visible page needs), so big files show quickly. The query string makes
  // each reload fetch the edited file rather than a cached copy. If that
  // isn't available, fall back to passing the whole file over.
  view.docVersion = (view.docVersion || 0) + 1;
  const forPath = state.path;
  let doc = null;
  try {
    const range = await rangeTransport(lib, `${window.__TAURI__.core.convertFileSrc(forPath)}?v=${view.docVersion}`);
    view.loadingTask = lib.getDocument({
      ...options,
      range,
      length: range.length,
      disableAutoFetch: true,
      disableStream: true,
      rangeChunkSize: RANGE_CHUNK,
    });
    doc = await view.loadingTask.promise;
  } catch (err) {
    console.warn("Range loading failed, reading the whole file instead:", err);
    view.loadingTask?.destroy().catch(() => {});
    const buf = await invoke("read_pdf", { path: forPath });
    if (progress.active) setProgress(60, "Parsing PDF…");
    view.loadingTask = lib.getDocument({ ...options, data: new Uint8Array(buf) });
    doc = await view.loadingTask.promise;
  }
  view.doc = doc;
  if (progress.active) setProgress(80, "Drawing page…");
  buildThumbs();
}

async function loadVectorPage() {
  view.page = await view.doc.getPage(state.page);
  // Remember the page's own (crop box) view for printing.
  if (view.page._pageInfo && !view.page._cropView) view.page._cropView = [...view.page._pageInfo.view];
  // pdf.js shows the CropBox by default; show the whole MediaBox instead so
  // bleed is visible and the view lines up with Ghostscript and the box
  // overlay (both MediaBox based).
  const media = state.pageBoxes && state.pageBoxes.media.rect;
  if (media && view.page._pageInfo) view.page._pageInfo.view = [...media];
}

// --- page thumbnails (left strip, like Acrobat's Page Thumbnails) ---
// Rebuilt whenever the document is (re)loaded, e.g. after a rotation.
// Thumbnails render lazily as they scroll into view.

// Longest side of a thumbnail, CSS px. Dragging the panel's right edge
// changes it (remembered); thumbnails stretch while dragging and are
// redrawn sharp at the new size when the drag ends.
const THUMB_MIN = 80;
const THUMB_MAX = 400;
let thumbSize = 120;
try {
  const saved = Number(localStorage.getItem("thumbSize"));
  if (saved >= THUMB_MIN && saved <= THUMB_MAX) thumbSize = saved;
} catch {}
document.getElementById("thumbs").style.setProperty("--thumb-size", `${thumbSize}px`);

(() => {
  const handle = document.getElementById("thumbsResizer");
  const box = document.getElementById("thumbs");
  let drag = null;
  handle.addEventListener("mousedown", (ev) => {
    if (ev.button !== 0) return;
    ev.preventDefault();
    drag = { x: ev.clientX, size: thumbSize };
    handle.classList.add("dragging");
    document.body.style.cursor = "col-resize";
  });
  window.addEventListener("mousemove", (ev) => {
    if (!drag) return;
    thumbSize = Math.round(Math.min(THUMB_MAX, Math.max(THUMB_MIN, drag.size + ev.clientX - drag.x)));
    box.style.setProperty("--thumb-size", `${thumbSize}px`);
  });
  window.addEventListener("mouseup", () => {
    if (!drag) return;
    const changed = drag.size !== thumbSize;
    drag = null;
    handle.classList.remove("dragging");
    document.body.style.cursor = "";
    if (!changed) return;
    try {
      localStorage.setItem("thumbSize", String(thumbSize));
    } catch {}
    buildThumbs(); // redraw sharp at the new size
  });
})();

function buildThumbs() {
  const box = document.getElementById("thumbs");
  const list = document.getElementById("thumbList");
  const scroll = box.scrollTop;
  if (view.thumbObserver) view.thumbObserver.disconnect();
  list.replaceChildren();
  box.classList.toggle("hidden", !view.doc);
  document.getElementById("thumbsResizer").classList.toggle("hidden", !view.doc);
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
      else pickPage(n, ev);
    });
    list.appendChild(item);
    observer.observe(item);
  }
  box.scrollTop = scroll;
  markCurrentThumb();
}

async function renderThumb(doc, item) {
  const page = await doc.getPage(Number(item.dataset.page));
  if (doc !== view.doc) return; // document reloaded meanwhile
  const base = page.getViewport({ scale: 1 });
  const longest = Math.max(base.width, base.height);
  const scale = thumbSize / longest;
  const dpr = window.devicePixelRatio || 1;
  const viewport = page.getViewport({ scale: scale * dpr });
  const canvas = document.createElement("canvas");
  canvas.width = Math.ceil(viewport.width);
  canvas.height = Math.ceil(viewport.height);
  // Sized from the panel's thumbnail size, so it follows a drag at once.
  canvas.style.width = `calc(var(--thumb-size) * ${(base.width / longest).toFixed(4)})`;
  canvas.style.height = `calc(var(--thumb-size) * ${(base.height / longest).toFixed(4)})`;
  await page.render({ canvasContext: canvas.getContext("2d"), viewport, background: "white" }).promise;
  item.querySelector(".thumb__img").replaceChildren(canvas);
}

function markCurrentThumb() {
  const box = document.getElementById("thumbs");
  for (const el of box.querySelectorAll(".thumb")) {
    const n = Number(el.dataset.page);
    const current = n === state.page;
    el.classList.toggle("current", current);
    el.classList.toggle("picked", pages.picked.has(n));
    if (current) el.scrollIntoView({ block: "nearest" });
  }
  const del = document.getElementById("deletePageBtn");
  del.title = pages.picked.size > 1
    ? `Delete the ${pages.picked.size} selected pages (Delete key after clicking in this panel)`
    : "Delete this page (Delete key after clicking in this panel). Ctrl-click or Shift-click pages to select several";
}

// Selected pages in the Pages panel: a click selects one page (and shows
// it), Ctrl-click adds or removes a page, Shift-click selects a run.
const pages = { picked: new Set(), anchor: null };

function pickPage(n, ev) {
  if (ev.ctrlKey || ev.metaKey) {
    if (!pages.picked.size) pages.picked.add(state.page);
    if (pages.picked.has(n) && pages.picked.size > 1) pages.picked.delete(n);
    else pages.picked.add(n);
    pages.anchor = n;
    markCurrentThumb();
    return;
  }
  if (ev.shiftKey) {
    const from = pages.anchor ?? state.page;
    pages.picked = new Set();
    for (let i = Math.min(from, n); i <= Math.max(from, n); i++) pages.picked.add(i);
    markCurrentThumb();
    return;
  }
  pages.picked = new Set([n]);
  pages.anchor = n;
  if (n !== state.page) goToPageNumber(n);
  else markCurrentThumb();
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
    const pad = 48 + RULER; // #viewport padding, both sides, plus the ruler
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
  document.getElementById("selectToolBtn").disabled = false;
  document.getElementById("mergeToolBtn").disabled = false;
  setTextLayerScale();
  drawRulers();
  renderGuides();
  if (merge.fields.length) renderMergeText();
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
  const key = `${state.path}|${state.page}|${dpi}|${state.simulateOverprint}|${[...state.activeSeparations].sort()}|${Object.keys(state.spotSwatches || {}).length}`;
  const proof = proofOn() ? proofChoice() : null;
  const fullKey = `${key}|${proof ? `${proof.id || proof.file}|${proof.paper}` : ""}`;
  if (fullKey === state.rasterKey) return;
  const token = ++state.renderToken;
  setLoading(true);
  try {
    if (proof) {
      // Soft proof through the chosen press profile (all plates).
      const uri = await invoke("render_proof", {
        path: state.path,
        page: state.page,
        dpi,
        simulate: state.simulateOverprint,
        profileId: proof.id || null,
        profileFile: proof.file || null,
        paper: proof.paper,
      });
      if (token !== state.renderToken) return;
      els.preview.src = uri;
      state.rasterKey = fullKey;
      return;
    }
    let dataUri;
    // Built from the separations, so overprint is exact and spot inks
    // show as opaque (on top, like metallic ink) rather than mixing in.
    dataUri = await invoke("render_separation_composite", {
      path: state.path,
      page: state.page,
      dpi,
      // All plates while the plate list is (re)loading after an edit.
      active: allPlatesOn() ? null : [...state.activeSeparations],
      spotColours: state.spotSwatches || {},
      opaqueSpots: true,
    });
    if (token !== state.renderToken) return; // a newer render superseded this one
    els.preview.src = dataUri;
    state.rasterKey = fullKey;
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
  if (!pages.picked.has(next)) pages.picked = new Set(); // moved on by other means
  updatePageControls();
  updateRasterDpi();
  loadSeparationsList();
  clearSelection();
  setProgress(40, `Loading page ${next}…`);
  try {
    await loadPageBoxes();
    setProgress(70, "Drawing page…");
    await showPage();
  } finally {
    hideProgress();
  }
  recolourFind();
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
  if (!container) return; // rotation is done from the Pages panel now
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
  resetRecolour(); // outlines would be in the old place after a rotation
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
  drawRecolourOverlay(svg);
  drawMergeOverlay(svg);
  drawSelectionOverlay(svg);
}

// --- wiring ---

els.winMin.addEventListener("click", () => appWindow.minimize());
els.winMax.addEventListener("click", () => appWindow.toggleMaximize());
els.winClose.addEventListener("click", () => appWindow.close());

// Closing the window (the × button, Alt+F4, the taskbar) with edits that
// haven't been exported asks first.
const closeDialog = document.getElementById("closeDialog");
let closeConfirmed = false;

// Asks what to do with `name`'s unsaved changes: "export", "discard" or
// "cancel".
function askBeforeClose(name, discardLabel = "Close without saving") {
  document.getElementById("closeDialogFile").textContent = name;
  document.getElementById("closeDiscard").textContent = discardLabel;
  return new Promise((resolve) => {
    closeDialog.classList.remove("hidden");
    const done = (choice) => {
      closeDialog.classList.add("hidden");
      for (const [id, fn] of handlers) document.getElementById(id).removeEventListener("click", fn);
      resolve(choice);
    };
    const handlers = [
      ["closeExport", () => done("export")],
      ["closeDiscard", () => done("discard")],
      ["closeCancel", () => done("cancel")],
    ];
    for (const [id, fn] of handlers) document.getElementById(id).addEventListener("click", fn);
  });
}

appWindow.onCloseRequested(async (event) => {
  saveTabState();
  if (closeConfirmed || !tabs.some((t) => t.edited)) return;
  event.preventDefault();
  for (const tab of tabs.filter((t) => t.edited)) {
    await switchTab(tab);
    const choice = await askBeforeClose(baseName(tab.sourcePath), "Don't save");
    if (choice === "export") {
      await exportPdf();
      if (state.edited) return; // save cancelled or failed: stay open
    } else if (choice !== "discard") {
      return;
    }
  }
  closeConfirmed = true;
  appWindow.close();
});


// --- eyedropper ---
// Hovering the page fills in each plate's ink % (and the total) in the
// Separations list, read from Ghostscript's tiffsep plates so overprint
// is taken into account. Only one request is in flight at a time; the
// latest pointer position is sent once it returns.

function clearInks() {
  for (const el of els.separationsList.querySelectorAll(".ink-value")) el.textContent = "";
}

function showInks(samples) {
  if (!eyedrop.inside) return; // pointer has left the page
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
  if (!state.path || !state.separationNames.length) return;
  sampleAt(...pointerFraction(ev));
});

els.imgWrap.addEventListener("mouseleave", () => {
  eyedrop.inside = false;
  eyedrop.pending = null;
  clearInks();
});

els.imgWrap.addEventListener("click", (ev) => {
  if (state.recolourTool && state.path) recolourPick(...pointerFraction(ev));
});

els.emptyOpenBtn.addEventListener("click", openPdf);
els.openBtn.addEventListener("click", openPdf);
els.exportBtn.addEventListener("click", exportPdf);
els.prevPage.addEventListener("click", () => goToPage(-1));
els.nextPage.addEventListener("click", () => goToPage(1));

els.overprintToggle.addEventListener("change", () => {
  state.simulateOverprint = els.overprintToggle.checked;
  renderCurrent();
});

// --- colour profile preview (soft proof) ---
// The tick by the Colour Profiles heading shows the page through a CMYK
// press profile: the PDF's output intent, another CMYK profile embedded
// in it, or an .icc file the user adds (remembered). Only profiles from
// the user's own files are used.

const proofToggle = document.getElementById("proofToggle");
const proofSelect = document.getElementById("proofProfile");
const proofPaper = document.getElementById("proofPaper");
let proofFile = null; // { path, name } of a chosen .icc file
var lastProfiles = []; // the open PDF's profiles, from loadProfiles
try {
  proofFile = JSON.parse(localStorage.getItem("proofFile") || "null");
} catch {}

function proofOn() {
  return proofToggle.checked && !!proofSelect.value;
}

// { id } (embedded) or { file } (chosen file), plus the paper setting.
function proofChoice() {
  const v = proofSelect.value;
  return v.startsWith("file:") ? { file: v.slice(5), paper: proofPaper.checked } : { id: v, paper: proofPaper.checked };
}

// Fills the "Preview with" menu: the open PDF's CMYK profiles (output
// intent first), then the user's own profile if they added one.
function fillProofProfiles(profiles) {
  const before = proofSelect.value;
  const opts = [];
  for (const p of profiles.filter((x) => x.colour_space === "CMYK")) {
    opts.push([p.id, p.output_intent ? `Output intent: ${p.condition || p.name}` : p.name]);
  }
  if (proofFile) opts.push([`file:${proofFile.path}`, `${proofFile.name} (your profile)`]);
  if (!opts.length) opts.push(["", "No CMYK profile: add your own"]);
  proofSelect.replaceChildren(
    ...opts.map(([value, label]) => {
      const o = document.createElement("option");
      o.value = value;
      o.textContent = label;
      return o;
    }),
  );
  proofSelect.value = opts.some(([v]) => v === before) ? before : opts[0][0];
  fillIntentOptions(profiles);
  if (proofToggle.checked) renderCurrent();
}

// --- output intent ---
// Starts on the PDF's own output intent (or None). Choosing another
// profile and clicking Apply saves it into the working copy; the CMYK
// values are not changed. Undo takes it back.

const intentSelect = document.getElementById("intentSelect");
const intentApplyBtn = document.getElementById("intentApplyBtn");
let intentCurrent = "none";

function fillIntentOptions(profiles) {
  const current = profiles.find((p) => p.output_intent);
  intentCurrent = current ? current.id : "none";
  const opts = [["none", "None"]];
  for (const p of profiles.filter((x) => x.colour_space === "CMYK")) {
    opts.push([p.id, p.output_intent ? `${p.condition && p.condition !== "Custom" ? p.condition : p.name} (current)` : p.name]);
  }
  if (proofFile) opts.push([`file:${proofFile.path}`, `${proofFile.name} (your profile)`]);
  intentSelect.replaceChildren(
    ...opts.map(([value, label]) => {
      const o = document.createElement("option");
      o.value = value;
      o.textContent = label;
      return o;
    }),
  );
  intentSelect.value = intentCurrent;
  intentApplyBtn.disabled = true;
}

intentSelect.addEventListener("change", () => {
  intentApplyBtn.disabled = !state.path || intentSelect.value === intentCurrent;
});

intentApplyBtn.addEventListener("click", async () => {
  if (!state.path) return;
  const v = intentSelect.value;
  intentApplyBtn.disabled = true;
  try {
    await invoke("set_output_intent", {
      path: state.path,
      profileId: v !== "none" && !v.startsWith("file:") ? v : null,
      profileFile: v.startsWith("file:") ? v.slice(5) : null,
    });
    setEdited(true);
    await loadProfiles();
  } catch (err) {
    alert(`Could not set the output intent:\n${err}`);
    intentApplyBtn.disabled = false;
  }
});

// Asks for an .icc file; true when a CMYK profile was chosen.
async function chooseProofFile() {
  const selected = await open({ multiple: false, filters: [{ name: "ICC profile", extensions: ["icc", "icm", "ICC", "ICM"] }] });
  if (!selected) return false;
  const path = Array.isArray(selected) ? selected[0] : selected;
  let info;
  try {
    info = await invoke("read_icc_file", { path });
  } catch (err) {
    alert(String(err));
    return false;
  }
  if (info.colour_space !== "CMYK") {
    alert(`${info.name} is a ${info.colour_space} profile. Previewing print colours needs a CMYK press profile.`);
    return false;
  }
  proofFile = { path, name: info.name };
  try {
    localStorage.setItem("proofFile", JSON.stringify(proofFile));
  } catch {}
  return true;
}

document.getElementById("proofAddBtn").addEventListener("click", async () => {
  if (!(await chooseProofFile())) return;
  proofToggle.checked = true;
  fillProofProfiles(lastProfiles);
  proofSelect.value = `file:${proofFile.path}`;
  renderCurrent();
});
proofSelect.addEventListener("change", () => {
  if (proofToggle.checked) renderCurrent();
});
proofToggle.addEventListener("change", async () => {
  // Nothing to preview with yet: ask for a profile.
  if (proofToggle.checked && !proofSelect.value) {
    if (!(await chooseProofFile())) {
      proofToggle.checked = false;
      return;
    }
    fillProofProfiles(lastProfiles);
    proofSelect.value = `file:${proofFile.path}`;
  }
  renderCurrent();
});
proofPaper.addEventListener("change", () => {
  if (proofOn()) renderCurrent();
});
fillProofProfiles([]);

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
    resetRecolour();
    state.separationNames = [];
    state.activeSeparations = new Set();
    await loadPageBoxes();
    updateRasterDpi();
    loadSpotSwatches();
    loadSeparationsList();
    setEdited(true);
    await showPage({ reloadDoc: true });
    enableFixes();
    loadFonts();
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

// --- recolour tool ---
// Click an object (shape or text) to pick it: the panel shows its fill and
// stroke. Choose what to change (just it, or everything with the same
// fill/stroke, on this page or all pages per "Apply edits to"), type the
// new CMYK values and apply. Matches on the current page are outlined in
// gold.

const recolour = { picked: null, found: null, findToken: 0 };
const PROCESS_INKS = ["C", "M", "Y", "K"];

function setRecolourTool(on) {
  state.recolourTool = on;
  document.getElementById("recolourToolBtn")?.classList.toggle("active", on);
  els.viewport.classList.toggle("recolour-tool", on);
  if (on) {
    toolsOff("recolour");
    setTextMode(false);
    // Bring the Recolour panel into view and make it sparkle for a moment.
    const panel = document.getElementById("recolourPanel");
    panel.scrollIntoView({ behavior: "smooth", block: "start" });
    sparkle(panel);
    clearInks();
  } else {
    setTextMode(true); // back to the default: text can be selected
  }
  renderRecolourPanel(); // the hint depends on whether the tool is on
}

// Spinning CMYK border plus twinkling sparkles around an element for 5 s.
function sparkle(el) {
  clearTimeout(el._sparkleTimer);
  el.querySelectorAll(".rc-spark").forEach((s) => s.remove());
  el.classList.remove("sparkle");
  void el.offsetWidth; // restart the animation
  el.classList.add("sparkle");
  const colours = ["#00aeef", "#ec008c", "#fff200", "#ffcc33"];
  for (let i = 0; i < 10; i++) {
    const s = document.createElement("span");
    s.className = "rc-spark";
    s.textContent = "✦";
    // Around the edge: pick a side, then a spot along it.
    const t = Math.random() * 100;
    const side = i % 4;
    s.style.left = side === 0 ? "-14px" : side === 1 ? "calc(100% + 2px)" : `${t}%`;
    s.style.top = side === 2 ? "-14px" : side === 3 ? "calc(100% - 4px)" : `${t}%`;
    s.style.color = colours[i % colours.length];
    s.style.animationDelay = `${(Math.random() * 0.8).toFixed(2)}s, 0s`;
    el.appendChild(s);
  }
  el._sparkleTimer = setTimeout(() => stopSparkle(el), 5000);
}

function stopSparkle(el) {
  clearTimeout(el._sparkleTimer);
  el.classList.remove("sparkle");
  el.querySelectorAll(".rc-spark").forEach((s) => s.remove());
}

// The Recolour panel recolours the selected objects; "same colour"
// matching is the Global Colour Change panel's.
const rcMode = () => "object";
const gMode = () => document.querySelector('input[name="gMode"]:checked').value;

// CSS colour for a picked colour (spots use their swatch colour).
function rcSwatch(info) {
  if (!info) return "transparent";
  if (info.hex) return info.hex;
  return info.inks.length ? plateColour(info.inks[0]) : "#888";
}

function cmykToCss([c, m, y, k]) {
  const f = (v) => Math.round(255 * (1 - v / 100) * (1 - k / 100));
  return `rgb(${f(c)}, ${f(m)}, ${f(y)})`;
}

function rcInputs(which) {
  return [...document.querySelectorAll(`.rc-cmyk[data-for="${which}"] input`)];
}

function rcValues(which) {
  return rcInputs(which).map((i) => Math.min(100, Math.max(0, Number.parseFloat(i.value) || 0)));
}

function buildCmykInputs() {
  for (const box of document.querySelectorAll(".rc-cmyk")) {
    box.replaceChildren();
    for (const ink of PROCESS_INKS) {
      const label = document.createElement("label");
      label.textContent = `${ink} %`;
      const input = document.createElement("input");
      input.type = "number";
      input.min = "0";
      input.max = "100";
      input.step = "1";
      input.value = "0";
      input.addEventListener("input", updateNewSwatches);
      label.appendChild(input);
      box.appendChild(label);
    }
  }
}

function updateNewSwatches() {
  document.getElementById("rcFillNewSwatch").style.background = cmykToCss(rcValues("fill"));
  document.getElementById("rcStrokeNewSwatch").style.borderColor = cmykToCss(rcValues("stroke"));
  document.getElementById("mergeSwatch").style.background = cmykToCss(rcValues("merge"));
}

// Which of the new fill / stroke editors apply in the chosen mode.
function rcChannels() {
  const p = recolour.picked;
  const mode = rcMode();
  return {
    fill: !!(p && p.fill) && mode !== "stroke",
    stroke: !!(p && p.stroke) && mode !== "fill",
  };
}

function renderRecolourPanel() {
  // The whole panel is always shown; until an object is picked everything
  // is greyed out and the hint says what to do.
  const p = recolour.picked;
  const empty = document.getElementById("recolourEmpty");
  empty.classList.toggle("hidden", !!p);
  empty.innerHTML = "Select a shape, text or logo with the <strong>Select</strong> tool (V) to change its colour.";
  document.getElementById("recolourBody").classList.toggle("waiting", !p);
  for (const id of ["rcApplyBtn", "rcClearBtn"]) document.getElementById(id).disabled = !p;
  if (!p) {
    for (const id of ["rcFillText", "rcStrokeText"]) document.getElementById(id).textContent = "–";
    for (const id of ["rcFillSwatch", "rcStrokeSwatch"]) document.getElementById(id).style.visibility = "hidden";
    for (const which of ["Fill", "Stroke"]) document.getElementById(`rcNew${which}`).classList.add("disabled");
    for (const i of [...rcInputs("fill"), ...rcInputs("stroke"), document.getElementById("rcFillOn"), document.getElementById("rcStrokeOn")]) {
      i.disabled = true;
    }
    return;
  }

  const show = (swatch, text, info, stroke) => {
    document.getElementById(text).textContent = info ? info.label : "none";
    const sw = document.getElementById(swatch);
    sw.style.visibility = info ? "visible" : "hidden";
    if (stroke) sw.style.borderColor = rcSwatch(info);
    else sw.style.background = rcSwatch(info);
  };
  show("rcFillSwatch", "rcFillText", p.fill, false);
  show("rcStrokeSwatch", "rcStrokeText", p.stroke, true);

  const n = sel.items.length;
  document.getElementById("rcCount").textContent = n ? `${n} selected object${n === 1 ? "" : "s"}` : "";

  const ch = rcChannels();
  document.getElementById("rcNewFill").classList.toggle("disabled", !ch.fill);
  document.getElementById("rcNewStroke").classList.toggle("disabled", !ch.stroke);
  for (const i of [...rcInputs("fill"), document.getElementById("rcFillOn")]) i.disabled = !ch.fill;
  for (const i of [...rcInputs("stroke"), document.getElementById("rcStrokeOn")]) i.disabled = !ch.stroke;
}

function setRcResult(text, cls = "") {
  const el = document.getElementById("rcResult");
  el.textContent = text;
  el.className = `rc-result ${cls}`;
}

// The Recolour panel works on the Select tool's selection: its first
// coloured object gives the colours shown and the "same colour" matches.
function syncRecolourFromSelection() {
  const o = sel.items.find((x) => x.paint !== null && x.paint !== undefined);
  if (!o) {
    if (recolour.picked) resetRecolour();
    return;
  }
  const key = sel.items.map((x) => x.index).join(",");
  if (recolour.picked && recolour.selKey === key) {
    renderRecolourPanel();
    return;
  }
  recolour.selKey = key;
  recolour.picked = { target: { page: state.page, index: o.paint }, kind: o.kind, fill: o.fill, stroke: o.stroke, norm: o.norm };
  rcInputs("fill").forEach((input, i) => (input.value = String(o.fill ? o.fill.cmyk[i] : 0)));
  rcInputs("stroke").forEach((input, i) => (input.value = String(o.stroke ? o.stroke.cmyk[i] : 0)));
  const seed = o.fill || o.stroke;
  rcInputs("global").forEach((input, i) => (input.value = String(seed ? seed.cmyk[i] : 0)));
  document.getElementById("gNewSwatch").style.background = cmykToCss(rcValues("global"));
  updateNewSwatches();
  setRcResult("");
  setGResult("");
  renderRecolourPanel();
  recolourFind();
}

function resetRecolour() {
  recolour.selKey = null;
  recolour.picked = null;
  recolour.found = null;
  recolour.findToken++;
  document.getElementById("rcCount").textContent = "";
  setRcResult("");
  renderRecolourPanel();
  renderGlobalPanel();
  drawBoxOverlay();
}

// [1, 2, 3, 5] → "1–3, 5"
function pageRanges(pages) {
  const out = [];
  for (let i = 0; i < pages.length; i++) {
    let j = i;
    while (j + 1 < pages.length && pages[j + 1] === pages[j] + 1) j++;
    out.push(i === j ? `${pages[i]}` : `${pages[i]}–${pages[j]}`);
    i = j;
  }
  return out.join(", ");
}

// Global Colour Change: counts and outlines every object that shares the
// selection's colour, on the pages in scope.
async function recolourFind() {
  renderGlobalPanel();
  const p = recolour.picked;
  if (!p || !state.path) return;
  const token = ++recolour.findToken;
  const count = document.getElementById("gCount");
  count.textContent = "Finding matches…";
  try {
    const found = await invoke("recolour_find", {
      path: state.path,
      page: state.page,
      target: p.target,
      mode: gMode(),
      allPages: editAllPages(),
    });
    if (token !== recolour.findToken) return;
    recolour.found = found;
    const n = found.count;
    count.textContent = n
      ? `${n} object${n > 1 ? "s" : ""} on page${found.pages.length > 1 ? "s" : ""} ${pageRanges(found.pages)}`
      : "Nothing matches on the pages in scope.";
  } catch (err) {
    if (token !== recolour.findToken) return;
    recolour.found = null;
    count.textContent = String(err);
  }
  drawBoxOverlay();
}

async function recolourPick(x, y) {
  setRcResult("");
  let picked;
  try {
    picked = await invoke("recolour_pick", {
      path: state.path,
      page: state.page,
      x,
      y,
      tol: 4 / view.scale, // 4 screen pixels, in points
    });
  } catch (err) {
    setRcResult(String(err), "err");
    return;
  }
  if (!picked) {
    resetRecolour();
    setRcResult("Nothing to recolour there. Click a shape or some text.");
    return;
  }
  recolour.picked = picked;
  // Start the editors from the picked colours.
  const fillVals = picked.fill ? picked.fill.cmyk : [0, 0, 0, 0];
  const strokeVals = picked.stroke ? picked.stroke.cmyk : [0, 0, 0, 0];
  rcInputs("fill").forEach((input, i) => (input.value = String(fillVals[i])));
  rcInputs("stroke").forEach((input, i) => (input.value = String(strokeVals[i])));
  updateNewSwatches();
  renderRecolourPanel();
  await recolourFind();
}

async function recolourApply() {
  const p = recolour.picked;
  if (!p || !state.path) return;
  const ch = rcChannels();
  const fill = ch.fill && document.getElementById("rcFillOn").checked ? rcValues("fill") : null;
  const stroke = ch.stroke && document.getElementById("rcStrokeOn").checked ? rcValues("stroke") : null;
  if (!fill && !stroke) {
    setRcResult("Tick New fill or New stroke first.", "err");
    return;
  }
  await runObjectEdit({ action: "colour", fill, stroke }, "Recolouring…");
  const res = document.getElementById("selResult");
  setRcResult(res.textContent, res.classList.contains("err") ? "err" : "ok");
}

// Global Colour Change panel: shows the colour to find and enables the
// options the selection's colours allow.
function renderGlobalPanel() {
  const p = recolour.picked;
  document.getElementById("gEmpty").classList.toggle("hidden", !!p);
  document.getElementById("gBody").classList.toggle("waiting", !p);
  document.getElementById("gApplyBtn").disabled = !p;
  const radios = [...document.querySelectorAll('input[name="gMode"]')];
  const need = { fill: !!p?.fill, stroke: !!p?.stroke, both: !!(p?.fill && p?.stroke) };
  for (const r of radios) {
    r.disabled = !p || !need[r.value];
    r.parentElement.classList.toggle("disabled", r.disabled);
  }
  const checked = radios.find((r) => r.checked);
  if (p && checked?.disabled) {
    const first = radios.find((r) => !r.disabled);
    if (first) first.checked = true;
  }
  const info = p ? (gMode() === "stroke" ? p.stroke : p.fill || p.stroke) : null;
  document.getElementById("gText").textContent = info ? info.label : "–";
  const sw = document.getElementById("gSwatch");
  sw.style.visibility = info ? "visible" : "hidden";
  if (info) sw.style.background = rcSwatch(info);
  if (!p) document.getElementById("gCount").textContent = "";
}

function setGResult(text, cls = "") {
  const el = document.getElementById("gResult");
  el.textContent = text;
  el.className = `rc-result ${cls}`;
}

async function globalApply() {
  const p = recolour.picked;
  if (!p || !state.path) return;
  const mode = gMode();
  const v = rcValues("global");
  const fill = mode !== "stroke" ? v : null;
  const stroke = mode !== "fill" ? v : null;
  const btn = document.getElementById("gApplyBtn");
  btn.disabled = true;
  setGResult("");
  setProgress(30, "Replacing colour…");
  try {
    const n = await invoke("recolour_apply", {
      path: state.path,
      page: state.page,
      target: p.target,
      mode,
      allPages: editAllPages(),
      fill,
      stroke,
    });
    setProgress(60, "Reloading…");
    setEdited(true);
    resetRecolour();
    setGResult(`✓ Changed ${n} object${n === 1 ? "" : "s"}`, "ok");
    refreshSelection();
    state.separationNames = [];
    state.activeSeparations = new Set();
    loadSpotSwatches();
    loadSeparationsList();
    await showPage({ reloadDoc: true });
  } catch (err) {
    setGResult(String(err), "err");
  } finally {
    btn.disabled = false;
    hideProgress();
  }
}

// Gold outlines: every match on this page, plus the picked object.
function drawRecolourOverlay(svg) {
  const p = recolour.picked;
  if (!p) return;
  const rect = ([x, y, w, h], cls) => {
    const r = document.createElementNS(SVG_NS, "rect");
    r.setAttribute("x", x);
    r.setAttribute("y", y);
    r.setAttribute("width", Math.max(w, 0.002));
    r.setAttribute("height", Math.max(h, 0.002));
    r.setAttribute("class", cls);
    svg.appendChild(r);
  };
  // Only while the pointer is over Global Colour Change, so the outlines
  // don't get in the way of selecting.
  if (recolour.found && recolour.showMatches) for (const n of recolour.found.norms) rect(n, "rc-match");

}

buildCmykInputs();
updateNewSwatches();
renderRecolourPanel();
for (const input of document.querySelectorAll('input[name="gMode"]')) input.addEventListener("change", recolourFind);
document.getElementById("gApplyBtn").addEventListener("click", globalApply);
{
  const panel = document.getElementById("globalPanel");
  panel.addEventListener("mouseenter", () => {
    recolour.showMatches = true;
    drawBoxOverlay();
  });
  panel.addEventListener("mouseleave", () => {
    recolour.showMatches = false;
    drawBoxOverlay();
  });
}
for (const input of rcInputs("global")) {
  input.addEventListener("input", () => {
    document.getElementById("gNewSwatch").style.background = cmykToCss(rcValues("global"));
  });
}
document.getElementById("rcApplyBtn").addEventListener("click", recolourApply);
document.getElementById("rcClearBtn").addEventListener("click", clearSelection);

// --- fonts ---
// Lists every font the PDF uses and whether it's embedded. Missing fonts
// also get a pop-up when the file is opened (after the white overprint
// warning, if that's showing).

const fontDialog = document.getElementById("fontDialog");
let fontWarningQueued = false;

function pageList(pages) {
  return pages.length === 1 ? `page ${pages[0]}` : `pages ${pageRanges(pages)}`;
}

// The ICC profiles embedded in the PDF: the output intent first, then the
// profiles its colour spaces use, each with what its header says and a
// button to save it as an .icc file.
async function loadProfiles() {
  const list = document.getElementById("iccList");
  const summary = document.getElementById("iccSummary");
  const forPath = state.path;
  if (!forPath) return;
  list.innerHTML = '<li class="hint">Reading colour profiles…</li>';
  summary.textContent = "";
  let profiles;
  try {
    profiles = await invoke("list_icc_profiles", { path: forPath });
  } catch (err) {
    if (forPath !== state.path) return;
    list.innerHTML = "";
    const li = document.createElement("li");
    li.className = "hint";
    li.textContent = String(err);
    list.appendChild(li);
    return;
  }
  if (forPath !== state.path) return;
  list.innerHTML = "";
  lastProfiles = profiles;
  fillProofProfiles(profiles);
  const intent = profiles.find((p) => p.output_intent);
  summary.className = "font-summary";
  summary.textContent = intent ? `output intent: ${intent.condition || intent.name}` : profiles.length ? "no output intent" : "none";
  if (!profiles.length) {
    list.innerHTML = '<li class="hint">No embedded profiles: colours are plain CMYK, RGB or grey.</li>';
    return;
  }
  const kb = (n) => (n >= 1024 * 1024 ? `${(n / 1024 / 1024).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`);
  for (const p of profiles) {
    const li = document.createElement("li");
    li.className = "ok icc-item";
    const lines = [p.name, `${p.colour_space} · ${p.class} · ICC v${p.version}`];
    if (p.output_intent) lines.push(`Output intent${p.standard ? ` (${p.standard})` : ""}: ${[p.condition, p.condition_info].filter(Boolean).join(" · ")}`);
    if (p.created) lines.push(`Created ${p.created}`);
    if (p.copyright) lines.push(p.copyright);
    li.title = lines.join("\n");
    const name = document.createElement("span");
    name.className = "font-name";
    name.textContent = p.name;
    const status = document.createElement("span");
    status.className = "font-status";
    status.textContent = p.output_intent
      ? `Output intent${p.standard ? ` · ${p.standard}` : ""}${p.condition ? ` · ${p.condition}` : ""}`
      : `${p.colour_space} · ${p.class}`;
    const meta = document.createElement("span");
    meta.className = "font-meta";
    const used = p.pages.length ? pageList(p.pages) : p.output_intent ? "whole document" : "not used on a page";
    meta.textContent = `${p.output_intent ? `${p.colour_space} · ` : ""}ICC v${p.version} · ${kb(p.size)} · ${used}`;
    const saveBtn = document.createElement("button");
    saveBtn.type = "button";
    saveBtn.className = "link-btn icc-save";
    saveBtn.textContent = "Save .icc";
    saveBtn.title = "Save this profile as an .icc file";
    saveBtn.addEventListener("click", async () => {
      const dest = await save({
        defaultPath: `${p.name.replace(/[\\/:*?"<>|]+/g, "").trim() || "profile"}.icc`,
        filters: [{ name: "ICC profile", extensions: ["icc", "icm"] }],
      });
      if (!dest) return;
      try {
        await invoke("save_icc_profile", { path: state.path, id: p.id, dest });
      } catch (err) {
        alert(`Could not save the profile:\n${err}`);
      }
    });
    li.append(name, status, meta, saveBtn);
    list.appendChild(li);
  }
}

async function loadFonts({ warn = false } = {}) {
  loadProfiles(); // the profiles change with the same edits as the fonts
  const list = document.getElementById("fontList");
  const summary = document.getElementById("fontSummary");
  const forPath = state.path;
  if (!forPath) return;
  list.innerHTML = '<li class="hint">Checking fonts…</li>';
  summary.textContent = "";
  let fonts;
  try {
    fonts = await invoke("list_fonts", { path: forPath });
  } catch (err) {
    if (forPath !== state.path) return;
    list.innerHTML = "";
    const li = document.createElement("li");
    li.className = "hint";
    li.textContent = String(err);
    list.appendChild(li);
    return;
  }
  if (forPath !== state.path) return;
  list.innerHTML = "";
  const missing = fonts.filter((f) => !f.embedded);
  if (!fonts.length) {
    summary.textContent = "none";
    list.innerHTML = '<li class="hint">No fonts (no live text, or all outlined).</li>';
    return;
  }
  summary.textContent = missing.length ? `${missing.length} not embedded` : "all embedded";
  summary.className = missing.length ? "font-summary bad" : "font-summary good";
  for (const f of fonts) {
    const li = document.createElement("li");
    li.className = f.embedded ? "ok" : "bad";
    li.title = `${f.name}\n${f.kind}${f.subset ? ", subset" : ""}\nUsed on ${pageList(f.pages)}`;
    const name = document.createElement("span");
    name.className = "font-name";
    name.textContent = f.name;
    const status = document.createElement("span");
    status.className = "font-status";
    status.textContent = f.embedded ? (f.subset ? "✓ Embedded (subset)" : "✓ Embedded") : "⚠ Not embedded";
    const meta = document.createElement("span");
    meta.className = "font-meta";
    meta.textContent = `${f.kind} · ${pageList(f.pages)}`;
    li.append(name, status, meta);
    list.appendChild(li);
  }
  if (warn && missing.length) {
    const dl = document.getElementById("fontDialogList");
    dl.innerHTML = "";
    for (const f of missing) {
      const li = document.createElement("li");
      const page = document.createElement("span");
      page.className = "op-warning__page";
      page.textContent = f.pages.length === 1 ? `Page ${f.pages[0]}` : `Pages ${pageRanges(f.pages)}`;
      const name = document.createElement("span");
      name.className = "op-warning__colour";
      name.textContent = f.name;
      const kind = document.createElement("span");
      kind.className = "op-warning__kind";
      kind.textContent = f.kind;
      li.append(page, name, kind);
      dl.appendChild(li);
    }
    if (overprintDialog.classList.contains("hidden")) fontDialog.classList.remove("hidden");
    else fontWarningQueued = true;
  }
}

// Shows a queued font warning once the overprint warning is closed.
function showQueuedFontWarning() {
  if (fontWarningQueued && overprintDialog.classList.contains("hidden")) {
    fontWarningQueued = false;
    fontDialog.classList.remove("hidden");
  }
}
new MutationObserver(showQueuedFontWarning).observe(overprintDialog, { attributes: true, attributeFilter: ["class"] });
document.getElementById("fontDialogOk").addEventListener("click", () => fontDialog.classList.add("hidden"));
window.addEventListener("keydown", (ev) => {
  if (ev.key === "Escape" && !fontDialog.classList.contains("hidden")) fontDialog.classList.add("hidden");
});

// --- tabs: one per open PDF ---
// Every open PDF has its own working copy and a tab named after its file.
// Switching tabs loads that PDF's working copy again and goes back to the
// page and zoom it was left at.

const tabs = [];
let activeTab = null;
let tabSwitching = false;
const tabBar = document.getElementById("tabBar");

function renderTabs() {
  tabBar.replaceChildren();
  tabBar.classList.toggle("hidden", !tabs.length);
  for (const tab of tabs) {
    const el = document.createElement("div");
    el.className = `tab${tab === activeTab ? " active" : ""}`;
    el.title = tab.sourcePath;
    const name = document.createElement("span");
    name.className = "tab__name";
    name.textContent = baseName(tab.sourcePath);
    el.appendChild(name);
    if (tab.edited) {
      const dot = document.createElement("span");
      dot.className = "tab__dirty";
      dot.textContent = "●";
      dot.title = "Unsaved changes";
      el.appendChild(dot);
    }
    const close = document.createElement("button");
    close.type = "button";
    close.className = "tab__close";
    close.textContent = "×";
    close.title = "Close (Ctrl+W)";
    el.appendChild(close);
    el.addEventListener("click", (ev) => {
      if (ev.target === close) closeTab(tab);
      else switchTab(tab);
    });
    el.addEventListener("auxclick", (ev) => {
      if (ev.button === 1) closeTab(tab); // middle click
    });
    tabBar.appendChild(el);
  }
}

// Remembers where the current tab was left.
function saveTabState() {
  if (!activeTab) return;
  activeTab.page = state.page;
  activeTab.zoom = view.zoom;
  activeTab.edited = state.edited;
}

async function switchTab(tab) {
  if (tab === activeTab || tabSwitching) return;
  tabSwitching = true;
  saveTabState();
  setProgress(10, `Opening ${baseName(tab.sourcePath)}…`);
  try {
    await showTab(tab, false);
  } catch (err) {
    alert(`Could not show ${baseName(tab.sourcePath)}:\n${err}`);
  } finally {
    tabSwitching = false;
    hideProgress();
  }
}

// Closes a tab, asking first if it has unsaved changes.
async function closeTab(tab) {
  if (tab.edited) {
    if (tab !== activeTab) await switchTab(tab);
    const choice = await askBeforeClose(baseName(tab.sourcePath));
    if (choice === "cancel") return;
    if (choice === "export") {
      await exportPdf();
      if (state.edited) return; // save cancelled or failed
    }
  }
  await dropTab(tab);
}

// Removes a tab and its working copy, showing a neighbour (or nothing).
async function dropTab(tab) {
  const i = tabs.indexOf(tab);
  if (i < 0) return;
  tabs.splice(i, 1);
  invoke("close_working_copy", { path: tab.path }).catch((err) => console.error(err));
  if (tab === activeTab) {
    activeTab = null;
    const next = tabs[Math.min(i, tabs.length - 1)];
    if (next) await switchTab(next);
    else showNoFile();
  }
  renderTabs();
}

// Back to the empty start screen after the last tab closes.
function showNoFile() {
  toolsOff();
  clearSelection();
  clearMergeBoxes();
  state.path = null;
  state.sourcePath = null;
  state.guides = null;
  setEdited(false);
  state.page = 1;
  state.pageCount = 1;
  state.pageBoxes = null;
  state.separationNames = [];
  state.activeSeparations = new Set();
  state.rasterKey = null;
  if (view.loadingTask) view.loadingTask.destroy().catch(() => {});
  view.loadingTask = null;
  view.doc = null;
  view.page = null;
  buildThumbs();
  els.vectorLayer.replaceChildren();
  els.preview.classList.add("hidden");
  els.imgWrap.classList.add("empty");
  els.imgWrap.style.width = "";
  els.imgWrap.style.height = "";
  document.getElementById("textLayer").replaceChildren();
  document.getElementById("textStatus").textContent = "";
  document.getElementById("emptyState").classList.remove("hidden");
  els.zoomLabel.textContent = "–";
  drawRulers();
  renderGuides();
  for (const b of [els.zoomIn, els.zoomOut, els.zoomFit, els.zoomActual]) b.disabled = true;
  for (const id of ["textModeBtn", "zoomToolBtn", "selectToolBtn", "mergeToolBtn"]) {
    document.getElementById(id).disabled = true;
  }
  for (const b of fixBtns) b.disabled = true;
  rgbResult.innerHTML = "";
  updatePageControls();
  updateRasterDpi();
  renderPageBoxesPanel();
  renderRotationPanel();
  drawBoxOverlay();
  resetRecolour();
  renderMergePanel();
  els.separationsList.innerHTML = '<li class="hint">Open a PDF to see its separations.</li>';
  document.getElementById("fontList").innerHTML = '<li class="hint">Open a PDF to see its fonts.</li>';
  document.getElementById("fontSummary").textContent = "";
}

// --- pages panel: insert and delete pages ---
// The + button (or dropping PDFs onto the panel) inserts other PDFs'
// pages; the red × (or Delete, after clicking in the panel) deletes the
// current page.

const thumbsBox = document.getElementById("thumbs");

function setPagesFocus(on) {
  state.pagesFocused = on;
  thumbsBox.classList.toggle("focused", on);
}

document.addEventListener("mousedown", (ev) => setPagesFocus(!!ev.target.closest("#thumbs")), true);

// The page count or order changed: reload everything that depends on it.
async function afterPagesChanged(count, page) {
  state.pageCount = count;
  state.page = Math.min(Math.max(1, page), count);
  setEdited(true);
  clearSelection();
  clearMergeBoxes(); // its page may have moved
  renderMergePanel();
  pages.picked = new Set();
  pages.anchor = null;
  resetRecolour();
  state.separationNames = [];
  state.activeSeparations = new Set();
  updatePageControls();
  updateRasterDpi();
  loadSpotSwatches();
  loadSeparationsList();
  setProgress(60, "Reloading…");
  await loadPageBoxes();
  await showPage({ reloadDoc: true });
  loadFonts();
  updateSpotButton();
}

// Inserts every page of `sources` so that `at` pages come before them.
async function insertPagesFrom(sources, at) {
  if (!state.path || !sources.length) return;
  setProgress(30, `Inserting ${sources.length === 1 ? baseName(sources[0]) : `${sources.length} PDFs`}…`);
  try {
    const count = await invoke("insert_pages", { path: state.path, sources, at });
    await afterPagesChanged(count, at + 1);
  } catch (err) {
    alert(`Could not insert pages:\n${err}`);
  } finally {
    hideProgress();
  }
}

// Deletes the selected pages, or the current page when none are selected.
async function deleteCurrentPage() {
  if (!state.path) return;
  const doomed = pages.picked.size ? [...pages.picked].sort((a, b) => a - b) : [state.page];
  if (state.pageCount <= 1) {
    alert("This is the only page, so it can't be deleted.");
    return;
  }
  if (doomed.length >= state.pageCount) {
    alert("A PDF needs at least one page, so not every page can be deleted.");
    return;
  }
  setProgress(30, doomed.length > 1 ? `Deleting ${doomed.length} pages…` : `Deleting page ${doomed[0]}…`);
  try {
    const count = await invoke("delete_page", { path: state.path, pages: doomed });
    pages.picked = new Set();
    pages.anchor = null;
    await afterPagesChanged(count, doomed[0]);
  } catch (err) {
    alert(`Could not delete the page:\n${err}`);
  } finally {
    hideProgress();
  }
}

// The + button opens a menu: insert above the selected page(s) or below
// them. Every page of each chosen PDF is inserted.
const insertMenu = document.getElementById("insertMenu");

function insertRange() {
  const picked = pages.picked.size ? [...pages.picked] : [state.page];
  return [Math.min(...picked), Math.max(...picked)];
}

document.getElementById("insertPagesBtn").addEventListener("click", (ev) => {
  if (!state.path) return;
  ev.stopPropagation();
  const [first, last] = insertRange();
  document.getElementById("insertAboveBtn").textContent = first === last ? `Insert above page ${first}` : `Insert above pages ${first}–${last}`;
  document.getElementById("insertBelowBtn").textContent = first === last ? `Insert below page ${last}` : `Insert below pages ${first}–${last}`;
  insertMenu.classList.toggle("hidden");
});

async function insertWhere(below) {
  insertMenu.classList.add("hidden");
  const [first, last] = insertRange();
  const selected = await open({ multiple: true, filters: PDF_FILTER, title: "Insert pages from" });
  if (!selected) return;
  await insertPagesFrom(Array.isArray(selected) ? selected : [selected], below ? last : first - 1);
}

document.getElementById("insertAboveBtn").addEventListener("click", () => insertWhere(false));
document.getElementById("insertBelowBtn").addEventListener("click", () => insertWhere(true));
document.addEventListener("click", (ev) => {
  if (!insertMenu.classList.contains("hidden") && !insertMenu.contains(ev.target)) insertMenu.classList.add("hidden");
});
document.addEventListener("keydown", (ev) => {
  if (ev.key === "Escape") insertMenu.classList.add("hidden");
});
document.getElementById("deletePageBtn").addEventListener("click", deleteCurrentPage);

// Where dropped PDFs would go in the Pages panel: { at, thumb, after },
// or null when the pointer isn't over the panel. `position` is in
// physical pixels.
function pageDropTarget(position) {
  if (!state.path || !position) return null;
  const dpr = window.devicePixelRatio || 1;
  return pageTargetAt(position.x / dpr, position.y / dpr);
}

// The insertion point in the Pages panel at client point (x, y): `at` =
// how many pages come before it.
function pageTargetAt(x, y) {
  const el = document.elementFromPoint(x, y);
  if (!el || !el.closest("#thumbs")) return null;
  const thumb = el.closest(".thumb");
  if (!thumb) return { at: state.pageCount, thumb: null, after: true };
  const n = Number(thumb.dataset.page);
  const r = thumb.getBoundingClientRect();
  const after = y > r.top + r.height / 2;
  return { at: after ? n : n - 1, thumb, after };
}

// Drag a thumbnail up or down the Pages panel to move that page.
let thumbDrag = null;

document.getElementById("thumbList").addEventListener("mousedown", (ev) => {
  const item = ev.target.closest(".thumb");
  if (!item || ev.button !== 0 || ev.target.closest(".thumb__rot") || !state.path) return;
  thumbDrag = { from: Number(item.dataset.page), item, x0: ev.clientX, y0: ev.clientY, active: false, target: null };
});

window.addEventListener("mousemove", (ev) => {
  const d = thumbDrag;
  if (!d) return;
  if (!d.active) {
    if (Math.abs(ev.clientY - d.y0) + Math.abs(ev.clientX - d.x0) < 6) return;
    d.active = true;
    d.item.classList.add("dragging");
  }
  ev.preventDefault();
  // Scroll the panel when dragging near its top or bottom.
  const r = thumbsBox.getBoundingClientRect();
  if (ev.clientY < r.top + 50) thumbsBox.scrollTop -= 12;
  else if (ev.clientY > r.bottom - 30) thumbsBox.scrollTop += 12;
  d.target = pageTargetAt(ev.clientX, ev.clientY);
  showPageDropTarget(d.target, true);
});

window.addEventListener("mouseup", async () => {
  const d = thumbDrag;
  if (!d) return;
  thumbDrag = null;
  if (!d.active) return;
  d.item.classList.remove("dragging");
  showPageDropTarget(null);
  // Swallow the click that follows the drag, so it doesn't change page.
  const swallow = (e) => e.stopPropagation();
  window.addEventListener("click", swallow, true);
  setTimeout(() => window.removeEventListener("click", swallow, true), 0);
  if (!d.target) return;
  const to = d.target.at >= d.from ? d.target.at : d.target.at + 1;
  if (to === d.from) return;
  setProgress(30, `Moving page ${d.from}…`);
  try {
    await invoke("move_page", { path: state.path, from: d.from, to });
    await afterPagesChanged(state.pageCount, to);
  } catch (err) {
    alert(`Could not move the page:\n${err}`);
  } finally {
    hideProgress();
  }
});

function showPageDropTarget(target, reorder = false) {
  thumbsBox.classList.toggle("drop-target", !!target && !reorder);
  for (const el of thumbsBox.querySelectorAll(".drop-before, .drop-after")) el.classList.remove("drop-before", "drop-after");
  if (target && target.thumb) target.thumb.classList.add(target.after ? "drop-after" : "drop-before");
}

// --- select tool: move, delete and overprint objects ---
// Click an object to select it (Shift-click adds or removes), drag to
// move the selection, or drag on an empty spot (Ctrl-drag anywhere) to
// select everything inside a box. Edits apply to the current page only.

const sel = { items: [], busy: false, offset: null, nudge: [0, 0], nudgeTimer: null };
let selDrag = null;

function toolsOff(except) {
  if (except !== "zoom" && state.zoomTool) setZoomTool(false);
  if (except !== "recolour" && state.recolourTool) setRecolourTool(false);
  if (except !== "select" && state.selectTool) setSelectTool(false);
  if (except !== "merge" && state.mergeTool) setMergeTool(false);
}

function setSelectTool(on) {
  if (on) toolsOff("select");
  state.selectTool = on;
  document.getElementById("selectToolBtn").classList.toggle("active", on);
  els.viewport.classList.toggle("select-tool", on);
  if (on) {
    setTextMode(false);
    clearInks();
    // Coming back (e.g. after recolouring): the same objects, re-read so
    // the panel shows their new colours.
    const saved = sel.saved;
    sel.saved = null;
    if (saved && saved.path === state.path && saved.page === state.page) {
      sel.items = saved.items;
      refreshSelection();
    }
  } else {
    const keep = sel.items.length ? { path: state.path, page: state.page, items: sel.items } : null;
    clearSelection();
    sel.saved = keep;
    setTextMode(true); // back to the default: text can be selected
  }
  renderSelectPanel();
}

function clearSelection() {
  sel.saved = null;
  clearTimeout(sel.nudgeTimer);
  sel.nudge = [0, 0];
  sel.items = [];
  sel.offset = null;
  setSelResult("");
  renderSelectPanel();
  drawBoxOverlay();
}

const isSelected = (o) => sel.items.some((s) => s.index === o.index);

function setSelResult(text, cls = "") {
  const el = document.getElementById("selResult");
  el.textContent = text;
  el.className = `rc-result ${cls}`;
}

const KIND_NAMES = { path: "shape", text: "text run", image: "image", form: "placed graphic", group: "framed group" };

// A plain name for a colour: "Spot Orange", "Spot Blue"…
function spotNameFor(cmyk) {
  const [c, m, y, k] = cmyk.map((v) => v / 100);
  const r = (1 - c) * (1 - k);
  const g = (1 - m) * (1 - k);
  const b = (1 - y) * (1 - k);
  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  const light = (max + min) / 2;
  const sat = max === min ? 0 : (max - min) / (1 - Math.abs(2 * light - 1));
  if (sat < 0.15 || max - min < 0.08) return light > 0.92 ? "Spot White" : light < 0.15 ? "Spot Black" : "Spot Grey";
  let hue = 0;
  if (max === r) hue = ((g - b) / (max - min) + 6) % 6;
  else if (max === g) hue = (b - r) / (max - min) + 2;
  else hue = (r - g) / (max - min) + 4;
  hue *= 60;
  const names = [
    [15, "Red"], [40, "Orange"], [65, "Yellow"], [90, "Lime"], [160, "Green"],
    [195, "Turquoise"], [255, "Blue"], [285, "Purple"], [330, "Pink"], [360, "Red"],
  ];
  let name = names.find(([upTo]) => hue < upTo)[1];
  if (name === "Orange" && light < 0.35) name = "Brown";
  if (light < 0.25 && name !== "Brown") name = `Dark ${name}`;
  return `Spot ${name}`;
}

function renderSelectPanel() {
  const n = sel.items.length;
  const empty = document.getElementById("selEmpty");
  empty.classList.toggle("hidden", n > 0);
  empty.innerHTML = state.selectTool
    ? "Click a shape, text or image on the page, or drag a box around several."
    : "Turn on the <strong>Select</strong> tool (V) and click a shape, text or image on the page.";
  document.getElementById("selBody").classList.toggle("waiting", !n);
  document.getElementById("selCount").textContent =
    n === 1 ? `1 ${KIND_NAMES[sel.items[0].kind] || "object"} selected` : n ? `${n} objects selected` : "";
  // Spot colour: the first selected colour, with a suggested name that
  // can be typed over (kept until the selection changes).
  const coloured = sel.items.find((o) => o.cmyk);
  const key = sel.items.map((o) => o.index).join(",");
  const nameInput = document.getElementById("selSpotName");
  if (key !== sel.spotKey) {
    sel.spotKey = key;
    sel.spotNameTyped = false;
    nameInput.value = coloured ? spotNameFor(coloured.cmyk) : "";
  }
  syncRecolourFromSelection();
  updateSpotPreview();
  document.getElementById("selSpotBtn").disabled = !coloured;
  const on = sel.items.filter((o) => o.overprint).length;
  const box = document.getElementById("selOverprint");
  box.checked = n > 0 && on === n;
  box.indeterminate = on > 0 && on < n;
  // Overprint can only be ticked for 100% black and spot colours (it can
  // always be turned off again).
  const allowed = n > 0 && sel.items.every((o) => o.overprintable);
  box.disabled = !allowed && on === 0;
  box.parentElement.classList.toggle("disabled", box.disabled);
  document.getElementById("selOverprintNote").textContent = n && !allowed
    ? "Only 100% black or spot colours can overprint. Convert the colour to a spot colour below first."
    : "";
}

// Blue outlines round the selection (moved by `sel.offset` while dragging).
function drawSelectionOverlay(svg) {
  if (!sel.items.length) return;
  const g = document.createElementNS(SVG_NS, "g");
  g.id = "selGroup";
  for (const o of sel.items) {
    const [x, y, w, h] = o.norm;
    const r = document.createElementNS(SVG_NS, "rect");
    r.setAttribute("x", x);
    r.setAttribute("y", y);
    r.setAttribute("width", Math.max(w, 0.002));
    r.setAttribute("height", Math.max(h, 0.002));
    r.setAttribute("class", "sel-box");
    g.appendChild(r);
  }
  if (sel.offset) g.setAttribute("transform", `translate(${sel.offset[0]} ${sel.offset[1]})`);
  svg.appendChild(g);
}

function setSelOffset(dx, dy) {
  sel.offset = dx || dy ? [dx, dy] : null;
  const g = document.getElementById("selGroup");
  if (g) g.setAttribute("transform", sel.offset ? `translate(${dx} ${dy})` : "");
}

// Runs a move / delete / overprint edit on the selection and redraws.
async function runObjectEdit(args, label) {
  if (!sel.items.length || sel.busy) return;
  sel.busy = true;
  setSelResult("");
  setProgress(30, label);
  try {
    const res = await invoke("objects_edit", {
      path: state.path,
      page: state.page,
      indices: sel.items.map((o) => o.index),
      action: args.action,
      dx: args.dx ?? null,
      dy: args.dy ?? null,
      on: args.on ?? null,
      name: args.name ?? null,
      cmyk: args.cmyk ?? null,
      fill: args.fill ?? null,
      stroke: args.stroke ?? null,
    });
    sel.items = res.objects;
    sel.offset = null;
    // The edit is saved: let the next move or delete start straight away,
    // without waiting for the page to finish redrawing (slow in the
    // simulated overprint view).
    sel.busy = false;
    renderSelectPanel();
    drawBoxOverlay();
    if (res.changed) setEdited(true);
    // The normal view can't show overprint (only the simulated view mixes
    // inks), so switch simulation on to show the result.
    if (args.action === "overprint" && args.on && res.changed && !state.simulateOverprint) {
      els.overprintToggle.checked = true;
      state.simulateOverprint = true;
      setSelResult("✓ Overprint on. Simulate overprinting is now switched on so you can see it.", "ok");
    }
    if (args.action === "colour" && res.changed) setSelResult(`✓ Recoloured ${res.changed} object${res.changed === 1 ? "" : "s"}`, "ok");
    if (args.action === "spot" && res.changed) setSelResult(`✓ Now in spot colour "${args.name}"`, "ok");
    if (args.action === "delete") setSelResult(`✓ Deleted ${res.changed} object${res.changed === 1 ? "" : "s"}`, "ok");
    if (res.skipped && args.action === "overprint") {
      setSelResult(`${res.skipped === 1 ? "One object isn't" : `${res.skipped} objects aren't`} 100% black or a spot colour, so ${res.skipped === 1 ? "it was" : "they were"} left as they are. Convert to a spot colour first, then set overprint.`, "err");
    } else if (res.skipped && args.action === "spot") {
      setSelResult(`${res.skipped} object${res.skipped === 1 ? "" : "s"} couldn't be changed: placed graphics are left as they are.`, "err");
    } else if (res.skipped) {
      setSelResult(`${res.skipped} object${res.skipped === 1 ? "" : "s"} couldn't be moved: a shape that also clips other artwork has to stay put.`, "err");
    }
    if (recolour.picked) resetRecolour(); // its object numbers may have changed
    if (args.action === "spot") updateSpotButton();
    state.separationNames = [];
    state.activeSeparations = new Set();
    loadSpotSwatches();
    loadSeparationsList();
    updateRasterDpi();
    setProgress(60, "Redrawing…");
    await showPage({ reloadDoc: true });
  } catch (err) {
    sel.offset = null;
    setSelResult(String(err), "err");
  } finally {
    sel.busy = false;
    hideProgress();
    renderSelectPanel();
    drawBoxOverlay();
  }
}

function moveSelection(dx, dy) {
  if (!dx && !dy) {
    setSelOffset(0, 0);
    return Promise.resolve();
  }
  return runObjectEdit({ action: "move", dx, dy }, "Moving…");
}

const deleteSelection = () => runObjectEdit({ action: "delete" }, "Deleting…");

// Re-reads the selection's colours and outlines after another tool (such
// as Recolour) changed the page, so e.g. Overprint knows the new colour.
async function refreshSelection() {
  if (!sel.items.length || !state.path) return;
  try {
    sel.items = await invoke("objects_info", { path: state.path, page: state.page, indices: sel.items.map((o) => o.index) });
  } catch (err) {
    console.error(err);
    sel.items = [];
  }
  sel.spotKey = null; // suggest a name for the new colour
  renderSelectPanel();
  drawBoxOverlay();
}

// Draws the shared marquee between two client points.
function drawMarqueeBox(x0, y0, x1, y1) {
  marquee.style.left = `${Math.min(x0, x1)}px`;
  marquee.style.top = `${Math.min(y0, y1)}px`;
  marquee.style.width = `${Math.abs(x1 - x0)}px`;
  marquee.style.height = `${Math.abs(y1 - y0)}px`;
  marquee.classList.remove("hidden");
}

// Clicking the page takes the keyboard away from any sidebar field, so
// Delete and the arrow keys act on the selection rather than the field.
els.viewport.addEventListener(
  "mousedown",
  () => {
    const f = document.activeElement;
    if (f && f !== document.body && f.closest && f.closest("input, textarea, select")) f.blur();
  },
  true,
);

els.viewport.addEventListener("mousedown", (ev) => {
  if (!state.selectTool || !view.page || ev.button !== 0 || sel.busy) return;
  ev.preventDefault();
  const [fx, fy] = pointerFraction(ev);
  // Shift or Ctrl add to the selection (a click toggles an object); a
  // Ctrl drag always draws a selection box, even starting on an object.
  const ctrl = ev.ctrlKey || ev.metaKey;
  const d = { x0: ev.clientX, y0: ev.clientY, fx0: fx, fy0: fy, moved: false, shift: ev.shiftKey || ctrl, hit: undefined, marquee: ctrl };
  selDrag = d;
  if (d.marquee) {
    d.pick = invoke("object_pick", { path: state.path, page: state.page, x: fx, y: fy, tol: 4 / view.scale })
      .catch(() => null)
      .then((hit) => (d.hit = hit));
    return;
  }
  // Pressing anywhere inside something already selected grabs the
  // selection, so a drag moves it rather than whatever is underneath
  // (such as the background between letters).
  const wrap = els.imgWrap.getBoundingClientRect();
  const mx = 4 / wrap.width;
  const my = 4 / wrap.height;
  const grabbed = !d.shift && sel.items.find((o) => {
    const [x, y, w, h] = o.norm;
    return fx >= x - mx && fx <= x + w + mx && fy >= y - my && fy <= y + h + my;
  });
  if (grabbed) {
    d.hit = grabbed;
    // A click without moving still picks exactly what's under the pointer.
    d.pick = invoke("object_pick", { path: state.path, page: state.page, x: fx, y: fy, tol: 4 / view.scale }).catch(() => null);
    return;
  }
  d.pick = invoke("object_pick", { path: state.path, page: state.page, x: fx, y: fy, tol: 4 / view.scale })
    .catch((err) => {
      setSelResult(String(err), "err");
      return null;
    })
    .then((hit) => {
      d.hit = hit;
      if (!hit) d.marquee = true;
      else if (!d.shift && !isSelected(hit)) {
        // Pressing on an unselected object selects it, ready to drag.
        sel.items = [hit];
        renderSelectPanel();
        drawBoxOverlay();
      }
      return hit;
    });
});

window.addEventListener("mousemove", (ev) => {
  const d = selDrag;
  if (!d) return;
  if (!d.moved && Math.abs(ev.clientX - d.x0) + Math.abs(ev.clientY - d.y0) > 4) d.moved = true;
  if (!d.moved || d.hit === undefined) return;
  if (d.marquee) drawMarqueeBox(d.x0, d.y0, ev.clientX, ev.clientY);
  else {
    const [fx, fy] = pointerFraction(ev);
    setSelOffset(fx - d.fx0, fy - d.fy0);
  }
});

window.addEventListener("mouseup", async (ev) => {
  const d = selDrag;
  if (!d) return;
  selDrag = null;
  marquee.classList.add("hidden");
  const hit = await d.pick;
  const [fx, fy] = pointerFraction(ev);
  if (!d.moved) {
    // A click: select, add / remove with Shift, or clear on empty space.
    setSelOffset(0, 0);
    if (hit) {
      if (!d.shift) sel.items = [hit];
      else if (isSelected(hit)) sel.items = sel.items.filter((s) => s.index !== hit.index);
      else sel.items.push(hit);
    } else if (!d.shift) sel.items = [];
    setSelResult("");
    renderSelectPanel();
    drawBoxOverlay();
    return;
  }
  if (d.marquee) {
    const rect = [Math.min(d.fx0, fx), Math.min(d.fy0, fy), Math.abs(fx - d.fx0), Math.abs(fy - d.fy0)];
    try {
      const found = await invoke("objects_in_rect", { path: state.path, page: state.page, rect });
      sel.items = d.shift ? [...sel.items, ...found.filter((o) => !isSelected(o))] : found;
      setSelResult(found.length ? "" : "Nothing lies wholly inside that box.");
    } catch (err) {
      setSelResult(String(err), "err");
    }
    renderSelectPanel();
    drawBoxOverlay();
    return;
  }
  if (d.shift && hit && !isSelected(hit)) sel.items.push(hit);
  await moveSelection(fx - d.fx0, fy - d.fy0);
});

document.getElementById("selectToolBtn").addEventListener("click", () => setSelectTool(!state.selectTool));
document.getElementById("selDeleteBtn").addEventListener("click", deleteSelection);
// The colour a spot conversion uses: what's typed in the Recolour panel
// (so a changed colour is picked up straight away), otherwise the
// selection's own colour.
function spotColour() {
  const p = recolour.picked;
  if (p && p.fill) return rcValues("fill");
  if (p && p.stroke) return rcValues("stroke");
  const coloured = sel.items.find((o) => o.cmyk);
  return coloured ? coloured.cmyk : null;
}

// Swatch and suggested name follow that colour, until a name is typed.
function updateSpotPreview() {
  const c = sel.items.length ? spotColour() : null;
  document.getElementById("selSpotSwatch").style.background = c ? cmykToCss(c) : "transparent";
  if (c && !sel.spotNameTyped) document.getElementById("selSpotName").value = spotNameFor(c);
}

document.getElementById("selSpotName").addEventListener("input", () => {
  sel.spotNameTyped = true;
});
for (const input of [...rcInputs("fill"), ...rcInputs("stroke")]) input.addEventListener("input", updateSpotPreview);

document.getElementById("selSpotBtn").addEventListener("click", () => {
  const cmyk = spotColour();
  const name = document.getElementById("selSpotName").value.trim();
  if (!cmyk) return;
  if (!name) {
    setSelResult("Give the spot colour a name.", "err");
    return;
  }
  runObjectEdit({ action: "spot", name, cmyk }, `Converting to ${name}…`);
});
document.getElementById("selClearBtn").addEventListener("click", clearSelection);
document.getElementById("selOverprint").addEventListener("change", (ev) => {
  runObjectEdit({ action: "overprint", on: ev.target.checked }, ev.target.checked ? "Setting overprint…" : "Removing overprint…");
});

const typingIn = (ev) => ev.target.closest && ev.target.closest("input, textarea, select");

window.addEventListener("keydown", (ev) => {
  if (!view.page || ev.ctrlKey || ev.altKey || ev.metaKey || typingIn(ev)) return;
  const key = ev.key.toLowerCase();
  if (key === "v") {
    setSelectTool(!state.selectTool);
  } else if (key === "m") {
    setMergeTool(!state.mergeTool);
  } else if (ev.key === "Delete" || ev.key === "Backspace") {
    // Objects first; with nothing selected, Delete in the Pages panel
    // deletes the page.
    if (state.selectTool && sel.items.length) deleteSelection();
    else if (state.pagesFocused && ev.key === "Delete") deleteCurrentPage();
    else return;
  } else if (state.selectTool && sel.items.length && !sel.busy && ev.key.startsWith("Arrow")) {
    // Nudge 0.5 mm (Shift: 5 mm); quick presses add up into one move.
    const dir = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] }[ev.key];
    if (!dir) return;
    const [w, h] = pageSizePt();
    const step = fromMm(ev.shiftKey ? 5 : 0.5);
    sel.nudge = [sel.nudge[0] + (dir[0] * step) / w, sel.nudge[1] + (dir[1] * step) / h];
    setSelOffset(...sel.nudge);
    clearTimeout(sel.nudgeTimer);
    sel.nudgeTimer = setTimeout(() => {
      const [dx, dy] = sel.nudge;
      sel.nudge = [0, 0];
      moveSelection(dx, dy);
    }, 400);
  } else return;
  ev.preventDefault();
});

// --- mail merge ---
// Drag a box on the page, choose a CSV file, and the page is replaced by
// one copy per CSV row with that row's text set in the box (font
// embedded, CMYK colour). The + adds more boxes, each showing its own
// column with its own font, size and colour.

const merge = { page: null, fields: [], active: -1, rows: null, csvName: "" };
let mergeDrag = null;

function clearMergeBoxes() {
  merge.fields = [];
  merge.active = -1;
  merge.page = null;
}

const activeField = () => merge.fields[merge.active] || null;

function setMergeTool(on) {
  if (on) toolsOff("merge");
  state.mergeTool = on;
  document.getElementById("mergeToolBtn").classList.toggle("active", on);
  els.viewport.classList.toggle("merge-tool", on);
  if (on) {
    setTextMode(false);
    clearInks();
  } else {
    setTextMode(true); // back to the default: text can be selected
  }
  document.getElementById("mergeBar").classList.toggle("hidden", !on);
  if (!on && merge.dropper) setMergeDropper(false);
  positionMergeBar();
  renderMergePanel();
  // Point out what to do first: draw a box.
  if (on && !mergeEl("mergeEmpty").classList.contains("hidden")) sparkle(mergeEl("mergeEmptyWrap"));
}

// The options bar floats over the top left of the page view, or wherever
// it was dragged to by its hand (kept inside the window).
const mergeBarPos = { moved: null };

function positionMergeBar() {
  const bar = document.getElementById("mergeBar");
  if (bar.classList.contains("hidden")) return;
  const main = bar.parentElement.getBoundingClientRect();
  const port = els.viewport.getBoundingClientRect();
  let [x, y] = mergeBarPos.moved || [port.left - main.left + 12 + RULER, port.top - main.top + 10 + RULER];
  x = Math.min(Math.max(0, x), Math.max(0, main.width - bar.offsetWidth));
  y = Math.min(Math.max(0, y), Math.max(0, main.height - 40));
  bar.style.left = `${x}px`;
  bar.style.top = `${y}px`;
  bar.style.maxWidth = `${Math.max(240, port.width - 24)}px`;
}
new ResizeObserver(positionMergeBar).observe(els.viewport);

{
  let drag = null;
  document.getElementById("mergeHandle").addEventListener("mousedown", (ev) => {
    if (ev.button !== 0 || ev.target.closest("#mergeCloseBtn")) return;
    ev.preventDefault();
    const bar = document.getElementById("mergeBar");
    drag = { dx: ev.clientX - bar.offsetLeft, dy: ev.clientY - bar.offsetTop };
    bar.classList.add("dragging");
  });
  window.addEventListener("mousemove", (ev) => {
    if (!drag) return;
    mergeBarPos.moved = [ev.clientX - drag.dx, ev.clientY - drag.dy];
    positionMergeBar();
  });
  window.addEventListener("mouseup", () => {
    if (!drag) return;
    drag = null;
    document.getElementById("mergeBar").classList.remove("dragging");
  });
}

const mergeEl = (id) => document.getElementById(id);

// The box settings shown in the bar.
function readMergeInputs() {
  return {
    column: Number(mergeEl("mergeColumn").value) || 0,
    kind: mergeEl("mergeKind").value,
    font: mergeEl("mergeFont").value,
    size: Number.parseFloat(mergeEl("mergeSize").value) || 12,
    cmyk: rcValues("merge"),
    align: mergeEl("mergeAlign").value,
    valign: mergeEl("mergeValign").value,
    shrink: mergeEl("mergeShrink").checked,
    overprint: mergeEl("mergeOverprint").checked,
    vcard: readVcardInputs(),
  };
}

const isBarcode = (f) => !!f.kind && f.kind !== "text";
// The barcode the backend draws: a contact card is a QR code.
const barcodeKind = (f) => (f.kind === "vcard" ? "qr" : f.kind);

// Font, size, alignment and shrink only apply to text; a contact card
// takes several columns instead of one.
function showKindOptions(kind) {
  const barcode = kind !== "text";
  for (const el of document.querySelectorAll("#mergeBar .merge-text-only")) el.classList.toggle("hidden", barcode);
  mergeEl("mergeColumnField").classList.toggle("hidden", kind === "vcard");
  mergeEl("mergeVcard").classList.toggle("hidden", kind !== "vcard");
}

// --- contact cards ---
// A "Contact card" box makes a QR code holding a vCard built from the
// chosen columns, so scanning it offers to save the contact.

const VCARD_PARTS = ["name", "org", "title", "phone", "mobile", "email", "web", "address"];
// Column headings that suggest each part, for a first guess.
const VCARD_GUESS = {
  name: /^(full ?)?name$|^contact$/i,
  org: /company|organi[sz]ation|business|^org$/i,
  title: /title|position|role|job/i,
  phone: /^(tel|phone|telephone|landline|office)/i,
  mobile: /mobile|cell/i,
  email: /e-?mail/i,
  web: /web|url|site/i,
  address: /address/i,
};
const vcardSelects = () => [...document.querySelectorAll("#mergeVcard select")];

function readVcardInputs() {
  const v = {};
  for (const s of vcardSelects()) v[s.dataset.vcard] = s.value === "" ? -1 : Number(s.value);
  return v;
}

function guessVcard() {
  const headings = merge.rows ? merge.rows[0] || [] : [];
  const v = {};
  for (const part of VCARD_PARTS) v[part] = headings.findIndex((h) => VCARD_GUESS[part].test(String(h).trim()));
  return v;
}

// vCard 3.0 text for one row.
function buildVcard(map, row) {
  const val = (part) => {
    const i = map && map[part];
    return i >= 0 ? String(row[i] ?? "").trim() : "";
  };
  const esc = (s) => s.replace(/\\/g, "\\\\").replace(/\n/g, "\\n").replace(/([,;])/g, "\\$1");
  const name = val("name");
  if (!VCARD_PARTS.some((p) => val(p))) return "";
  const words = name.split(/\s+/).filter(Boolean);
  const last = words.length > 1 ? words.pop() : "";
  const lines = ["BEGIN:VCARD", "VERSION:3.0", `N:${esc(last)};${esc(words.join(" "))};;;`, `FN:${esc(name || val("org"))}`];
  if (val("org")) lines.push(`ORG:${esc(val("org"))}`);
  if (val("title")) lines.push(`TITLE:${esc(val("title"))}`);
  if (val("phone")) lines.push(`TEL;TYPE=WORK,VOICE:${val("phone")}`);
  if (val("mobile")) lines.push(`TEL;TYPE=CELL:${val("mobile")}`);
  if (val("email")) lines.push(`EMAIL:${val("email")}`);
  if (val("web")) lines.push(`URL:${val("web")}`);
  if (val("address")) lines.push(`ADR;TYPE=WORK:;;${esc(val("address"))};;;;`);
  lines.push("END:VCARD");
  return lines.join("\n");
}

// What a box shows for one row.
function fieldValue(f, row) {
  if (f.kind === "vcard") return buildVcard(f.vcard, row);
  return row[f.column] ?? "";
}

function showFieldInBar(f) {
  mergeEl("mergeKind").value = f.kind || "text";
  showKindOptions(mergeEl("mergeKind").value);
  for (const s of vcardSelects()) {
    const i = f.vcard ? f.vcard[s.dataset.vcard] : -1;
    s.value = i >= 0 ? String(i) : "";
  }
  mergeEl("mergeFont").value = f.font;
  mergeEl("mergeSize").value = String(f.size);
  mergeEl("mergeAlign").value = f.align;
  mergeEl("mergeValign").value = f.valign;
  mergeEl("mergeShrink").checked = f.shrink;
  mergeEl("mergeOverprint").checked = f.overprint;
  rcInputs("merge").forEach((input, i) => (input.value = String(f.cmyk[i])));
  updateNewSwatches();
}

// A new box takes the current settings and the next column along. Its
// colour is chosen to show up against the page until the user picks one.
function newMergeField() {
  return { ...readMergeInputs(), rect: null, column: merge.fields.length, autoColour: true };
}

// White text on a dark background, black on a light one: reads the inks
// in the middle of the box.
async function autoMergeColour(f) {
  const [x, y, w, h] = f.rect;
  let inks;
  try {
    inks = await invoke("sample_inks", { path: state.path, page: state.page, dpi: SAMPLE_DPI, x: x + w / 2, y: y + h / 2 });
  } catch (err) {
    console.error(err);
    return;
  }
  // The user chose a colour meanwhile, or it's a barcode, which stays
  // black so it scans.
  if (!f.autoColour || isBarcode(f)) return;
  const v = (n) => (inks.find((s) => s.name === n)?.percent || 0) / 100;
  const [c, m, ye, k] = ["Cyan", "Magenta", "Yellow", "Black"].map(v);
  let light = ((1 - c) * 0.3 + (1 - m) * 0.59 + (1 - ye) * 0.11) * (1 - k);
  for (const s of inks) if (!["Cyan", "Magenta", "Yellow", "Black"].includes(s.name)) light *= 1 - 0.7 * (s.percent / 100);
  f.cmyk = light < 0.5 ? [0, 0, 0, 0] : [0, 0, 0, 100];
  if (f === activeField()) showFieldInBar(f);
  renderMergePanel();
}

function activateField(i) {
  merge.active = i;
  const f = activeField();
  if (f) showFieldInBar(f);
  renderMergePanel();
  drawBoxOverlay();
}

function mergeRows() {
  if (!merge.rows) return [];
  return mergeEl("mergeHeader").checked ? merge.rows.slice(1) : merge.rows;
}

// Column names: the heading row when "Skip first row" is on, otherwise
// numbers with the first value as a reminder.
function mergeColumns() {
  const count = Math.max(1, merge.fields.length, ...(merge.rows || []).map((r) => r.length));
  const first = merge.rows ? merge.rows[0] || [] : [];
  const sample = mergeRows()[0] || [];
  return Array.from({ length: count }, (_, i) => {
    if (mergeEl("mergeHeader").checked && first[i]) return first[i];
    return sample[i] ? `Column ${i + 1} (${sample[i]})` : `Column ${i + 1}`;
  });
}

function setMergeResult(text, cls = "") {
  const el = document.getElementById("mergeResult");
  el.textContent = text;
  el.className = `rc-result ${cls}`;
}

function renderMergePanel() {
  const columns = mergeColumns();
  const f = activeField();

  // One chip per box; the active one is highlighted.
  const chips = mergeEl("mergeFields");
  chips.replaceChildren();
  merge.fields.forEach((field, i) => {
    const chip = document.createElement("span");
    chip.className = `merge-chip${i === merge.active ? " active" : ""}${field.rect ? "" : " empty"}`;
    const name = field.kind === "vcard" ? "Contact card" : columns[field.column] || `Column ${field.column + 1}`;
    chip.title = `Box ${i + 1}: ${name}`;
    const label = document.createElement("button");
    label.type = "button";
    label.className = "merge-chip__label";
    label.textContent = `${i + 1} · ${name.length > 14 ? `${name.slice(0, 13)}…` : name}`;
    label.addEventListener("click", () => activateField(i));
    const del = document.createElement("button");
    del.type = "button";
    del.className = "merge-chip__del";
    del.textContent = "×";
    del.title = `Remove box ${i + 1}`;
    del.addEventListener("click", () => {
      merge.fields.splice(i, 1);
      if (!merge.fields.length) clearMergeBoxes();
      activateField(Math.min(merge.active, merge.fields.length - 1));
    });
    chip.append(label, del);
    chips.appendChild(chip);
  });
  mergeEl("mergeAddBtn").disabled = !merge.fields.some((x) => x.rect);

  const hint = mergeEl("mergeEmpty");
  hint.textContent = !merge.fields.length
    ? "Drag a box where the merge should go."
    : f && !f.rect
      ? `Drag a box for box ${merge.active + 1}.`
      : "";
  hint.classList.toggle("hidden", !hint.textContent);
  // A new prompt (e.g. after + or Clear boxes) gets the animated frame;
  // it goes once the box is drawn.
  if (!hint.textContent) stopSparkle(mergeEl("mergeEmptyWrap"));
  else if (hint.textContent !== merge.lastHint && state.mergeTool) sparkle(mergeEl("mergeEmptyWrap"));
  merge.lastHint = hint.textContent;
  mergeEl("mergeOptions").classList.toggle("waiting", !f);

  const select = mergeEl("mergeColumn");
  select.replaceChildren(
    ...columns.map((name, i) => {
      const o = document.createElement("option");
      o.value = String(i);
      o.textContent = name;
      return o;
    }),
  );
  if (f) select.value = String(Math.min(f.column, columns.length - 1));
  // The contact card's column menus, each with a "none" choice.
  for (const s of vcardSelects()) {
    const none = document.createElement("option");
    none.value = "";
    none.textContent = "—";
    s.replaceChildren(
      none,
      ...columns.map((name, i) => {
        const o = document.createElement("option");
        o.value = String(i);
        o.textContent = name;
        return o;
      }),
    );
    const i = f && f.vcard ? f.vcard[s.dataset.vcard] : -1;
    s.value = i >= 0 && i < columns.length ? String(i) : "";
  }

  const rows = mergeRows();
  const info = mergeEl("mergeCsvInfo");
  if (!merge.rows) info.textContent = "";
  else if (!rows.length) info.textContent = `${merge.csvName}: no rows to merge.`;
  else {
    const n = Math.max(...merge.rows.map((r) => r.length));
    info.textContent = `${merge.csvName}: ${rows.length} row${rows.length === 1 ? "" : "s"}, ${n} column${n === 1 ? "" : "s"}`;
  }
  const run = mergeEl("mergeRunBtn");
  run.disabled = !merge.fields.some((x) => x.rect) || !rows.length;
  renderMergeText();
  run.textContent = rows.length ? `Create ${rows.length} page${rows.length === 1 ? "" : "s"}` : "Create pages";
}

// The first row's text shown in each box, in roughly the chosen font,
// size, colour and position, so the merge can be judged before it's run.
const MERGE_CSS_FONTS = {
  sans: ["Helvetica, Arial, sans-serif", "normal"],
  "sans-bold": ["Helvetica, Arial, sans-serif", "bold"],
  serif: ["'Times New Roman', Times, serif", "normal"],
  mono: ["'Courier New', Courier, monospace", "normal"],
};

function renderMergeText() {
  let layer = document.getElementById("mergeTextLayer");
  if (!layer) {
    layer = document.createElement("div");
    layer.id = "mergeTextLayer";
    els.imgWrap.appendChild(layer);
  }
  layer.replaceChildren();
  if (merge.page !== state.page || !view.page) return;
  const columns = mergeColumns();
  const first = mergeRows()[0] || [];
  for (const f of merge.fields) {
    if (!f.rect) continue;
    const [x, y, w, h] = f.rect;
    const box = document.createElement("div");
    box.className = "merge-text";
    box.style.left = `${x * 100}%`;
    box.style.top = `${y * 100}%`;
    box.style.width = `${w * 100}%`;
    box.style.height = `${h * 100}%`;
    if (isBarcode(f)) {
      box.style.justifyContent = "center";
      box.style.alignItems = "center";
      box.appendChild(barcodePreview(f, String(fieldValue(f, first)).trim(), box));
      layer.appendChild(box);
      continue;
    }
    // Drawn as SVG text so the baseline, size and alignment match the merge
    // exactly (same baseline formula as merge_program in pages.rs).
    const value = first[f.column];
    const svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("class", "merge-text__svg");
    const text = document.createElementNS(SVG_NS, "text");
    text.textContent = value || `«${columns[f.column] || `Column ${f.column + 1}`}»`;
    if (!value) text.classList.add("placeholder");
    const opt = [...mergeEl("mergeFont").options].find((o) => o.value === f.font);
    if (opt && opt.dataset.family) {
      text.style.fontFamily = `"${opt.dataset.family}"`;
      text.style.fontWeight = /bold|black|heavy/i.test(opt.dataset.style) ? "bold" : /light|thin/i.test(opt.dataset.style) ? "300" : "normal";
      text.style.fontStyle = /italic|oblique/i.test(opt.dataset.style) ? "italic" : "normal";
    } else {
      const [family, weight] = MERGE_CSS_FONTS[f.font] || MERGE_CSS_FONTS.sans;
      text.style.fontFamily = family;
      text.style.fontWeight = weight;
    }
    text.setAttribute("fill", cmykToCss(f.cmyk));
    text.setAttribute("text-anchor", { left: "start", right: "end" }[f.align] || "middle");
    svg.appendChild(text);
    box.appendChild(svg);
    layer.appendChild(box);
    const bw = box.clientWidth;
    const bh = box.clientHeight;
    let px = f.size * view.scale;
    text.style.fontSize = `${px}px`;
    // Shrink to fit, as the merge will.
    const len = text.getComputedTextLength();
    if (f.shrink && len > bw && bw > 0) {
      px *= bw / len;
      text.style.fontSize = `${px}px`;
    }
    text.setAttribute("x", String({ left: 0, right: bw }[f.align] ?? bw / 2));
    const baseline = { top: 0.72 * px, bottom: bh - 0.22 * px }[f.valign] ?? bh / 2 + 0.35 * px;
    text.setAttribute("y", String(baseline));
  }
}

// Barcode modules from the backend, by type and value, so the preview is
// the exact code the merge will draw.
const barcodeCache = new Map();

// The first row's barcode, filling the box as the merge will (with its
// clear margin). Fetched once per value; the preview redraws when it
// arrives.
function barcodePreview(f, value, box) {
  const label = { qr: "QR code", datamatrix: "Data Matrix", gs1datamatrix: "GS1 Data Matrix", vcard: "Contact card" }[f.kind] || "Barcode";
  const note = (text) => {
    const span = document.createElement("span");
    span.className = "placeholder merge-barcode-note";
    span.textContent = text;
    span.style.fontSize = "12px";
    return span;
  };
  if (!value) return note(`«${label}»`);
  const key = `${f.kind}\n${value}`;
  const m = barcodeCache.get(key);
  if (!m) {
    barcodeCache.set(key, null);
    invoke("barcode_matrix", { kind: barcodeKind(f), text: value })
      .then((res) => barcodeCache.set(key, res))
      .catch((err) => barcodeCache.set(key, { error: String(err) }))
      .finally(renderMergeText);
    return note("…");
  }
  if (m.error) {
    const span = note(m.error);
    span.classList.add("merge-barcode-error");
    return span;
  }
  const svg = document.createElementNS(SVG_NS, "svg");
  const q = m.quiet;
  svg.setAttribute("viewBox", `${-q} ${-q} ${m.width + 2 * q} ${m.height + 2 * q}`);
  svg.setAttribute("preserveAspectRatio", "xMidYMid meet");
  svg.setAttribute("shape-rendering", "crispEdges");
  svg.style.width = "100%";
  svg.style.height = "100%";
  let d = "";
  for (let y = 0; y < m.height; y++) {
    for (let x = 0; x < m.width; x++) {
      if (!m.bits[y * m.width + x]) continue;
      let end = x;
      while (end < m.width && m.bits[y * m.width + end]) end++;
      d += `M${x} ${y}h${end - x}v1h${x - end}z`;
      x = end;
    }
  }
  const path = document.createElementNS(SVG_NS, "path");
  path.setAttribute("d", d);
  path.setAttribute("fill", cmykToCss(f.cmyk));
  svg.appendChild(path);
  return svg;
}

function drawMergeOverlay(svg) {
  renderMergeText();
  if (merge.page !== state.page) return;
  merge.fields.forEach((f, i) => {
    if (!f.rect) return;
    const [x, y, w, h] = f.rect;
    const r = document.createElementNS(SVG_NS, "rect");
    r.setAttribute("x", x);
    r.setAttribute("y", y);
    r.setAttribute("width", w);
    r.setAttribute("height", h);
    r.setAttribute("class", `merge-box${i === merge.active ? " active" : ""}`);
    svg.appendChild(r);
  });
}

// The box under a point (fractions of the page), or -1.
function mergeBoxAt(fx, fy) {
  if (merge.page !== state.page) return -1;
  for (let i = merge.fields.length - 1; i >= 0; i--) {
    const r = merge.fields[i].rect;
    if (r && fx >= r[0] && fx <= r[0] + r[2] && fy >= r[1] && fy <= r[1] + r[3]) return i;
  }
  return -1;
}

// Eyedropper: the next click on the page takes that object's colour (its
// exact CMYK from the PDF, or the inks under the pointer for images).
function setMergeDropper(on) {
  merge.dropper = on;
  mergeEl("mergeDropper").classList.toggle("active", on);
  els.viewport.classList.toggle("merge-dropper", on);
  if (on) setMergeResult("Click a colour on the page to use it for the text.");
}

async function dropperPick(fx, fy) {
  setMergeDropper(false);
  let cmyk = null;
  try {
    const picked = await invoke("recolour_pick", { path: state.path, page: state.page, x: fx, y: fy, tol: 4 / view.scale });
    const c = picked && (picked.fill || picked.stroke);
    if (c) cmyk = c.cmyk;
    if (!cmyk) {
      const inks = await invoke("sample_inks", { path: state.path, page: state.page, dpi: SAMPLE_DPI, x: fx, y: fy });
      const ink = (n) => Math.round(inks.find((s) => s.name === n)?.percent || 0);
      cmyk = [ink("Cyan"), ink("Magenta"), ink("Yellow"), ink("Black")];
    }
  } catch (err) {
    setMergeResult(String(err), "err");
    return;
  }
  rcInputs("merge").forEach((input, i) => (input.value = String(Math.round(cmyk[i]))));
  updateNewSwatches();
  const f = activeField();
  if (f) {
    f.cmyk = rcValues("merge");
    f.autoColour = false;
  }
  setMergeResult(`Colour set to C${cmyk[0]} M${cmyk[1]} Y${cmyk[2]} K${cmyk[3]}`, "ok");
  renderMergePanel();
}

// Press on a box to drag it somewhere else; press anywhere else to draw.
els.viewport.addEventListener("mousedown", (ev) => {
  if (!state.mergeTool || !view.page || ev.button !== 0) return;
  ev.preventDefault();
  const f0 = pointerFraction(ev);
  if (merge.dropper) {
    dropperPick(...f0);
    return;
  }
  const hit = mergeBoxAt(...f0);
  if (hit >= 0) {
    if (hit !== merge.active) activateField(hit);
    mergeDrag = { move: hit, f0, from: [...merge.fields[hit].rect] };
    return;
  }
  mergeDrag = { x0: ev.clientX, y0: ev.clientY, f0 };
});

window.addEventListener("mousemove", (ev) => {
  const d = mergeDrag;
  if (state.mergeTool && !d && view.page) {
    // A move cursor over a box says it can be dragged.
    els.viewport.classList.toggle("merge-over-box", mergeBoxAt(...pointerFraction(ev)) >= 0);
  }
  if (!d) return;
  if (d.move === undefined) {
    drawMarqueeBox(d.x0, d.y0, ev.clientX, ev.clientY);
    return;
  }
  const [fx, fy] = pointerFraction(ev);
  const [x, y, w, h] = d.from;
  const f = merge.fields[d.move];
  f.rect = [Math.min(1 - w, Math.max(0, x + fx - d.f0[0])), Math.min(1 - h, Math.max(0, y + fy - d.f0[1])), w, h];
  drawBoxOverlay();
});

window.addEventListener("mouseup", (ev) => {
  const d = mergeDrag;
  if (!d) return;
  mergeDrag = null;
  if (d.move !== undefined) return; // a box was moved (or just picked)
  marquee.classList.add("hidden");
  const clip = (v) => Math.min(1, Math.max(0, v));
  const [ax, ay] = d.f0.map(clip);
  const [bx, by] = pointerFraction(ev).map(clip);
  const rect = [Math.min(ax, bx), Math.min(ay, by), Math.abs(bx - ax), Math.abs(by - ay)];
  if (rect[2] < 0.01 || rect[3] < 0.005) {
    // A click: pick the box under the pointer.
    const hit = merge.page === state.page
      ? merge.fields.findIndex((f) => f.rect && ax >= f.rect[0] && ax <= f.rect[0] + f.rect[2] && ay >= f.rect[1] && ay <= f.rect[1] + f.rect[3])
      : -1;
    if (hit >= 0) activateField(hit);
    else setMergeResult("Drag out a box on the page for the merge.", "err");
    return;
  }
  // All the boxes go on one page: drawing on another page starts again.
  if (merge.page !== null && merge.page !== state.page) clearMergeBoxes();
  let f = activeField();
  if (!f) {
    f = newMergeField();
    merge.fields.push(f);
    merge.active = merge.fields.length - 1;
  }
  const firstDraw = !f.rect;
  f.rect = rect;
  merge.page = state.page;
  if (firstDraw && f.autoColour) autoMergeColour(f);
  setMergeResult("");
  renderMergePanel();
  drawBoxOverlay();
});

async function chooseMergeCsv() {
  const selected = await open({
    multiple: false,
    filters: [{ name: "CSV or spreadsheet", extensions: ["csv", "CSV", "txt", "xlsx", "XLSX", "xls", "XLS", "xlsm", "ods"] }],
  });
  if (!selected) return;
  const path = Array.isArray(selected) ? selected[0] : selected;
  await openMergeData(path);
}

// Reads a CSV or spreadsheet (at `sheet`, if it has one by that name).
async function openMergeData(path, sheet = null) {
  // A spreadsheet with more than one sheet gets a Sheet menu.
  let sheets = [];
  try {
    sheets = await invoke("merge_sheets", { path });
  } catch (err) {
    setMergeResult(String(err), "err");
  }
  merge.path = path;
  const select = mergeEl("mergeSheet");
  select.replaceChildren(
    ...sheets.map((name) => {
      const o = document.createElement("option");
      o.value = name;
      o.textContent = name;
      return o;
    }),
  );
  mergeEl("mergeSheetRow").classList.toggle("hidden", sheets.length < 2);
  const pick = sheet && sheets.includes(sheet) ? sheet : (sheets[0] ?? null);
  if (pick) select.value = pick;
  await loadMergeRows(pick);
}

// The boxes not yet made into pages, with the data file, sheet and
// "Skip first row", as saved inside the PDF by Save PDF as. Null when
// there are no boxes.
function currentMergeSetup() {
  const placed = merge.fields.filter((f) => f.rect);
  if (!placed.length || !merge.page) return null;
  const sheetSelect = mergeEl("mergeSheet");
  return JSON.stringify({
    version: 1,
    page: merge.page,
    header: mergeEl("mergeHeader").checked,
    file: merge.path || null,
    sheet: merge.path && sheetSelect.options.length ? sheetSelect.value : null,
    fields: placed.map((f) => ({
      rect: f.rect,
      column: f.column,
      kind: f.kind || "text",
      font: f.font,
      size: f.size,
      cmyk: f.cmyk,
      align: f.align,
      valign: f.valign,
      shrink: f.shrink,
      overprint: f.overprint,
      vcard: f.vcard,
    })),
  });
}

// Brings back saved boxes and reopens their data file, if it's still there.
async function restoreMergeSetup(json) {
  let setup;
  try {
    setup = JSON.parse(json);
  } catch {
    return;
  }
  if (!setup || !Array.isArray(setup.fields) || !setup.fields.length) return;
  if (!(setup.page >= 1 && setup.page <= state.pageCount)) return;
  merge.fields = setup.fields.map((f) => ({ ...f, autoColour: false }));
  merge.page = setup.page;
  merge.active = 0;
  mergeEl("mergeHeader").checked = !!setup.header;
  showFieldInBar(merge.fields[0]);
  setMergeTool(true);
  if (setup.file) {
    await openMergeData(setup.file, setup.sheet);
    if (!merge.rows) {
      setMergeResult(`Mail merge boxes restored. Couldn't open ${baseName(setup.file)}: choose the data file again.`, "err");
    } else {
      setMergeResult("Mail merge boxes restored from this PDF.", "ok");
    }
  } else {
    setMergeResult("Mail merge boxes restored from this PDF. Choose a CSV or Excel file.", "ok");
  }
  renderMergePanel();
  drawBoxOverlay();
}

async function loadMergeRows(sheet) {
  const path = merge.path;
  try {
    merge.rows = await invoke("merge_read_csv", { path, sheet });
    merge.csvName = sheet && mergeEl("mergeSheet").options.length > 1 ? `${baseName(path)} › ${sheet}` : baseName(path);
    setMergeResult("");
  } catch (err) {
    merge.rows = null;
    setMergeResult(String(err), "err");
  }
  renderMergePanel();
}

// What a merge would do: the page, each row's values (one per box, rows
// that would be blank left out) and the boxes' settings. Null if there's
// nothing to merge yet.
function mergeJob() {
  const placed = merge.fields.filter((f) => f.rect);
  const page = merge.page;
  const rows = mergeRows()
    .map((r) => placed.map((f) => fieldValue(f, r)))
    .filter((r) => r.some((v) => v));
  if (!placed.length || !rows.length || !state.path || !page) return null;
  const spec = {
    fields: placed.map((f) => ({
      kind: barcodeKind(f) || "text",
      rect: f.rect,
      font: f.font,
      size: f.size,
      cmyk: f.cmyk,
      align: f.align,
      valign: f.valign,
      shrink: f.shrink,
      overprint: f.overprint,
    })),
  };
  return { page, rows, spec };
}

async function runMerge() {
  const job = mergeJob();
  if (!job) return;
  const { page, rows, spec } = job;
  mergeEl("mergeRunBtn").disabled = true;
  setMergeResult("");
  setProgress(30, `Making ${rows.length} page${rows.length === 1 ? "" : "s"}…`);
  try {
    const made = await invoke("mail_merge", { path: state.path, page, rows, spec });
    await afterPagesChanged(state.pageCount - 1 + made, page);
    setMergeResult(`✓ Made ${made} page${made === 1 ? "" : "s"} (${made === 1 ? `page ${page}` : `pages ${page}–${page + made - 1}`})`, "ok");
  } catch (err) {
    setMergeResult(String(err), "err");
  } finally {
    hideProgress();
    renderMergePanel();
    drawBoxOverlay();
  }
}

// Installed fonts join the built-in ones in the font menu.
async function loadSystemFonts() {
  let fonts = [];
  try {
    fonts = await invoke("system_fonts");
  } catch (err) {
    console.error(err);
    return;
  }
  if (!fonts.length) return;
  const group = document.createElement("optgroup");
  group.label = "Installed fonts";
  for (const f of fonts) {
    const o = document.createElement("option");
    o.value = `sys:${f.path}`;
    o.textContent = /^(regular|normal|book|roman)$/i.test(f.style) ? f.family : `${f.family} ${f.style}`;
    o.title = f.ps_name;
    o.dataset.family = f.family;
    o.dataset.style = f.style;
    group.appendChild(o);
  }
  mergeEl("mergeFont").appendChild(group);
}

function initMerge() {
  // Text colour starts as 100% black.
  const k = rcInputs("merge")[3];
  if (k) k.value = "100";
  updateNewSwatches();
  mergeEl("mergeToolBtn").addEventListener("click", () => setMergeTool(!state.mergeTool));
  mergeEl("mergeCloseBtn").addEventListener("click", () => setMergeTool(false));
  mergeEl("mergeDropper").addEventListener("click", () => setMergeDropper(!merge.dropper));
  mergeEl("mergeCsvBtn").addEventListener("click", chooseMergeCsv);
  mergeEl("mergeHeader").addEventListener("change", renderMergePanel);
  mergeEl("mergeSheet").addEventListener("change", () => loadMergeRows(mergeEl("mergeSheet").value));
  mergeEl("mergeRunBtn").addEventListener("click", runMerge);
  loadSystemFonts();
  mergeEl("mergeAddBtn").addEventListener("click", () => {
    merge.fields.push(newMergeField());
    activateField(merge.fields.length - 1);
  });
  mergeEl("mergeClearBtn").addEventListener("click", () => {
    clearMergeBoxes();
    setMergeResult("");
    renderMergePanel();
    drawBoxOverlay();
  });
  // Changing a setting changes the selected box.
  const save = () => {
    const f = activeField();
    if (!f) return;
    Object.assign(f, readMergeInputs());
    renderMergePanel();
  };
  mergeEl("mergeKind").addEventListener("change", () => {
    const kind = mergeEl("mergeKind").value;
    showKindOptions(kind);
    // Barcodes print black, so they scan.
    if (kind !== "text") {
      rcInputs("merge").forEach((input, i) => (input.value = i === 3 ? "100" : "0"));
      updateNewSwatches();
      if (activeField()) activeField().autoColour = false;
    }
    // A new contact card starts from columns whose headings fit.
    if (kind === "vcard" && Object.values(readVcardInputs()).every((i) => i < 0)) {
      const guess = guessVcard();
      for (const s of vcardSelects()) s.value = guess[s.dataset.vcard] >= 0 ? String(guess[s.dataset.vcard]) : "";
    }
  });
  for (const id of ["mergeColumn", "mergeKind", "mergeFont", "mergeSize", "mergeAlign", "mergeValign", "mergeShrink", "mergeOverprint"]) {
    mergeEl(id).addEventListener("change", save);
    mergeEl(id).addEventListener("input", save);
  }
  for (const s of vcardSelects()) s.addEventListener("change", save);
  for (const input of rcInputs("merge")) {
    input.addEventListener("input", () => {
      if (activeField()) activeField().autoColour = false;
      save();
    });
  }
  renderMergePanel();
}

// --- rulers and guides ---
// Rulers in mm run along the top and left of the page view, measured from
// the page's top left corner as shown. Drag from a ruler onto the page to
// add a guide; drag a guide back onto a ruler to remove it. Guides belong
// to the tab and show on every page. The corner button clears them.

const RULER = 18; // px
const rulerTop = document.getElementById("rulerTop");
const rulerLeft = document.getElementById("rulerLeft");
const rulerCorner = document.getElementById("rulerCorner");
const guideLabel = document.getElementById("guideLabel");

function positionRulers() {
  const main = rulerTop.parentElement.getBoundingClientRect();
  const port = els.viewport.getBoundingClientRect();
  const x = port.left - main.left;
  const y = port.top - main.top;
  const w = els.viewport.clientWidth;
  const h = els.viewport.clientHeight;
  Object.assign(rulerTop.style, { left: `${x + RULER}px`, top: `${y}px`, width: `${Math.max(0, w - RULER)}px`, height: `${RULER}px` });
  Object.assign(rulerLeft.style, { left: `${x}px`, top: `${y + RULER}px`, width: `${RULER}px`, height: `${Math.max(0, h - RULER)}px` });
  Object.assign(rulerCorner.style, { left: `${x}px`, top: `${y}px` });
}

const pxPerMm = () => view.scale / MM_PER_PT;

function drawRuler(canvas, horizontal) {
  const r = canvas.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  const len = horizontal ? r.width : r.height;
  if (len <= 0) return;
  canvas.width = Math.round(r.width * dpr);
  canvas.height = Math.round(r.height * dpr);
  const ctx = canvas.getContext("2d");
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.fillStyle = "#1b1c22";
  ctx.fillRect(0, 0, r.width, r.height);
  const wrap = els.imgWrap.getBoundingClientRect();
  const ppm = pxPerMm();
  const [wPt, hPt] = pageSizePt();
  const pageMm = toMm(horizontal ? wPt : hPt);
  const origin = horizontal ? wrap.left - r.left : wrap.top - r.top; // px of 0 mm
  // The page's extent is a shade lighter.
  ctx.fillStyle = "#23242c";
  if (horizontal) ctx.fillRect(origin, 0, pageMm * ppm, r.height);
  else ctx.fillRect(0, origin, r.width, pageMm * ppm);
  // Labelled steps at least 50 px apart, with smaller ticks between.
  const major = [1, 2, 5, 10, 20, 50, 100, 200, 500, 1000, 2000].find((s) => s * ppm >= 50) || 5000;
  const minor = major / (String(major)[0] === "2" ? 4 : 5);
  const mm0 = -origin / ppm;
  const mm1 = mm0 + len / ppm;
  ctx.strokeStyle = "#6a6b76";
  ctx.fillStyle = "#a5a6b0";
  ctx.font = "9px Poppins, sans-serif";
  ctx.lineWidth = 1;
  ctx.beginPath();
  for (let i = Math.floor(mm0 / minor); i * minor <= mm1; i++) {
    const v = i * minor;
    const p = Math.round(origin + v * ppm) + 0.5;
    const isMajor = Math.abs(v / major - Math.round(v / major)) < 1e-6;
    const t = isMajor ? RULER : RULER * 0.3;
    if (horizontal) {
      ctx.moveTo(p, RULER);
      ctx.lineTo(p, RULER - t);
    } else {
      ctx.moveTo(RULER, p);
      ctx.lineTo(RULER - t, p);
    }
    if (isMajor) {
      const label = String(Math.round(v));
      if (horizontal) ctx.fillText(label, p + 3, 9);
      else {
        ctx.save();
        ctx.translate(9, p + 3);
        ctx.rotate(-Math.PI / 2);
        ctx.textAlign = "right";
        ctx.fillText(label, 0, 0);
        ctx.restore();
      }
    }
  }
  ctx.stroke();
  // Edge line against the page view.
  ctx.strokeStyle = "#2a2b33";
  ctx.beginPath();
  if (horizontal) {
    ctx.moveTo(0, r.height - 0.5);
    ctx.lineTo(r.width, r.height - 0.5);
  } else {
    ctx.moveTo(r.width - 0.5, 0);
    ctx.lineTo(r.width - 0.5, r.height);
  }
  ctx.stroke();
}

function drawRulers() {
  const show = !!view.page;
  for (const el of [rulerTop, rulerLeft, rulerCorner]) el.classList.toggle("hidden", !show);
  if (!show) return;
  positionRulers();
  drawRuler(rulerTop, true);
  drawRuler(rulerLeft, false);
}

let guideLayer = null;

function renderGuides() {
  if (!guideLayer) {
    guideLayer = document.createElement("div");
    guideLayer.id = "guideLayer";
    els.imgWrap.appendChild(guideLayer);
  }
  guideLayer.replaceChildren();
  if (!view.page || !state.guides) return;
  const [wPt, hPt] = pageSizePt();
  const size = { h: toMm(hPt), v: toMm(wPt) };
  for (const g of state.guides) {
    const el = document.createElement("div");
    el.className = `guide guide--${g.axis}`;
    el.style[g.axis === "h" ? "top" : "left"] = `${(g.mm / size[g.axis]) * 100}%`;
    el.title = `${g.mm.toFixed(1)} mm from the ${g.axis === "h" ? "top" : "left"} · drag onto a ruler to remove`;
    el.addEventListener("mousedown", (ev) => startGuideDrag(g, ev));
    guideLayer.appendChild(el);
  }
}

let guideDrag = null;

function startGuideDrag(guide, ev) {
  if (ev.button !== 0 || !state.guides) return;
  ev.preventDefault();
  ev.stopPropagation(); // not a click for the page tools
  guideDrag = { guide };
  moveGuide(ev);
}

function moveGuide(ev) {
  const g = guideDrag.guide;
  const wrap = els.imgWrap.getBoundingClientRect();
  const raw = g.axis === "h" ? (ev.clientY - wrap.top) / pxPerMm() : (ev.clientX - wrap.left) / pxPerMm();
  g.mm = Math.round(raw * 10) / 10;
  renderGuides();
  guideLabel.textContent = `${g.mm.toFixed(1)} mm`;
  guideLabel.style.left = `${ev.clientX + 14}px`;
  guideLabel.style.top = `${ev.clientY + 12}px`;
  guideLabel.classList.remove("hidden");
}

for (const [ruler, axis] of [[rulerTop, "h"], [rulerLeft, "v"]]) {
  ruler.addEventListener("mousedown", (ev) => {
    if (ev.button !== 0 || !view.page || !state.guides) return;
    const guide = { axis, mm: 0 };
    state.guides.push(guide);
    startGuideDrag(guide, ev);
  });
}

window.addEventListener("mousemove", (ev) => {
  if (guideDrag) moveGuide(ev);
});

window.addEventListener("mouseup", (ev) => {
  if (!guideDrag) return;
  const g = guideDrag.guide;
  guideDrag = null;
  guideLabel.classList.add("hidden");
  // Dropped back on a ruler, or off the page view: remove it.
  const port = els.viewport.getBoundingClientRect();
  const inside = ev.clientX > port.left + RULER && ev.clientY > port.top + RULER && ev.clientX < port.right && ev.clientY < port.bottom;
  if (!inside) state.guides.splice(state.guides.indexOf(g), 1);
  renderGuides();
});

rulerCorner.addEventListener("click", () => {
  if (!state.guides) return;
  state.guides.length = 0;
  renderGuides();
});

els.viewport.addEventListener("scroll", drawRulers);
new ResizeObserver(drawRulers).observe(els.viewport);

// --- opening files from outside: double-click / "Open with" / drag and drop ---

// Files the operating system handed to the app (at launch, or later while
// it's running) wait in a queue in the app; take them and open the first.
async function openPendingFiles() {
  let paths = [];
  try {
    paths = await invoke("take_pending_open");
  } catch (err) {
    console.error(err);
  }
  for (const p of paths) await loadPdf(p);
}

window.__TAURI__.event.listen("open-files", () => openPendingFiles());
openPendingFiles(); // a PDF double-clicked to launch the app

// Drag PDFs onto the window to open each in a tab, or onto the Pages
// panel to insert their pages where the gold line shows.
const dropHint = document.getElementById("dropHint");
window.__TAURI__.webview.getCurrentWebview().onDragDropEvent(async (ev) => {
  const { type, paths, position } = ev.payload;
  if (type === "enter" || type === "over") {
    const target = pageDropTarget(position);
    showPageDropTarget(target);
    dropHint.classList.toggle("hidden", !!target);
  } else if (type === "leave") {
    showPageDropTarget(null);
    dropHint.classList.add("hidden");
  } else if (type === "drop") {
    const target = pageDropTarget(position);
    showPageDropTarget(null);
    dropHint.classList.add("hidden");
    const pdfs = (paths || []).filter((p) => p.toLowerCase().endsWith(".pdf"));
    if (!pdfs.length) {
      if (paths && paths.length) alert("That isn't a PDF. Drop a .pdf file.");
      return;
    }
    if (target) await insertPagesFrom(pdfs, target.at);
    else for (const p of pdfs) await loadPdf(p);
  }
});

// --- tutorial link & footer (same as RapidCulling) ---

// The tutorial video's link ("New user? Watch this first!") lives in
// links.json in the GitHub repo, read at start-up, so it can be set or
// changed after a release without a new one (same as RapidRetouch). The last
// link read is remembered for offline starts; before any has been read, the
// link below is used. A new link brings the button back even if the old one
// was dismissed.
const LINKS_URL = "https://raw.githubusercontent.com/crispin81/Mangoprint-Prepress-PDF/main/links.json";
const LINKS_TIMEOUT_MS = 5000;
const TUTORIAL_DEFAULT_URL = "https://youtu.be/T5qqebtV9_g";
const TUTORIAL_URL_KEY = "mangoprint.tutorialUrl";
const TUTORIAL_DISMISSED_KEY = "mangoprint.tutorialDismissed"; // holds the dismissed link
const COFFEE_URL = "https://buymeacoffee.com/chriscorkphotography";
const SITE_URL = "https://mangoprint.co.uk";

function openExternal(url) {
  if (url) window.__TAURI__.opener.openUrl(url).catch((err) => console.error(err));
}

{
  const link = document.getElementById("videoLink");
  const store = {
    get: (k) => {
      try {
        return localStorage.getItem(k);
      } catch {
        return null; // localStorage unavailable
      }
    },
    set: (k, v) => {
      try {
        localStorage.setItem(k, v);
      } catch {
        // Just won't persist across restarts.
      }
    },
  };
  let tutorialUrl = store.get(TUTORIAL_URL_KEY) || TUTORIAL_DEFAULT_URL;
  const showTutorial = () => {
    const dismissed = store.get(TUTORIAL_DISMISSED_KEY);
    // "1" is how earlier versions recorded a dismissal (of the first video).
    const isDismissed = dismissed === tutorialUrl || (dismissed === "1" && tutorialUrl === TUTORIAL_DEFAULT_URL);
    link.classList.toggle("hidden", !tutorialUrl || isDismissed);
  };
  showTutorial();
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), LINKS_TIMEOUT_MS);
  fetch(LINKS_URL, { cache: "no-store", signal: ctrl.signal })
    .then((res) => (res.ok ? res.json() : null))
    .then((links) => {
      const url = links && typeof links.tutorial_video === "string" ? links.tutorial_video.trim() : null;
      if (url && /^https:\/\//.test(url)) {
        tutorialUrl = url;
        store.set(TUTORIAL_URL_KEY, url);
        showTutorial();
      }
    })
    .catch(() => {}) // offline: keep the remembered link
    .finally(() => clearTimeout(timer));
  document.getElementById("videoLinkCta").addEventListener("click", (ev) => {
    ev.preventDefault();
    openExternal(tutorialUrl);
  });
  document.getElementById("videoLinkClose").addEventListener("click", () => {
    link.classList.add("hidden");
    store.set(TUTORIAL_DISMISSED_KEY, tutorialUrl);
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

// With no tool on, hovering text gives the text cursor and text can be
// selected. The Select text button turns any tool off to get back to that.
document.getElementById("textModeBtn").addEventListener("click", () => {
  toolsOff();
  setTextMode(true);
});
els.imgWrap.classList.toggle("text-mode", state.textMode);
window.addEventListener("keydown", (ev) => {
  if (ev.key !== "Escape") return;
  if (!closeDialog.classList.contains("hidden")) return;
  if (state.zoomTool) setZoomTool(false);
  else if (state.selectTool) {
    if (sel.items.length) clearSelection();
    else setSelectTool(false);
  } else if (state.mergeTool) {
    if (merge.dropper) setMergeDropper(false);
    else setMergeTool(false);
  }
  else if (state.recolourTool) setRecolourTool(false);
});

document.getElementById("scopeAll").addEventListener("click", () => {
  setApplyScope(true);
  recolourFind();
});
document.getElementById("scopePage").addEventListener("click", () => {
  setApplyScope(false);
  recolourFind();
});

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

function setZoomTool(on) {
  state.zoomTool = on;
  document.getElementById("zoomToolBtn").classList.toggle("active", on);
  els.viewport.classList.toggle("zoom-tool", on);
  els.imgWrap.classList.toggle("zoom-tool", on);
  if (on) {
    toolsOff("zoom");
    setTextMode(false);
    clearInks();
  } else {
    setTextMode(true); // back to the default: text can be selected
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

// --- print ---
// Every page is drawn (as the crop box shows it, with edits) into a
// print-only area, then the system print dialog opens.

let printing = false;
const printArea = document.getElementById("printArea");

function clearPrintArea() {
  for (const img of printArea.querySelectorAll("img")) URL.revokeObjectURL(img.src);
  printArea.replaceChildren();
}
window.addEventListener("afterprint", clearPrintArea);

async function printPdf() {
  const doc = view.doc;
  if (!doc || printing) return;
  printing = true;
  clearPrintArea();
  // Big documents print at a lower resolution to keep memory in check.
  const dpi = doc.numPages > 40 ? 150 : 250;
  try {
    for (let n = 1; n <= doc.numPages; n++) {
      setProgress(Math.round(((n - 1) / doc.numPages) * 90), `Preparing page ${n} of ${doc.numPages} to print…`);
      const page = await doc.getPage(n);
      // The current page shows its whole MediaBox on screen; print the crop.
      const shown = page._pageInfo && page._cropView ? page._pageInfo.view : null;
      if (shown) page._pageInfo.view = page._cropView;
      try {
        const base = page.getViewport({ scale: 1 });
        if (n === 1) {
          document.getElementById("printPageStyle").textContent = `@page { size: ${base.width}pt ${base.height}pt; margin: 0; }`;
        }
        const vp = page.getViewport({ scale: dpi / 72 });
        const canvas = document.createElement("canvas");
        canvas.width = Math.ceil(vp.width);
        canvas.height = Math.ceil(vp.height);
        await page.render({ canvasContext: canvas.getContext("2d"), viewport: vp, background: "white" }).promise;
        const blob = await new Promise((resolve) => canvas.toBlob(resolve, "image/png"));
        canvas.width = canvas.height = 0;
        const img = document.createElement("img");
        img.className = "print-page";
        img.style.width = `${base.width}pt`;
        img.style.height = `${base.height}pt`;
        img.src = URL.createObjectURL(blob);
        printArea.appendChild(img);
      } finally {
        if (shown) page._pageInfo.view = shown;
      }
    }
    await Promise.all([...printArea.querySelectorAll("img")].map((i) => i.decode().catch(() => {})));
    hideProgress();
    window.print();
  } catch (err) {
    clearPrintArea();
    alert(`Could not print:\n${err}`);
  } finally {
    hideProgress();
    printing = false;
  }
}

document.getElementById("printBtn").addEventListener("click", printPdf);

// --- undo / redo ---
// Every edit keeps a copy of the PDF from before it, so Ctrl+Z steps back
// (a deleted page or object, a recolour, a page box…) and Ctrl+Y or
// Ctrl+Shift+Z steps forward again.

let undoBusy = false;

async function undoStep(undo) {
  if (!state.path || undoBusy || sel.busy) return;
  undoBusy = true;
  try {
    const changed = await invoke("undo_step", { path: state.path, undo });
    if (!changed) {
      setProgress(100, undo ? "Nothing to undo" : "Nothing to redo");
      setTimeout(hideProgress, 900);
      return;
    }
    setProgress(30, undo ? "Undoing…" : "Redoing…");
    const count = await invoke("open_pdf", { path: state.path });
    await afterPagesChanged(count, state.page);
    enableFixes();
    // Back to how the file was opened (and never saved): nothing to save.
    const counts = await invoke("undo_counts", { path: state.path });
    if (!counts.undo && !(activeTab && activeTab.saved)) setEdited(false);
  } catch (err) {
    alert(`Could not ${undo ? "undo" : "redo"}:\n${err}`);
  } finally {
    undoBusy = false;
    hideProgress();
  }
}

// Ctrl+S Save PDF as, Ctrl+O open, Ctrl+W close the tab, Ctrl+Z undo,
// Ctrl+Y / Ctrl+Shift+Z redo (typing fields keep their own undo).
window.addEventListener("keydown", (ev) => {
  if (!(ev.ctrlKey || ev.metaKey) || ev.altKey) return;
  const k = ev.key.toLowerCase();
  if ((k === "z" || k === "y") && typingIn(ev)) return;
  if (k === "p") printPdf();
  else if (k === "z") undoStep(!ev.shiftKey);
  else if (k === "y") undoStep(false);
  else if (k === "s") exportPdf();
  else if (k === "o") openPdf();
  else if (k === "w") {
    if (activeTab) closeTab(activeTab);
  } else return;
  ev.preventDefault();
});

// --- info text that can be folded away ---
// Each panel's explanation has an arrow; once people know the tools they
// can fold it away, and it stays folded next time (per panel).
function makeInfoCollapsible() {
  const KEY = "mangoprint.infoFolded";
  let folded = {};
  try {
    folded = JSON.parse(localStorage.getItem(KEY) || "{}") || {};
  } catch {
    folded = {};
  }
  const save = () => {
    try {
      localStorage.setItem(KEY, JSON.stringify(folded));
    } catch {
      // Just won't be remembered.
    }
  };
  for (const p of document.querySelectorAll("#sidebar .panel > p.hint:not([id])")) {
    const panel = p.closest(".panel");
    const title = panel.querySelector("h2")?.textContent.trim() || "panel";
    const key = `${title}#${[...panel.querySelectorAll(":scope > p.hint:not([id])")].indexOf(p)}`;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "info-toggle";
    btn.innerHTML = '<span class="info-icon">i</span><span class="info-arrow"></span>';
    const show = (open) => {
      p.classList.toggle("info-folded", !open);
      btn.classList.toggle("open", open);
      btn.title = open ? "Hide this explanation" : "Show the explanation";
      btn.setAttribute("aria-expanded", String(open));
    };
    show(!folded[key]);
    btn.addEventListener("click", () => {
      const open = p.classList.contains("info-folded");
      if (open) delete folded[key];
      else folded[key] = true;
      save();
      show(open);
    });
    p.before(btn);
  }

  // "Hide info" at the top of the sidebar hides every explanation (and
  // its button); unticking brings back each panel's own setting.
  const HIDE = "mangoprint.hideInfo";
  const box = document.getElementById("hideInfo");
  const apply = () => document.getElementById("sidebar").classList.toggle("info-hidden", box.checked);
  try {
    box.checked = localStorage.getItem(HIDE) === "1";
  } catch {
    box.checked = false;
  }
  apply();
  box.addEventListener("change", () => {
    try {
      localStorage.setItem(HIDE, box.checked ? "1" : "0");
    } catch {
      // Just won't be remembered.
    }
    apply();
  });
}
makeInfoCollapsible();

checkGhostscript();
renderPageBoxesPanel();
renderRotationPanel();
renderSelectPanel();
initMerge();
