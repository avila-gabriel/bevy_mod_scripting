const RR = ReflectRef;
const SR = StaticReflectReference;

const staticCall = (typeHandle, name, ...args) => SR.call(typeHandle, name, ...args);
const refCall    = (obj, name, ...args)    => RR.ref_call(obj, name, ...args);
const refGet     = (obj, key)              => RR.ref_get(obj, key);
const refSet     = (obj, key, value)       => RR.ref_set(obj, key, value);
const refLen     = (obj)                   => RR.ref_len(obj);

// Resolve World (prefer `world`, otherwise `types.World`)
const World =
  (typeof world !== "undefined" && world) ||
  (typeof types === "object" && types && types.World);

const LifeState =
  (typeof globalThis.LifeState !== "undefined") ? globalThis.LifeState
  : staticCall(World, "get_type_by_name", "LifeState");

const Settings =
  (typeof globalThis.Settings !== "undefined") ? globalThis.Settings
  : staticCall(World, "get_type_by_name", "Settings");

function fetchLifeState() {
  const qb  = staticCall(World, "query");
  const qb2 = refCall(qb, "component", LifeState);
  const it  = refCall(qb2, "build");

  let first = null;
  if (Array.isArray(it)) first = it[0] ?? null;
  else if (it && typeof it === "object") {
    const ks = Object.keys(it);
    first = ks.length ? it[ks[0]] : null;
  } else first = it;

  if (!first) throw new Error("fetchLifeState: no query results");

  const comps = refCall(first, "components");
  if (!Array.isArray(comps) || comps.length === 0)
    throw new Error("fetchLifeState: first result has no components");
  return comps[0];
}

function dimsFromSettings() {
  const s    = staticCall(World, "get_resource", Settings);
  const phys = refGet(s, "physical_grid_dimensions");
  const disp = refGet(s, "display_grid_dimensions");
  const dimX = refGet(phys, "_0");
  const dimY = refGet(phys, "_1");
  const scrX = refGet(disp, "_0");
  const scrY = refGet(disp, "_1");
  return { dimX, dimY, scrX, scrY };
}

info("JS: the game_of_life.js script just got loaded");

let seeded = false;

function seedOnce() {
  const life  = fetchLifeState();
  const cells = refGet(life, "cells");
  const n     = refLen(cells) | 0;

  const seeds = Math.min(1000, n);
  for (let i = 0; i < seeds; i++) {
    const idx = (Math.random() * n) | 0;
    refSet(cells, idx, 255);
  }
  info("JS: seeded initial cells");
}

function on_script_loaded() {
  // IMPORTANT: do not touch the world here; just say hi.
  info("JS: Hello! I will seed on the first update tick.");
  info("JS: Click to toggle cells after `gol start`.");
}

function on_update() {
  if (!seeded) {
    seeded = true;
    seedOnce();        // defer all the real work here
  }

  const { dimX, dimY } = dimsFromSettings();
  const life  = fetchLifeState();
  const cells = refGet(life, "cells");
  const total = (dimX | 0) * (dimY | 0);

  const prev = new Uint8Array(total);
  for (let i = 0; i < total; i++) prev[i] = refGet(cells, i) ? 1 : 0;

  for (let i = 0; i < total; i++) {
    const x = i % dimX, y = (i / dimX) | 0;

    const north     = prev[(y === 0 ? dimY - 1 : y - 1) * dimX + x];
    const south     = prev[(y === dimY - 1 ? 0 : y + 1) * dimX + x];
    const east      = (x === dimX - 1) ? 0 : prev[i + 1];
    const west      = (x === 0)        ? 0 : prev[i - 1];
    const northeast = (x === dimX - 1) ? 0 : prev[(y === 0 ? dimY - 1 : y - 1) * dimX + (x + 1)];
    const southeast = (x === dimX - 1) ? 0 : prev[(y === dimY - 1 ? 0 : y + 1) * dimX + (x + 1)];
    const northwest = (x === 0)        ? 0 : prev[(y === 0 ? dimY - 1 : y - 1) * dimX + (x - 1)];
    const southwest = (x === 0)        ? 0 : prev[(y === dimY - 1 ? 0 : y + 1) * dimX + (x - 1)];

    const nbh = north + south + east + west + northeast + southeast + northwest + southwest;

    if (prev[i] === 0 && nbh === 3) {
      refSet(cells, i, 255);
    } else if (prev[i] === 1 && (nbh < 2 || nbh > 3)) {
      refSet(cells, i, 0);
    }
  }
}

function on_click(x, y) {
  const { dimX, dimY, scrX, scrY } = dimsFromSettings();

  const life  = fetchLifeState();
  const cells = refGet(life, "cells");

  const cx = Math.floor(x / (scrX / dimX));
  const cy = Math.floor(y / (scrY / dimY));
  const idx = cy * dimX + cx;

  const offsets = [
    [ 0,  0],[ 1,  0],[ 0,  1],[ 1,  1],
    [-1,  0],[ 0, -1],[-1, -1],[ 1, -1],
    [-1,  1]
  ];

  for (const [ox, oy] of offsets) {
    const j = idx + ox + oy * dimX;
    if (j >= 0 && j < dimX * dimY) refSet(cells, j, 255);
  }

  info(`JS: click at ${x}, ${y}`);
}

function on_script_unloaded() {
  info("JS: I am being unloaded, goodbye!");
  const life  = fetchLifeState();
  const cells = refGet(life, "cells");
  const n     = refLen(cells) | 0;
  for (let i = 0; i < n; i++) refSet(cells, i, 0);
}
