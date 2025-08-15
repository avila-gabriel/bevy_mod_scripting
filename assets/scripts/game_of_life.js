// -------------------- helpers --------------------
function readTuple2RR(rr) {
  // tuples are exposed as fields "_0", "_1"
  const a = rr.get("_0");
  const b = rr.get("_1");
  return [(a | 0), (b | 0)];
}

function fetch_life_state() {
  const LifeState = world.call("get_type_by_name", "LifeState");
  const q = world.call("query").call("component", LifeState).call("build");
  for (const entry of q) {
    const comps = entry.call("components");
    return comps[0];
  }
  return undefined;
}

// -------------------- cached refs --------------------
let life_ref = null;   // ReflectReference to LifeState component
let cells_ref = null;  // ReflectReference to Vec<u8>
let prev = [];         // reused snapshot buffer

function ensure_refs() {
  if (cells_ref) return true;
  life_ref = fetch_life_state();
  if (!life_ref) return false;
  cells_ref = life_ref.get("cells");
  return !!cells_ref;
}

// -------------------- parameters --------------------
const SEED_COUNT = 1000;

// -------------------- callbacks --------------------
function on_script_loaded() {
  if (!ensure_refs()) return;

  const cells = cells_ref;
  const len = cells.len() | 0;

  // random seeding
  let seeded = 0;
  for (let i = 0; i < Math.min(SEED_COUNT, len); i++) {
    const idx = (rand() * len) | 0;
    cells.set(idx, 255);
    seeded++;
  }
}

function on_click(x, y) {
  if (!ensure_refs()) return;

  const settings = world.call("get_resource", world.call("get_type_by_name", "Settings"));
  const [dx, dy] = readTuple2RR(settings.get("physical_grid_dimensions"));
  const [sx, sy] = readTuple2RR(settings.get("display_grid_dimensions"));
  if (dx === 0 || dy === 0 || sx === 0 || sy === 0) return;

  const cw = sx / dx, ch = sy / dy;
  const cx = Math.floor(x / cw), cy = Math.floor(y / ch);
  const base = cy * dx + cx;

  const cells = cells_ref;
  const OFF = [[0,0],[1,0],[0,1],[1,1],[-1,0],[0,-1],[-1,-1],[1,-1],[-1,1]];
  for (const [ox, oy] of OFF) {
    const i = base + ox + oy * dx;
    if (i >= 0 && i < dx * dy) cells.set(i, 255);
  }
}

function on_update() {
  if (!ensure_refs()) return;

  const cells = cells_ref;
  const len = cells.len() | 0;

  // grid size from Settings
  const settings = world.call("get_resource", world.call("get_type_by_name", "Settings"));
  const [dx, dy] = readTuple2RR(settings.get("physical_grid_dimensions"));
  if (dx === 0 || dy === 0 || dx * dy !== len) return;

  // snapshot (reuses array)
  if (prev.length !== len) prev.length = len;
  for (let i = 0; i < len; i++) prev[i] = cells.get(i) !== 0;

  // Conway step with toroidal wrapping
  for (let y = 0; y < dy; y++) {
    const yU = (y === 0 ? dy - 1 : y - 1);
    const yD = (y + 1 === dy ? 0 : y + 1);
    const row = y * dx, rowU = yU * dx, rowD = yD * dx;

    for (let x = 0; x < dx; x++) {
      const xL = (x === 0 ? dx - 1 : x - 1);
      const xR = (x + 1 === dx ? 0 : x + 1);
      const i  = row + x;

      const n =
        (prev[rowU + xL] ? 1 : 0) + (prev[rowU + x] ? 1 : 0) + (prev[rowU + xR] ? 1 : 0) +
        (prev[row  + xL] ? 1 : 0)                            + (prev[row  + xR] ? 1 : 0) +
        (prev[rowD + xL] ? 1 : 0) + (prev[rowD + x] ? 1 : 0) + (prev[rowD + xR] ? 1 : 0);

      if (!prev[i] && n === 3)      cells.set(i, 255);
      else if (prev[i] && (n < 2 || n > 3)) cells.set(i, 0);
      // else unchanged
    }
  }
}

function on_script_unloaded() {
  if (!cells_ref) return;
  const len = cells_ref.len() | 0;
  for (let i = 0; i < len; i++) cells_ref.set(i, 0);
}
