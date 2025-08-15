use super::reference::{JsReflectReference, JsStaticReflectReference};
use bevy_mod_scripting_core::{
    asset::Language,
    bindings::{function::script_function::FunctionCallContext, script_value::ScriptValue},
    error::InteropError,
};
use boa_engine::{
    error::JsNativeError,
    js_string,
    native_function::NativeFunction,
    object::{builtins::JsArray, FunctionObjectBuilder, JsObject},
    property::{Attribute, PropertyKey},
    Context, JsString, JsValue, Source,
};
use std::{
    cell::Cell,
    collections::VecDeque,
    ops::{Deref, DerefMut},
};

// Some host -> JS call paths (e.g., invoking a JS function that a script passed into a
// host API) happen outside the immediate event-handler frame where we hold `&mut Context`.
// We follow the same pattern as the World TLS: a guarded, single-thread assumption.
thread_local! { static TLS_JS_CTX: Cell<*mut Context> = const { Cell::new(std::ptr::null_mut()) }; }

/// Run `f` with `ctx` installed in TLS so nested conversions/callbacks can retrieve it.
pub fn with_js_context<R>(ctx: &mut Context, f: impl FnOnce(&mut Context) -> R) -> R {
    TLS_JS_CTX.with(|cell| {
        let prev = cell.replace(ctx as *mut _);
        let r = f(ctx);
        cell.set(prev);
        r
    })
}

/// Retrieve the current JS Context previously installed by `with_js_context`.
/// The returned reference is only valid while that call is on the stack.
pub fn try_current_ctx() -> Result<&'static mut Context, InteropError> {
    TLS_JS_CTX.with(|cell| {
        let ptr = cell.get();
        if ptr.is_null() {
            Err(InteropError::invariant(
                "no active JS Context; wrap call site with `with_js_context(ctx, ...)`",
            ))
        } else {
            // SAFETY: single-threaded access guaranteed by our usage pattern.
            Ok(unsafe { &mut *ptr })
        }
    })
}

#[inline]
fn ext<E: std::fmt::Display>(e: E) -> InteropError { InteropError::invariant(e.to_string()) }

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

thread_local! { static NEXT_FN_ID: Cell<u64> = const { Cell::new(0) }; }
fn next_fn_id() -> u64 { NEXT_FN_ID.with(|c| { let id = c.get(); c.set(id.wrapping_add(1)); id }) }

fn store_callable(ctx: &mut Context, f: boa_engine::object::builtins::JsFunction) -> u64 {
    let id = next_fn_id();
    let reg = ensure_registry(ctx).expect("registry creatable");
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

/// Wrapper around a [`ScriptValue`] that participates in conversions to and from
/// Boa's JavaScript values. Use this when shuttling values across the VM boundary.
#[derive(Debug, Clone)]
pub struct JsScriptValue(pub ScriptValue);

impl Deref for JsScriptValue {
    type Target = ScriptValue;
    fn deref(&self) -> &Self::Target { &self.0 }
}
impl DerefMut for JsScriptValue {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.0 }
}
impl From<ScriptValue> for JsScriptValue { fn from(v: ScriptValue) -> Self { Self(v) } }
impl From<JsScriptValue> for ScriptValue { fn from(v: JsScriptValue) -> Self { v.0 } }

/// Convert a Boa [`JsValue`] into a Rust type.
///
/// Implementors may need the active Boa [`Context`] to allocate or inspect
/// JS objects (e.g., to iterate arrays or read object properties). The context
/// is only valid for the duration of the call and must not be stored.
pub trait FromJs: Sized {
    /// Convert `value` from JavaScript into `Self`, using `ctx` for any VM interaction.
    ///
    /// Returns an [`InteropError`] when the value cannot be represented as `Self`
    /// (e.g., unsupported type, conversion failure).
    fn from_js(value: &JsValue, ctx: &mut Context) -> Result<Self, InteropError>;
}

/// Convert a Rust value into a Boa [`JsValue`].
///
/// Implementors may allocate objects/arrays/functions inside the provided
/// Boa [`Context`]. The produced value belongs to `ctx`'s realm.
pub trait IntoJs {
    /// Produce a JavaScript value for `self` in the given [`Context`].
    ///
    /// Returns an [`InteropError`] when conversion fails (e.g., attempting to
    /// encode an unsupported Rust type).
    fn into_js(self, ctx: &mut Context) -> Result<JsValue, InteropError>;
}

/// Caller context for host function calls originating from JS.
pub const JS_CALLER_CONTEXT: FunctionCallContext = FunctionCallContext::new(Language::Js);

impl FromJs for JsScriptValue {
    fn from_js(value: &JsValue, ctx: &mut Context) -> Result<Self, InteropError> {
        // undefined / null => Unit
        if value.is_null_or_undefined() {
            return Ok(Self(ScriptValue::Unit));
        }

        // Callable -> wrap in ScriptValue::Function (kept GC-reachable in the JS registry)
        if let Some(func) = value.as_function() {
            let id = store_callable(ctx, func.clone());
            let fun = move |_fcx: FunctionCallContext, args: VecDeque<ScriptValue>| {
                let ctx = match try_current_ctx() { Ok(c) => c, Err(e) => return ScriptValue::Error(e) };
                // to JS
                let mut js_args = Vec::with_capacity(args.len());
                for a in args {
                    match JsScriptValue(a).into_js(ctx) {
                        Ok(v) => js_args.push(v),
                        Err(e) => return ScriptValue::Error(e),
                    }
                }
                // call
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

        // Strings
        if let Some(s) = value.as_string() {
            return Ok(Self(ScriptValue::String(s.to_std_string_escaped().into())));
        }

        // Objects (includes arrays and our host wrappers)
        if let Some(obj) = value.as_object() {
            // 1) Dynamic reflect handle -> pass through as a real reference
            if let Some(rr) = obj.downcast_ref::<JsReflectReference>() {
                return Ok(Self(ScriptValue::Reference(rr.0.clone())));
            }

            // 2) Static type token: NOT representable as a ScriptValue without changing the enum.
            //    It’s meant to be used via its own `.call(...)` / `.fn(...)` entrypoints.
            if obj.downcast_ref::<JsStaticReflectReference>().is_some() {
                return Err(InteropError::unsupported_operation(
                    None,
                    None,
                    "StaticReflectReference cannot be passed as a value; use its .call(...)/.fn(...) APIs or get a dynamic type handle via world.get_type_by_name(...)".to_owned(),
                ));
            }

            // 3) Array => List
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

            // 4) Plain object => Map<String, ScriptValue> (Object.keys + toString on each key)
            let object_ctor = ctx
                .global_object()
                .get(js_string!("Object"), ctx)
                .map_err(ext)?
                .as_object()
                .ok_or_else(|| InteropError::invariant("global Object is not an object"))?
                .clone();

            let keys_fn = object_ctor
                .get(js_string!("keys"), ctx)
                .map_err(ext)?
                .as_object()
                .ok_or_else(|| InteropError::invariant("Object.keys is not a function"))?
                .clone();

            let keys_val = keys_fn
                .call(&JsValue::undefined(), &[JsValue::from(obj.clone())], ctx)
                .map_err(ext)?;

            let keys_arr = JsArray::from_object(
                keys_val
                    .as_object()
                    .ok_or_else(|| InteropError::invariant("Object.keys did not return an object"))?
                    .clone(),
            )
            .map_err(|_| InteropError::invariant("Object.keys did not return an array"))?;

            let len = keys_arr.length(ctx).map_err(ext)? as usize;
            let keys_obj = keys_arr.deref().clone();
            let mut map = std::collections::HashMap::with_capacity(len);
            for i in 0..len {
                let key_val = keys_obj.get(i as i32, ctx).map_err(ext)?;
                let key_js = key_val.to_string(ctx).map_err(ext)?;
                let key = key_js.to_std_string_escaped();
                let val = obj.get(key_js, ctx).map_err(ext)?;
                map.insert(key, JsScriptValue::from_js(&val, ctx)?.0);
            }
            return Ok(Self(ScriptValue::Map(map)));
        }

        // Numbers
        if value.is_number() {
            let n = value.to_number(ctx).map_err(ext)?;
            if n.is_finite() {
                if n.fract() == 0.0 && n.abs() <= (1i64 << 53) as f64 {
                    return Ok(Self(ScriptValue::Integer(n as i64)));
                }
                return Ok(Self(ScriptValue::Float(n)));
            }
        }

        // Booleans
        if value.is_boolean() {
            return Ok(Self(ScriptValue::Bool(value.to_boolean())));
        }

        Err(InteropError::unsupported_operation(
            None,
            None,
            "Unsupported JsValue -> ScriptValue".to_owned(),
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
                // Wrap as a host object with the canonical ReflectReference prototype we installed.
                let proto = ctx
                    .global_object()
                    .get(js_string!("__bms_rr_proto"), ctx)
                    .ok()
                    .and_then(|v| v.as_object().cloned());
                let obj = JsObject::from_proto_and_data(proto, JsReflectReference::from(r));
                Ok(JsValue::from(obj))
            }

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
                        Ok(js)
                    })
                };
                let fobj = FunctionObjectBuilder::new(ctx.realm(), nf)
                    .name("hostFn")
                    .length(0)
                    .constructor(false)
                    .build();
                Ok(JsValue::from(fobj))
            }

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
                        Ok(js)
                    })
                };
                let fobj = FunctionObjectBuilder::new(ctx.realm(), nf)
                    .name("hostFn")
                    .length(0)
                    .constructor(false)
                    .build();
                Ok(JsValue::from(fobj))
            }

            ScriptValue::List(items) => {
                let mut js_items = Vec::with_capacity(items.len());
                for it in items {
                    js_items.push(JsScriptValue(it).into_js(ctx)?);
                }
                let arr = JsArray::from_iter(js_items.into_iter(), ctx);
                Ok(JsValue::from(arr))
            }

            ScriptValue::Map(map) => {
                // ⚠️ Avoid double-borrowing `ctx`: collect first, then build.
                let mut props: Vec<(JsString, JsValue)> = Vec::with_capacity(map.len());
                for (k, v) in map {
                    let key = JsString::from(k);
                    let val = JsScriptValue(v).into_js(ctx)?;
                    props.push((key, val));
                }
                let mut init = boa_engine::object::ObjectInitializer::new(ctx);
                for (key, val) in props {
                    init.property(key, val, Attribute::all());
                }
                Ok(JsValue::from(init.build()))
            }

            ScriptValue::Error(e) => Err(e),
        }
    }
}

#[allow(dead_code)]
fn eval_in_context(ctx: &mut Context, src: &str) -> Result<JsValue, InteropError> {
    ctx.eval(Source::from_bytes(src)).map_err(ext)
}
