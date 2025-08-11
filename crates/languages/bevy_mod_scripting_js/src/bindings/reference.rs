use super::script_value::{JsScriptValue, JS_CALLER_CONTEXT, FromJs, IntoJs};
use bevy_mod_scripting_core::{
    bindings::{script_value::ScriptValue, ReflectReference, ThreadWorldContainer, WorldContainer},
    reflection_extensions::TypeIdExtensions,
};
use std::any::TypeId;
use boa_engine::{
    class::{Class, ClassBuilder},
    error::JsNativeError,
    js_string,
    native_function::NativeFunction,
    Context, JsData, JsResult, JsValue,
};
use boa_gc::{Finalize, Trace, Tracer};

/// JavaScript wrapper for [`bevy_mod_scripting_core::bindings::ReflectReference`].
///
/// This object provides a reflection interface from JS to Bevy values. Any Rust type
/// registered in the type registry can be accessed and manipulated via this handle.
#[derive(Debug, Clone, PartialEq, Finalize, JsData)]
pub struct JsReflectReference(pub ReflectReference);

// SAFETY: `ReflectReference` is a pure Rust type; no Boa GC edges live here.
unsafe impl Trace for JsReflectReference {
    unsafe fn trace(&self, _tracer: &mut Tracer) {}
    unsafe fn trace_non_roots(&self) {}
    fn run_finalizer(&self) {}
}

impl From<ReflectReference> for JsReflectReference {
    fn from(v: ReflectReference) -> Self { Self(v) }
}
impl From<JsReflectReference> for ReflectReference {
    fn from(v: JsReflectReference) -> Self { v.0.clone() }
}

/// JavaScript wrapper that represents a *type-only* handle via a Rust [`TypeId`].
///
/// Used to expose “static” calls in JS (e.g., making `Entity.from_raw(x)` available
/// as `Entity.from_raw(x)` by binding a global `StaticReflectReference(TypeId::of::<Entity>())`).
#[derive(Debug, Clone, PartialEq, Finalize, JsData)]
pub struct JsStaticReflectReference(pub TypeId);

// SAFETY: `TypeId` is pure Rust; no Boa GC edges here either.
unsafe impl Trace for JsStaticReflectReference {
    unsafe fn trace(&self, _tracer: &mut Tracer) {}
    unsafe fn trace_non_roots(&self) {}
    fn run_finalizer(&self) {}
}

impl Class for JsReflectReference {
    const NAME: &'static str = "ReflectReference";

    fn data_constructor(_new_target: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ().with_message("ReflectReference cannot be constructed from JS").into())
    }

    fn init(class: &mut ClassBuilder) -> JsResult<()> {
        // Function-only API:
        class.static_method(js_string!("ref_get"), 2, NativeFunction::from_fn_ptr(ref_get));
        class.static_method(js_string!("ref_set"), 3, NativeFunction::from_fn_ptr(ref_set));
        class.static_method(js_string!("ref_call"), 2, NativeFunction::from_fn_ptr(ref_call));

        class.static_method(js_string!("ref_add"), 2, binop("add"));
        class.static_method(js_string!("ref_sub"), 2, binop("sub"));
        class.static_method(js_string!("ref_mul"), 2, binop("mul"));
        class.static_method(js_string!("ref_div"), 2, binop("div"));
        class.static_method(js_string!("ref_rem"), 2, binop("rem"));
        class.static_method(js_string!("ref_pow"), 2, binop("pow"));
        class.static_method(js_string!("ref_eq"),  2, binop("eq"));
        class.static_method(js_string!("ref_lt"),  2, binop("lt"));

        class.static_method(js_string!("ref_len"), 1, NativeFunction::from_fn_ptr(ref_len));
        Ok(())
    }
}
impl Class for JsStaticReflectReference {
    const NAME: &'static str = "StaticReflectReference";

    fn data_constructor(_n: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<Self> {
        Err(JsNativeError::typ()
            .with_message("StaticReflectReference cannot be constructed from JS")
            .into())
    }

    fn init(class: &mut ClassBuilder) -> JsResult<()> {
        // One clear entrypoint for static/type-level calls:
        class.static_method(js_string!("call"), 3, NativeFunction::from_fn_ptr(static_ref_call));
        Ok(())
    }
}

// —— helpers —— //

fn js_type_err<T>(msg: &str) -> JsResult<T> {
    Err(JsNativeError::typ().with_message(msg.to_string()).into())
}

// helper to extract a JsStaticReflectReference from a JS value
fn first_arg_as_static_ref(args: &[JsValue]) -> JsResult<JsStaticReflectReference> {
    let Some(obj) = args.get(0).and_then(|v| v.as_object()) else {
        return Err(JsNativeError::typ()
            .with_message("StaticReflectReference.call: first argument must be a StaticReflectReference object")
            .into());
    };
    if let Some(inner) = obj.downcast_ref::<JsStaticReflectReference>() {
        Ok(JsStaticReflectReference(inner.0)) // TypeId is Copy; rebuild wrapper
    } else {
        Err(JsNativeError::typ()
            .with_message("StaticReflectReference.call: expected StaticReflectReference object")
            .into())
    }
}

fn first_arg_as_ref(args: &[JsValue]) -> JsResult<ReflectReference> {
    let Some(obj) = args.get(0).and_then(|v| v.as_object()) else {
        return js_type_err("expected object as first argument");
    };
    if let Some(inner) = obj.downcast_ref::<JsReflectReference>() {
        Ok(inner.0.clone())
    } else {
        js_type_err("expected ReflectReference object")
    }
}
/// todo
pub(crate) fn ref_get(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let world = ThreadWorldContainer
        .try_get_world()
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    let self_ref = first_arg_as_ref(args)?;

    let key_sv = JsScriptValue::from_js(args.get(1).unwrap_or(&JsValue::undefined()), ctx)
        .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?
        .0;

    let out = {
        let registry = world.script_function_registry();
        let registry = registry.read();
        registry
            .magic_functions
            .get(JS_CALLER_CONTEXT, self_ref, key_sv)
            .map_err(|e| JsNativeError::error().with_message(e.to_string()))?
    };

    JsScriptValue(out)
        .into_js(ctx)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()).into())
}
/// todo
pub(crate) fn ref_set(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let world = ThreadWorldContainer
        .try_get_world()
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    let self_ref = first_arg_as_ref(args)?;

    let key_sv = JsScriptValue::from_js(args.get(1).unwrap_or(&JsValue::undefined()), ctx)
        .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?
        .0;
    let val_sv = JsScriptValue::from_js(args.get(2).unwrap_or(&JsValue::undefined()), ctx)
        .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?
        .0;

    {
        let registry = world.script_function_registry();
        let registry = registry.read();
        registry
            .magic_functions
            .set(JS_CALLER_CONTEXT, self_ref, key_sv, val_sv)
            .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    }

    Ok(JsValue::undefined())
}
/// todo
pub(crate) fn ref_call(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let world = ThreadWorldContainer
        .try_get_world()
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    let self_ref = first_arg_as_ref(args)?;

    let Some(name_js) = args.get(1).and_then(|v| v.as_string()) else {
        return js_type_err("ref_call: second argument must be a string name");
    };
    let name = name_js.to_std_string_escaped();

    let mut sv_args = Vec::new();
    for v in &args[2..] {
        let sv = JsScriptValue::from_js(v, ctx)
            .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?
            .0;
        sv_args.push(sv);
    }

    let target_type_id = self_ref
        .tail_type_id(world.clone())
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?
        .or_fake_id();

    let mut full_args = Vec::with_capacity(1 + sv_args.len());
    full_args.push(ScriptValue::Reference(self_ref));
    full_args.extend(sv_args);

    let out = world
        .try_call_overloads(target_type_id, name, full_args, JS_CALLER_CONTEXT)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    JsScriptValue(out)
        .into_js(ctx)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()).into())
}

fn static_ref_call(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let world = ThreadWorldContainer
        .try_get_world()
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    let sref = first_arg_as_static_ref(args)?; // returns JsStaticReflectReference
    let Some(name_js) = args.get(1).and_then(|v| v.as_string()) else {
        return Err(JsNativeError::typ()
            .with_message("StaticReflectReference.call: second argument must be a string name")
            .into());
    };
    let name = name_js.to_std_string_escaped();

    // Convert remaining args JS -> ScriptValue
    let mut sv_args = Vec::with_capacity(args.len().saturating_sub(2));
    for v in &args[2..] {
        sv_args.push(
            JsScriptValue::from_js(v, ctx)
                .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?
                .0,
        );
    }

    // Dispatch to your registered overloads for the target type
    let out = world
        .try_call_overloads(sref.0, name, sv_args, JS_CALLER_CONTEXT)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    JsScriptValue(out)
        .into_js(ctx)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()).into())
}

fn binop(op: &'static str) -> NativeFunction {
    // SAFETY: Boa’s `from_closure` is marked unsafe; we ensure the closure
    // does not capture any non-`'static` GC-managed references.
    unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let world = ThreadWorldContainer
                .try_get_world()
                .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
            let self_ref = first_arg_as_ref(args)?;

            let other = JsScriptValue::from_js(args.get(1).unwrap_or(&JsValue::undefined()), ctx)
                .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?
                .0;

            let target_type_id = self_ref
                .tail_type_id(world.clone())
                .map_err(|e| JsNativeError::error().with_message(e.to_string()))?
                .or_fake_id();

            let out = world
                .try_call_overloads(
                    target_type_id,
                    op,
                    vec![ScriptValue::Reference(self_ref), other],
                    JS_CALLER_CONTEXT,
                )
                .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

            JsScriptValue(out)
                .into_js(ctx)
                .map_err(|e| JsNativeError::error().with_message(e.to_string()).into())
        })
    }
}

/// todo
pub(crate) fn ref_len(_this: &JsValue, args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let world = ThreadWorldContainer
        .try_get_world()
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    let self_ref = first_arg_as_ref(args)?;

    let len = self_ref
        .len(world)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    Ok(match len {
        Some(n) => JsValue::from(n as i32),
        None => JsValue::undefined(),
    })
}
