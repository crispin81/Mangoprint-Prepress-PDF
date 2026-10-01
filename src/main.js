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
  setLoading(true);
  try {
    const path = await invoke("create_working_copy", { path: source });
    const pageCount = await invoke("open_pdf", { path });
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
  } catch (err) {
    alert(`Could not open PDF:\n${err}`);
  } finally {
    setLoading(false);
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
  if (view.doc) {
    view.doc.destroy();
    view.doc = null;
    view.page = null;
  }
  const buf = await invoke("read_pdf", { path: state.path });
  const url = (p) => new URL(p, location.href).href;
  view.doc = await lib.getDocument({
    data: new Uint8Array(buf),
    cMapUrl: url("vendor/pdfjs/cmaps/"),
    cMapPacked: true,
    standardFontDataUrl: url("vendor/pdfjs/standard_fonts/"),
    wasmUrl: url("vendor/pdfjs/wasm/"),
    iccUrl: url("vendor/pdfjs/iccs/"),
    isEvalSupported: false,
    enableXfa: false,
  }).promise;
}

async function loadVectorPage() {
  view.page = await view.doc.getPage(state.page);
  // pdf.js shows the CropBox by default; show the whole MediaBox instead so
  // bleed is visible and the view lines up with Ghostscript and the box
  // overlay (both MediaBox based).
  const media = state.pageBoxes && state.pageBoxes.media.rect;
  if (media && view.page._pageInfo) view.page._pageInfo.view = [...media];
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

async function goToPage(delta) {
  const next = state.page + delta;
  if (next < 1 || next > state.pageCount) return;
  state.page = next;
  updatePageControls();
  updateRasterDpi();
  loadSeparationsList();
  await loadPageBoxes();
  await showPage();
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

async function setRotation(degrees) {
  if (!state.path) return;
  try {
    state.pageBoxes = await invoke("set_page_rotation", { path: state.path, page: state.page, degrees });
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

async function applyBox(def, inputs) {
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
    state.pageBoxes = await invoke("set_page_box", { path: state.path, page: state.page, name: def.name, rect });
    await afterBoxEdit();
  } catch (err) {
    setBoxError(def.key, String(err));
  }
}

async function resetBox(def) {
  setBoxError(def.key, "");
  try {
    state.pageBoxes = await invoke("reset_page_box", { path: state.path, page: state.page, name: def.name });
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
  await showPage({ reloadDoc: true });
}

function renderPageBoxesPanel() {
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
    size.textContent = `${fmtMm(W - ins.left - ins.right)} × ${fmtMm(H - ins.top - ins.bottom)} mm`;

    // How far the bleed extends past the trim, per side.
    if (def.key === "bleed" && state.pageBoxes.trim) {
      const t = boxInsets(state.pageBoxes.trim);
      const amounts = INSET_SIDES.map((s) => toMm(t[s] - ins[s]));
      const same = amounts.every((a) => Math.abs(a - amounts[0]) < 0.005);
      size.textContent += same
        ? `  ·  ${amounts[0].toFixed(2)} mm bleed each side`
        : `  ·  bleed T ${amounts[0].toFixed(2)} / B ${amounts[1].toFixed(2)} / L ${amounts[2].toFixed(2)} / R ${amounts[3].toFixed(2)} mm`;
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

    const actions = document.createElement("div");
    actions.className = "box-row-actions";
    const applyBtn = document.createElement("button");
    applyBtn.textContent = "Apply";
    applyBtn.addEventListener("click", () => applyBox(def, inputs));
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
    const [x, y, w, h] = info.norm;

    const rect = document.createElementNS(SVG_NS, "rect");
    rect.setAttribute("x", x);
    rect.setAttribute("y", y);
    rect.setAttribute("width", w);
    rect.setAttribute("height", h);
    rect.setAttribute("stroke", def.color);
    rect.setAttribute("stroke-width", "2");
    if (def.dash !== "0") rect.setAttribute("stroke-dasharray", def.dash);

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

function clearInks() {
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
  if (!state.path || eyedrop.held || !state.separationNames.length) return;
  sampleAt(...pointerFraction(ev));
});

els.imgWrap.addEventListener("mouseleave", () => {
  eyedrop.inside = false;
  eyedrop.pending = null;
  if (!eyedrop.held) clearInks();
});

els.imgWrap.addEventListener("click", (ev) => {
  if (!state.path || !state.separationNames.length) return;
  eyedrop.held = !eyedrop.held;
  els.separationsList.classList.toggle("held", eyedrop.held);
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
