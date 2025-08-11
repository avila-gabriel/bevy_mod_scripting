use super::reference::{JsReflectReference, JsStaticReflectReference};
use bevy_mod_scripting_core::{
    asset::Language,
    bindings::{function::script_function::FunctionCallContext, script_value::ScriptValue},
    error::InteropError,
};
use boa_engine::{
    js_string,
    native_function::NativeFunction,
    object::{JsObject, FunctionObjectBuilder, builtins::{JsArray, JsFunction}},
    property::PropertyKey,
    value::{Convert, TryFromJs},
    Context, JsString, JsValue,
    error::JsNativeError,
};
use std::{
    cell::Cell,
    ops::{Deref, DerefMut},
    collections::VecDeque,
};

/// Make the current Boa [`Context`] available to nested conversions and callbacks
/// during `f(ctx)` by stashing `ctx` in thread-local storage.
///
/// This mirrors the world container pattern: it assumes single-threaded use of a
/// given VM. The TLS pointer is set for the duration of the call and then restored.
///
/// # Safety
/// The pointer stored in TLS is only valid for the lifetime of this call; callers
/// must ensure the VM is not accessed from other threads while this is active.
pub fn with_js_context<R>(ctx: &mut Context, f: impl FnOnce(&mut Context) -> R) -> R {
    TLS_JS_CTX.with(|cell| {
        let prev = cell.replace(ctx as *mut _);
        let r = f(ctx);
        cell.set(prev);
        r
    })
}

/// Get a mutable reference to the current thread’s active Boa [`Context`]
/// previously installed by [`with_js_context`].
///
/// Returns an [`InteropError`] if no context is currently installed.
/// The returned reference is `'static` only by construction; it is valid
/// **only** while the outer `with_js_context` call is on the stack.
pub fn try_current_ctx() -> Result<&'static mut Context, InteropError> {
    TLS_JS_CTX.with(|cell| {
        let ptr = cell.get();
        if ptr.is_null() {
            Err(InteropError::invariant(
                "no active JS Context in thread-local storage; \
                 wrap the call site with `with_js_context(ctx, || ...)`",
            ))
        } else {
            // SAFETY: confined to single-threaded use, set by with_js_context.
            Ok(unsafe { &mut *ptr })
        }
    })
}

// ---------- TLS: make the current Boa Context available to nested callbacks ----------
// Safety note: exactly like your World container pattern, this assumes single-threaded
// use of a given VM. We only borrow for the duration of a guarded call.
thread_local! {
    static TLS_JS_CTX: Cell<*mut Context> = const { Cell::new(std::ptr::null_mut()) };
}

fn ext<E: std::fmt::Display>(e: E) -> InteropError {
    InteropError::invariant(e.to_string())
    // or InteropError::external(e.to_string()) if you have that
}

// ---------- Registry in globalThis to keep JS callables GC-reachable ----------

fn ensure_registry(ctx: &mut Context) -> Result<JsObject, InteropError> {
    let global = ctx.global_object().clone();
    let key = js_string!("__bms_fn_reg");

    let maybe = global.get(key.clone(), ctx).map_err(ext)?;
    if maybe.is_undefined() {
        let reg = JsObject::with_null_proto();
        global.set(key, reg.clone(), false, ctx).map_err(ext)?;
        Ok(reg)
    } else {
        Ok(maybe
            .as_object()
            .ok_or_else(|| InteropError::invariant("registry is not an object"))?
            .clone())
    }
}

thread_local! {
    static NEXT_FN_ID: Cell<u64> = const { Cell::new(0) };
}

fn next_fn_id() -> u64 {
    NEXT_FN_ID.with(|c| {
        let id = c.get();
        c.set(id.wrapping_add(1));
        id
    })
}

fn store_callable(ctx: &mut Context, f: JsFunction) -> u64 {
    let id = next_fn_id();
    let reg = ensure_registry(ctx).expect("registry must be creatable");
    let key = PropertyKey::from(JsString::from(format!("fn:{id}")));
    let _ = reg.set(key, JsValue::from(f), false, ctx);
    id
}

fn call_stored(ctx: &mut Context, id: u64, args: &[JsValue]) -> Result<JsValue, InteropError> {
    let reg = ensure_registry(ctx)?;
    let key = PropertyKey::from(JsString::from(format!("fn:{id}")));
    let v = reg.get(key, ctx).map_err(ext)?;
    let f = v
        .as_function()
        .ok_or_else(|| InteropError::invariant("stored value is not a function"))?;
    f.call(&JsValue::undefined(), args, ctx).map_err(ext)
}

// ---------- Public wrapper ----------

/// A wrapper around a [`ScriptValue`] that implements conversions to/from Boa:
/// see [`FromJs`] and [`IntoJs`].
#[derive(Debug, Clone)]
pub struct JsScriptValue(pub ScriptValue);

// Deref helpers
impl Deref for JsScriptValue {
    type Target = ScriptValue;
    fn deref(&self) -> &Self::Target { &self.0 }
}
impl DerefMut for JsScriptValue {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.0 }
}

// Round-trip ScriptValue <-> wrapper
impl From<ScriptValue> for JsScriptValue { fn from(v: ScriptValue) -> Self { Self(v) } }
impl From<JsScriptValue> for ScriptValue { fn from(v: JsScriptValue) -> Self { v.0 } }

/// Convert a Boa [`JsValue`] into a Rust type, using the given Boa [`Context`].
pub trait FromJs: Sized {
    /// Attempt to convert `value` into `Self`, possibly allocating JS objects
    /// or consulting the VM state via `ctx`.
    fn from_js(value: &JsValue, ctx: &mut Context) -> Result<Self, InteropError>;
}

/// Convert a Rust type into a Boa [`JsValue`], using the given Boa [`Context`].
pub trait IntoJs {
    /// Produce a JS value representing `self`, possibly allocating into the
    /// target realm and consulting the VM via `ctx`.
    fn into_js(self, ctx: &mut Context) -> Result<JsValue, InteropError>;
}

/// The caller context used when invoking host functions from JavaScript
pub const JS_CALLER_CONTEXT: FunctionCallContext = FunctionCallContext::new(Language::Js);

impl FromJs for JsScriptValue {
    fn from_js(value: &JsValue, ctx: &mut Context) -> Result<Self, InteropError> {
        // null/undefined
        if value.is_null_or_undefined() {
            return Ok(Self(ScriptValue::Unit));
        }

        // string (must come before bool/number conversions to avoid truthiness)
        if let Some(s) = value.as_string() {
            return Ok(Self(ScriptValue::String(s.to_std_string_escaped().into())));
        }

        // function — store in registry and return a callable ScriptValue
        if let Some(func) = value.as_function() {
            let id = store_callable(ctx, func.clone());
            let fun = move |_fcx: FunctionCallContext, args: VecDeque<ScriptValue>| {
                let ctx = match try_current_ctx() {
                    Ok(c) => c,
                    Err(e) => return ScriptValue::Error(e),
                };

                let mut js_args = Vec::with_capacity(args.len());
                for a in args {
                    match JsScriptValue(a).into_js(ctx) {
                        Ok(v) => js_args.push(v),
                        Err(e) => return ScriptValue::Error(e),
                    }
                }

                match call_stored(ctx, id, &js_args) {
                    Ok(v) => match JsScriptValue::from_js(&v, ctx) {
                        Ok(w) => w.0,
                        Err(e) => ScriptValue::Error(e),
                    },
                    Err(e) => ScriptValue::Error(e),
                }
            };
            return Ok(Self(ScriptValue::Function(fun.into())));
        }

        // object / array / reference / static-type
        if let Some(obj) = value.as_object() {
            // 0) Static type handle? (TypeId wrapper)
            if let Some(sr) = obj.downcast_ref::<JsStaticReflectReference>() {
                // TypeId is Copy; this just copies the id
                return Ok(Self(ScriptValue::StaticReference(sr.0)));
            }

            // 1) Instance ReflectReference?
            if let Some(rr) = obj.downcast_ref::<JsReflectReference>() {
                return Ok(Self(ScriptValue::Reference(rr.0.clone())));
            }

            // 2) Array -> List
            if let Ok(arr) = JsArray::from_object(obj.clone()) {
                let len = arr.length(ctx).map_err(ext)? as usize;
                let arr_obj = arr.deref().clone();
                let mut out = Vec::with_capacity(len);
                for i in 0..len {
                    let elem = arr_obj.get(i as i32, ctx).map_err(ext)?;
                    out.push(JsScriptValue::from_js(&elem, ctx)?.0);
                }
                return Ok(Self(ScriptValue::List(out)));
            }

            // 3) Plain object -> Map
            let mut map = std::collections::HashMap::new();

            let object_ctor = ctx.global_object()
                .get(js_string!("Object"), ctx).map_err(ext)?
                .as_object().ok_or_else(|| InteropError::invariant("global Object is not an object"))?
                .clone();

            let keys_fn = object_ctor
                .get(js_string!("keys"), ctx).map_err(ext)?
                .as_object().ok_or_else(|| InteropError::invariant("Object.keys is not a function"))?
                .clone();

            let keys_val = keys_fn
                .call(&JsValue::undefined(), &[JsValue::from(obj.clone())], ctx)
                .map_err(ext)?;

            let keys_arr = JsArray::from_object(
                keys_val.as_object().ok_or_else(|| InteropError::invariant("Object.keys did not return an object"))?.clone()
            ).map_err(|_| InteropError::invariant("Object.keys did not return an array"))?;

            let len = keys_arr.length(ctx).map_err(ext)? as usize;
            let keys_obj = keys_arr.deref().clone();

            for i in 0..len {
                let key_val = keys_obj.get(i as i32, ctx).map_err(ext)?;
                let key_js = key_val.to_string(ctx).map_err(ext)?;
                let key = key_js.to_std_string_escaped();

                let val = obj.get(key_js, ctx).map_err(ext)?;
                map.insert(key, JsScriptValue::from_js(&val, ctx)?.0);
            }

            return Ok(Self(ScriptValue::Map(map)));
        }

        // number (guard so booleans don't get coerced to 0/1)
        if value.is_number() {
            if let Ok(Convert(i)) = Convert::<i32>::try_from_js(value, ctx) {
                return Ok(Self(ScriptValue::Integer(i as i64)));
            }
            if let Ok(Convert(f)) = Convert::<f64>::try_from_js(value, ctx) {
                return Ok(Self(ScriptValue::Float(f)));
            }
        }

        // boolean LAST (and only if it's actually a boolean)
        if value.is_boolean() {
            if let Ok(Convert(b)) = Convert::<bool>::try_from_js(value, ctx) {
                return Ok(Self(ScriptValue::Bool(b)));
            }
        }

        Err(InteropError::invariant(
            "Unsupported JsValue -> ScriptValue with current docs",
        ))
    }
}


impl IntoJs for JsScriptValue {
    fn into_js(self, ctx: &mut Context) -> Result<JsValue, InteropError> {
        match self.0 {
            ScriptValue::Unit => Ok(JsValue::undefined()),
            ScriptValue::Bool(b) => Ok(JsValue::from(b)),
            ScriptValue::Integer(i) => Ok(JsValue::from(i as f64)),
            ScriptValue::Float(f) => Ok(JsValue::from(f)),
            ScriptValue::String(s) => Ok(JsValue::from(JsString::from(s.as_ref()))),

            ScriptValue::Reference(r) => {
                // Create a `ReflectReference` JS object with embedded Rust data.
                let ctor = ctx
                    .global_object()
                    .get(js_string!("ReflectReference"), ctx)
                    .map_err(|e| InteropError::invariant(e.to_string()))?
                    .as_object()
                    .ok_or_else(|| InteropError::invariant("ReflectReference ctor missing"))?
                    .clone();

                let proto = ctor
                    .get(js_string!("prototype"), ctx)
                    .map_err(|e| InteropError::invariant(e.to_string()))?
                    .as_object()
                    .cloned();

                let obj = boa_engine::object::JsObject::from_proto_and_data(
                    proto,
                    JsReflectReference::from(r),
                );
                Ok(JsValue::from(obj))
            },

            ScriptValue::StaticReference(tid) => {
                let ctor = ctx
                    .global_object()
                    .get(js_string!("StaticReflectReference"), ctx)
                    .map_err(|e| InteropError::invariant(e.to_string()))?
                    .as_object()
                    .ok_or_else(|| InteropError::invariant("StaticReflectReference ctor missing"))?
                    .clone();

                let proto = ctor.get(js_string!("prototype"), ctx)
                    .map_err(|e| InteropError::invariant(e.to_string()))?
                    .as_object()
                    .cloned();

                let obj = boa_engine::object::JsObject::from_proto_and_data(
                    proto,
                    JsStaticReflectReference(tid),
                );
                Ok(JsValue::from(obj))
            },

            // Stateless function
            ScriptValue::Function(function) => {
                let nf = unsafe {
                    NativeFunction::from_closure(move |_this, js_args, js_ctx| {
                        let mut sv_args = VecDeque::with_capacity(js_args.len());
                        for v in js_args {
                            let sv = JsScriptValue::from_js(v, js_ctx)
                                .map_err(|e| JsNativeError::error().with_message(e.to_string()))?
                                .0;
                            sv_args.push_back(sv);
                        }

                        let out = function
                            .call(sv_args, JS_CALLER_CONTEXT)
                            .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

                        let js = JsScriptValue(out)
                            .into_js(js_ctx)
                            .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
                        Ok(js) // <-- return it
                    })
                };

                let fobj = FunctionObjectBuilder::new(ctx.realm(), nf)
                    .name("hostFn").length(0).constructor(false).build();
                Ok(JsValue::from(fobj))
            },

            // Stateful/mutable function
            ScriptValue::FunctionMut(function_mut) => {
                let nf = unsafe {
                    NativeFunction::from_closure(move |_this, js_args, js_ctx| {
                        let mut sv_args = VecDeque::with_capacity(js_args.len());
                        for v in js_args {
                            let sv = JsScriptValue::from_js(v, js_ctx)
                                .map_err(|e| JsNativeError::error().with_message(e.to_string()))?
                                .0;
                            sv_args.push_back(sv);
                        }

                        let out = function_mut
                            .call(sv_args, JS_CALLER_CONTEXT)
                            .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

                        let js = JsScriptValue(out)
                            .into_js(js_ctx)
                            .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
                        Ok(js) // <-- and return here too
                    })
                };

                let fobj = FunctionObjectBuilder::new(ctx.realm(), nf)
                    .name("hostFn").length(0).constructor(false).build();
                Ok(JsValue::from(fobj))
            },

            ScriptValue::List(items) => {
                let mut js_items = Vec::with_capacity(items.len());
                for it in items {
                    js_items.push(JsScriptValue(it).into_js(ctx)?);
                }
                let arr = JsArray::from_iter(js_items.into_iter(), ctx);
                Ok(JsValue::from(arr))
            },

            ScriptValue::Map(map) => {
                let mut entries = Vec::with_capacity(map.len());
                for (k, v) in map {
                    let key = JsString::from(k);
                    let val = JsScriptValue(v).into_js(ctx)?;
                    entries.push((key, val));
                }

                let mut init = boa_engine::object::ObjectInitializer::new(ctx);
                for (k, v) in entries {
                    init.property(k, v, boa_engine::property::Attribute::all());
                }
                Ok(JsValue::from(init.build()))
            }

            // Just propagate the error (don’t wrap it as “external”).
            ScriptValue::Error(e) => Err(e),
        }
    }
}
