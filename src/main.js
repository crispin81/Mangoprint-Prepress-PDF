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
  modeOverprint: document.getElementById("modeOverprint"),
  modeSeparations: document.getElementById("modeSeparations"),
  boxOverlay: document.getElementById("boxOverlay"),
  pageBoxes: document.getElementById("pageBoxes"),
  pageRotation: document.getElementById("pageRotation"),
  rotationCurrent: document.getElementById("rotationCurrent"),
  unitMm: document.getElementById("unitMm"),
  unitPt: document.getElementById("unitPt"),
  winMin: document.getElementById("winMin"),
  winMax: document.getElementById("winMax"),
  winClose: document.getElementById("winClose"),
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
  dpi: 300, // on-screen preview resolution (the PDF itself is never rasterised)
  mode: "overprint", // "overprint" | "separations"
  simulateOverprint: false,
  separationNames: [],
  activeSeparations: new Set(),
  renderToken: 0,
  pageBoxes: null,
  boxVisible: new Set(["trim", "bleed"]),
  unit: "mm", // "mm" | "pt" — PDF stores points; mm is converted for display/entry
};

// Eyedropper request state (see the eyedropper section below).
const eyedrop = { inFlight: false, pending: null, held: false };

const MM_PER_PT = 25.4 / 72;
const toUnit = (pt) => (state.unit === "mm" ? pt * MM_PER_PT : pt);
const fromUnit = (v) => (state.unit === "mm" ? v / MM_PER_PT : v);
const fmtMm = (pt) => (pt * MM_PER_PT).toFixed(2);

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
    if (state.mode === "separations") {
      await loadSeparationsList();
    }
    await loadPageBoxes();
    await renderCurrent();
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
  let range = null;
  try {
    range = await invoke("page_image_dpi", { path: state.path, page: state.page });
  } catch (err) {
    console.error(err);
    els.rasterDpi.textContent = "?";
    return;
  }
  if (!range) {
    els.rasterDpi.textContent = "no raster images";
    return;
  }
  const [lo, hi] = range;
  els.rasterDpi.textContent = lo === hi ? `${lo}` : `${lo}–${hi}`;
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
  if (eyedrop.held) {
    eyedrop.held = false;
    showInkMessage("Hover over the page to read ink values.");
  }
  els.pageLabel.textContent = state.path ? `Page ${state.page} / ${state.pageCount}` : "–";
  els.prevPage.disabled = !state.path || state.page <= 1;
  els.nextPage.disabled = !state.path || state.page >= state.pageCount;
}

async function loadSeparationsList() {
  if (!state.path) return;
  const names = await invoke("list_separations", {
    path: state.path,
    page: state.page,
    dpi: state.dpi,
  });
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

    li.append(checkbox, swatch, label);
    els.separationsList.appendChild(li);
  }
}

async function renderCurrent() {
  if (!state.path) return;
  const token = ++state.renderToken;
  setLoading(true);
  try {
    let dataUri;
    if (state.mode === "overprint") {
      dataUri = await invoke("render_overprint", {
        path: state.path,
        page: state.page,
        dpi: state.dpi,
        simulate: state.simulateOverprint,
      });
    } else {
      dataUri = await invoke("render_separation_composite", {
        path: state.path,
        page: state.page,
        dpi: state.dpi,
        active: [...state.activeSeparations],
      });
    }
    if (token !== state.renderToken) return; // a newer render superseded this one
    els.preview.src = dataUri;
    document.getElementById("imgWrap").classList.remove("empty");
    document.getElementById("emptyState").classList.add("hidden");
  } catch (err) {
    if (token === state.renderToken) alert(`Render failed:\n${err}`);
  } finally {
    if (token === state.renderToken) setLoading(false);
  }
}

async function goToPage(delta) {
  const next = state.page + delta;
  if (next < 1 || next > state.pageCount) return;
  state.page = next;
  updatePageControls();
  updateRasterDpi();
  if (state.mode === "separations") await loadSeparationsList();
  await loadPageBoxes();
  await renderCurrent();
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

async function applyBox(def, inputs) {
  setBoxError(def.key, "");
  const rect = inputs.map((i) => fromUnit(Number.parseFloat(i.value)));
  if (rect.some((v) => !Number.isFinite(v))) {
    setBoxError(def.key, "Enter a number for all four fields.");
    return;
  }
  const [x0, y0, x1, y1] = rect;
  if (!(x1 > x0 && y1 > y0)) {
    setBoxError(def.key, "x1 must be greater than x0, and y1 greater than y0.");
    return;
  }
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
  if (state.mode === "separations") await loadSeparationsList();
  await renderCurrent();
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

    const [rx0, ry0, rx1, ry1] = info.rect;
    const w = rx1 - rx0;
    const h = ry1 - ry0;
    const size = document.createElement("div");
    size.className = "box-size";
    size.textContent = `${fmtMm(w)} × ${fmtMm(h)} mm  ·  ${w.toFixed(2)} × ${h.toFixed(2)} pt`;

    const fields = document.createElement("div");
    fields.className = "box-fields";
    const fieldLabels = ["x0", "y0", "x1", "y1"].map((n) => `${n} (${state.unit})`);
    const inputs = fieldLabels.map((fname, i) => {
      const label = document.createElement("label");
      label.textContent = fname;
      const input = document.createElement("input");
      input.type = "number";
      input.step = state.unit === "mm" ? "0.01" : "0.1";
      input.value = toUnit(info.rect[i]).toFixed(2);
      label.appendChild(input);
      fields.appendChild(label);
      return input;
    });

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
    titleEl.textContent = `${def.label}: ${fmtMm(bx1 - bx0)} × ${fmtMm(by1 - by0)} mm (${(bx1 - bx0).toFixed(1)} × ${(by1 - by0).toFixed(1)} pt)`;
    rect.appendChild(titleEl);

    svg.appendChild(rect);
  }
}

// --- wiring ---

els.winMin.addEventListener("click", () => appWindow.minimize());
els.winMax.addEventListener("click", () => appWindow.toggleMaximize());
els.winClose.addEventListener("click", () => appWindow.close());

function setUnit(unit) {
  state.unit = unit;
  els.unitMm.classList.toggle("active", unit === "mm");
  els.unitPt.classList.toggle("active", unit === "pt");
  renderPageBoxesPanel();
}
els.unitMm.addEventListener("click", () => setUnit("mm"));
els.unitPt.addEventListener("click", () => setUnit("pt"));

// --- eyedropper ---
// Hovering the preview asks the backend for ink % at that point (read
// from the tiffsep plates). Only one request is in flight at a time; the
// latest pointer position is sent once it returns.

const inkReadout = document.getElementById("inkReadout");
const eyedropToggle = document.getElementById("eyedropToggle");
const imgWrap = document.getElementById("imgWrap");

function setEyedropCursor() {
  imgWrap.classList.toggle("eyedrop", eyedropToggle.checked);
}

function showInkMessage(text) {
  inkReadout.classList.remove("held");
  inkReadout.innerHTML = "";
  const li = document.createElement("li");
  li.className = "hint";
  li.textContent = text;
  inkReadout.appendChild(li);
}

function showInks(samples) {
  inkReadout.innerHTML = "";
  inkReadout.classList.toggle("held", eyedrop.held);
  let total = 0;
  for (const { name, percent } of samples) {
    total += percent;
    const li = document.createElement("li");
    const sw = document.createElement("span");
    sw.className = "swatch";
    sw.style.background = SWATCHES[name] || spotSwatch(name);
    const n = document.createElement("span");
    n.className = "ink-name";
    n.textContent = name;
    const v = document.createElement("span");
    v.className = "ink-value";
    v.textContent = `${Math.round(percent)}%`;
    li.append(sw, n, v);
    inkReadout.appendChild(li);
  }
  const t = document.createElement("li");
  t.className = "ink-total";
  t.innerHTML = `<span class="ink-name">Total ink</span><span class="ink-value">${Math.round(total)}%</span>`;
  inkReadout.appendChild(t);
}

async function sampleAt(x, y) {
  if (eyedrop.inFlight) {
    eyedrop.pending = [x, y];
    return;
  }
  eyedrop.inFlight = true;
  try {
    const samples = await invoke("sample_inks", { path: state.path, page: state.page, dpi: state.dpi, x, y });
    if (eyedrop.pending === null) showInks(samples); // skip if a newer point is queued
  } catch (err) {
    showInkMessage(String(err));
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
  const r = els.preview.getBoundingClientRect();
  return [(ev.clientX - r.left) / r.width, (ev.clientY - r.top) / r.height];
}

els.preview.addEventListener("mousemove", (ev) => {
  if (!eyedropToggle.checked || !state.path || eyedrop.held) return;
  if (eyedrop.inFlight === false && inkReadout.children.length <= 1) showInkMessage("Reading separations…");
  sampleAt(...pointerFraction(ev));
});

els.preview.addEventListener("click", (ev) => {
  if (!eyedropToggle.checked || !state.path) return;
  eyedrop.held = !eyedrop.held;
  inkReadout.classList.toggle("held", eyedrop.held);
  sampleAt(...pointerFraction(ev));
});

eyedropToggle.addEventListener("change", () => {
  eyedrop.held = false;
  setEyedropCursor();
  showInkMessage(eyedropToggle.checked ? "Hover over the page to read ink values." : "Eyedropper is off.");
});
setEyedropCursor();

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

els.modeOverprint.addEventListener("change", () => {
  if (!els.modeOverprint.checked) return;
  state.mode = "overprint";
  renderCurrent();
});

els.modeSeparations.addEventListener("change", async () => {
  if (!els.modeSeparations.checked) return;
  state.mode = "separations";
  await loadSeparationsList();
  await renderCurrent();
});

checkGhostscript();
renderPageBoxesPanel();
renderRotationPanel();
