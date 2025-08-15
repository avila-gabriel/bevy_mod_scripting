> WIP: test gleam gen code; clean glue based on whats not in game of life example

# JavaScript (Boa) Scripting Guide for `bevy_mod_scripting`

This guide explains **how JavaScript scripts interact with your Bevy app** via the Boa runtime and the `bevy_mod_scripting_js` plugin.  
We’ll separate **generic bindings concepts** from **Game of Life–specific wiring** so you can reuse the knowledge in other projects.

---

## 1. Generic: How JS talks to the Rust World

When `bevy_mod_scripting_js` loads a JS file, it injects:

- `world` — a handle to the **Bevy World**, accessed via reflection APIs.
- `entity` — the entity that owns the script (optional to use).
- `script_id` — string identifying the script instance.
- Any globals registered from Rust (e.g. `info`, `rand`).

There’s no `console.log`;

---

### 1.1 Calling into Rust (`world.call`)

`world.call(method, ...args)` invokes a reflected function registered by the core scripting API.

Common methods you’ll use:

```js
const LifeState = world.call("get_type_by_name", "LifeState");

const query = world.call("query").call("component", LifeState).call("build");

for (const entry of query) {
    const comps = entry.call("components");
    const comp  = comps[0];
}
```

> Always build queries fresh when needed — don’t keep the builder around between frames.

---

### 1.2 ReflectReference (Rust values in JS)

Most objects you work with in JS are **`ReflectReference`** handles to live Rust data.

They provide:

```js
handle.get(fieldName);
handle.set(fieldNameOrIndex, value);
handle.len();
handle.call("method", ...args);
```

Special cases:

- **Tuples**: access fields as `"_0"`, `"_1"`, etc.
- **Vec<u8> or Vec<T>**: use `get(index)`, `set(index, value)`, `len()` — don’t try `call("set", ...)`.

---

### 1.3 Lifecycle Callbacks

Your script can define these functions; they’ll be invoked from Rust:

- `on_script_loaded()` — runs once after the script is loaded.
- `on_update()` — called each fixed update (depends on `UPDATE_FREQUENCY` in Rust).
- `on_click(x, y)` — called from Rust when a mouse click occurs (if wired).

These names come from `callback_labels!` in Rust:

```rs
callback_labels!(
    OnUpdate => "on_update",
    OnClick  => "on_click"
);
```

---

### 1.4 Registering Globals in Rust

In the Bevy app, you can make functions available to all scripts:

```rs
pub fn register_script_functions(app: &mut App) -> &mut App {
    let world = app.world_mut();
    NamespaceBuilder::<GlobalNamespace>::new_unregistered(world)
        .register("info", |s: String| bevy::log::info!(s))
        .register("rand", rand::random::<f32>);
    app
}
```

In JS:

```js
info("Hello from JS");
let r = rand();
```

---

### 1.5 Data Mapping JS ↔ Rust

- **Numbers**: JS `number` ↔ Rust numeric types (f64 by default).
- **Booleans**: match directly.
- **Strings**: match directly.
- **JS arrays**: ↔ Rust `Vec<ScriptValue>`.
- **JS objects**: ↔ Rust `HashMap<String, ScriptValue>`.
- **ReflectReference**: live pointer to Rust value.
- **Tuples**: read/write as `"_0"`, `"_1"`, etc.

---

## 2. Game of Life–Specific Wiring

The `main.rs` wiring for Game of Life is just an example of how to hook up scripting:

- **LifeState component**:  
  ```rs
  #[derive(Debug, Default, Clone, Reflect, Component)]
  pub struct LifeState {
      pub cells: Vec<u8>,
  }
  ```
  Accessible in JS via `life.get("cells")`.

- **Settings resource**:  
  Stores grid dimensions and colors, accessible in JS:
  ```js
  let Settings = world.call("get_type_by_name", "Settings");
  let settings = world.call("get_resource", Settings);
  ```

- **Script events**:  
  The Rust code sends `ScriptCallbackEvent` for `OnUpdate` and `OnClick` every frame / on mouse click.

- **Rendering**:  
  Scripts mutate `LifeState.cells`, and `update_rendered_state` copies that to the GPU texture.

- **Command interface** (`gol start <lang>`):  
  Lets you spawn script entities dynamically via console commands.

---

## 3. Common Pitfalls

- **Wrong setter**: Use `cells.set(i, v)` for Vec; `cells.call("set", i, v)` won’t work.
- **Borrow errors**: Don’t hold query builders across frames; extract what you need immediately.
- **Bad tuple reads**: Remember `"_0"`, `"_1"`.
- **No output**: Use `info` — `console.log` doesn’t exist.

---

## 4. Minimal Example

```js
let MyComp = null;

function on_script_loaded() {
    MyComp = world.call("get_type_by_name", "MyComponent");
}

function on_update() {
    const q = world.call("query").call("component", MyComp).call("build");
    for (const e of q) {
        const comps = e.call("components");
        const c = comps[0];
        const count = c.get("counter");
        c.set("counter", count + 1);
    }
}
```

---

With this separation, you now know what’s **generic scripting API** and what’s just **Game of Life–specific setup**.  
You can swap out `LifeState` and `Settings` for your own types and the principles stay the same.

Also put this near the top of the script to see whats in globalThis (game_of_life):
```js
info((() => {
  const m=[], v=[];
  for (const k of Object.getOwnPropertyNames(globalThis)) {
    (typeof globalThis[k] === 'function' ? m : v).push(k);
  }
  return `=== Methods ===\n${m.sort().join(", ")}\n\n=== Other values ===\n${v.sort().join(", ")}`;
})());
```