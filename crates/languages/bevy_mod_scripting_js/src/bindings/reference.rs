use super::script_value::{FromJs, IntoJs, JsScriptValue, JS_CALLER_CONTEXT};
use bevy::ecs::world::World as BevyWorld;
use bevy_mod_scripting_core::{
    bindings::{
        function::script_function::DynamicScriptFunctionMut, script_value::ScriptValue,
        ReflectReference, ThreadWorldContainer, WorldContainer,
        query::{ScriptComponentRegistration, ScriptTypeRegistration}
    },
    error::InteropError,
};
use boa_engine::{
    error::JsNativeError, js_string, native_function::NativeFunction,
    object::{FunctionObjectBuilder, JsObject},
    property::{Attribute, PropertyKey},
    Context, JsResult, JsString, JsValue,
};
use boa_gc::{Finalize, Trace, Tracer};
use std::any::TypeId;

/// JavaScript wrapper for a dynamic Bevy reflect reference.
#[derive(Debug, Clone, PartialEq, Finalize, boa_engine::JsData)]
pub struct JsReflectReference(pub ReflectReference);

// SAFETY: opaque Rust data; no GC edges.
unsafe impl Trace for JsReflectReference {
    unsafe fn trace(&self, _tracer: &mut Tracer) {}
    unsafe fn trace_non_roots(&self) {}
    fn run_finalizer(&self) {}
}

impl From<ReflectReference> for JsReflectReference {
    fn from(v: ReflectReference) -> Self {
        Self(v)
    }
}
impl From<JsReflectReference> for ReflectReference {
    fn from(v: JsReflectReference) -> Self {
        v.0
    }
}

/// JavaScript wrapper for a type-only static reflect handle (TypeId).
#[derive(Debug, Clone, Copy, PartialEq, Finalize, boa_engine::JsData)]
pub struct JsStaticReflectReference(pub TypeId);

unsafe impl Trace for JsStaticReflectReference {
    unsafe fn trace(&self, _tracer: &mut Tracer) {}
    unsafe fn trace_non_roots(&self) {}
    fn run_finalizer(&self) {}
}

impl JsReflectReference {
    /// Install the `ReflectReference` constructor and prototype on `globalThis`.
    pub fn install_class(ctx: &mut Context) -> JsResult<()> {
        // Prototype with all instance methods.
        let proto = JsObject::with_null_proto();
        define_method(&proto, js_string!("get"), 1, rr_get, ctx)?;
        define_method(&proto, js_string!("set"), 2, rr_set, ctx)?;
        define_method(&proto, js_string!("call"), 1, rr_call, ctx)?;
        define_method(&proto, js_string!("method"), 1, rr_method, ctx)?;
        define_method(&proto, js_string!("len"), 0, rr_len, ctx)?;
        define_method(&proto, js_string!("toString"), 0, rr_to_string, ctx)?;
        define_symbol_iterator(&proto, rr_symbol_iterator, ctx)?;

        // Non-constructible ctor.
        let ctor = unsafe {
            NativeFunction::from_closure(|_this, _args, _ctx| {
                Err(JsNativeError::typ()
                    .with_message("ReflectReference cannot be constructed from JS")
                    .into())
            })
        };
        let ctor = FunctionObjectBuilder::new(ctx.realm(), ctor)
            .name("ReflectReference")
            .length(0)
            .build();

        // Attach prototype to the ctor.
        ctor.set(js_string!("prototype"), proto.clone(), false, ctx)
            .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

        let nf_set = NativeFunction::from_fn_ptr(rr_static_set);
        let set_fn = FunctionObjectBuilder::new(ctx.realm(), nf_set)
            .name("ref_set")
            .length(3)
            .constructor(false)
            .build();
        ctor.set(js_string!("ref_set"), set_fn, false, ctx)
            .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

        // Expose ctor on globalThis.
        ctx.register_global_property(
            PropertyKey::from(js_string!("ReflectReference")),
            JsValue::from(ctor),
            Attribute::all(),
        )
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

        // Stash the canonical prototype for host->JS wrapping (non-enumerable).
        ctx.register_global_property(
            PropertyKey::from(js_string!("__bms_rr_proto")),
            JsValue::from(proto),
            Attribute::empty(),
        )
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

        Ok(())
    }
}

impl JsStaticReflectReference {
    /// Install the `StaticReflectReference` constructor and prototype on `globalThis`.
    pub fn install_class(ctx: &mut Context) -> JsResult<()> {
        let proto = JsObject::with_null_proto();
        define_method(&proto, js_string!("fn"), 1, sref_fn, ctx)?; // returns callable
        define_method(&proto, js_string!("call"), 1, sref_call, ctx)?; // direct call(name, ...args)

        let ctor = unsafe {
            NativeFunction::from_closure(|_this, _args, _ctx| {
                Err(JsNativeError::typ()
                    .with_message("StaticReflectReference cannot be constructed from JS")
                    .into())
            })
        };

        let ctor = FunctionObjectBuilder::new(ctx.realm(), ctor)
            .name("StaticReflectReference")
            .length(0)
            .build();

        ctor.set(js_string!("prototype"), proto.clone(), false, ctx)
            .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

        ctx.register_global_property(
            PropertyKey::from(js_string!("StaticReflectReference")),
            JsValue::from(ctor),
            Attribute::all(),
        )
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

        Ok(())
    }
}

fn rr_this(this: &JsValue) -> JsResult<ReflectReference> {
    let obj = this.as_object().ok_or_else(|| {
        JsNativeError::typ().with_message("ReflectReference method called on non-object")
    })?;
    let inner = obj.downcast_ref::<JsReflectReference>().ok_or_else(|| {
        JsNativeError::typ().with_message("this is not a ReflectReference")
    })?;
    Ok(inner.0.clone())
}

fn rr_get(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let world = ThreadWorldContainer
        .try_get_world()
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    let self_ref = rr_this(this)?;

    let key_sv = JsScriptValue::from_js(args.get(0).unwrap_or(&JsValue::undefined()), ctx)
        .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?
        .0;

    let registry = world.script_function_registry();
    let registry = registry.read();
    let out = registry
        .magic_functions
        .get(JS_CALLER_CONTEXT, self_ref, key_sv)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    JsScriptValue(out)
        .into_js(ctx)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()).into())
}

fn rr_set(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let world = ThreadWorldContainer
        .try_get_world()
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    let self_ref = rr_this(this)?;

    let key_sv = JsScriptValue::from_js(args.get(0).unwrap_or(&JsValue::undefined()), ctx)
        .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?
        .0;
    let val_sv = JsScriptValue::from_js(args.get(1).unwrap_or(&JsValue::undefined()), ctx)
        .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?
        .0;

    let registry = world.script_function_registry();
    let registry = registry.read();
    registry
        .magic_functions
        .set(JS_CALLER_CONTEXT, self_ref, key_sv, val_sv)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    Ok(JsValue::undefined())
}

/// Methods on ScriptQueryBuilder that accept components.
#[inline]
fn is_qb_componentish_method(name: &str) -> bool {
    matches!(
        name,
        "component"
            | "components"
            | "with_component"
            | "with_components"
            | "without_component"
            | "without_components"
    )
}

/// Resolve strings/static tokens into a ScriptComponentRegistration reference when needed.
/// Also supports vectors of such (for `components`, `with_components`, `without_components`).
fn resolve_componentish_arg(
    world: bevy_mod_scripting_core::bindings::WorldGuard<'_>,
    sv: ScriptValue,
) -> Result<ScriptValue, bevy_mod_scripting_core::error::InteropError> {
    match sv {
        // List? normalize each element.
        ScriptValue::List(v) => {
            let mut out = Vec::with_capacity(v.len());
            for item in v {
                out.push(resolve_componentish_arg(world.clone(), item)?);
            }
            Ok(ScriptValue::List(out))
        }
        // Already a reference? If it's a ScriptTypeRegistration, try `.as_component()`.
        ScriptValue::Reference(rr) => {
            let tid_opt = rr.tail_type_id(world.clone())?;
            if tid_opt == Some(TypeId::of::<ScriptComponentRegistration>()) {
                return Ok(ScriptValue::Reference(rr));
            }
            if tid_opt == Some(TypeId::of::<ScriptTypeRegistration>()) {
                // Call the type-level converter: as_component(self)
                let out = world.try_call_overloads(
                    TypeId::of::<ScriptTypeRegistration>(),
                    "as_component",
                    vec![ScriptValue::Reference(rr.clone())],
                    JS_CALLER_CONTEXT,
                )?;
                // Ensure we actually got a component registration back.
                if let ScriptValue::Reference(cand) = &out {
                    if cand.tail_type_id(world.clone())?
                        == Some(TypeId::of::<ScriptComponentRegistration>())
                    {
                        return Ok(out);
                    }
                }
                return Err(InteropError::type_mismatch(
                    TypeId::of::<ScriptComponentRegistration>(),
                    tid_opt,
                ));
            }
            // Other refs: pass through.
            Ok(ScriptValue::Reference(rr))
        }
        // String name? Prefer a direct component lookup; fallback to type->as_component.
        ScriptValue::String(name) => {
            // 1) Try `World::get_component_by_name(string)`
            if let Ok(fun) =
                world.lookup_function([TypeId::of::<BevyWorld>()], "get_component_by_name")
            {
                let out = fun.call(vec![ScriptValue::String(name.clone())], JS_CALLER_CONTEXT)?;
                if let ScriptValue::Reference(rr) = &out {
                    if rr.tail_type_id(world.clone())?
                        == Some(TypeId::of::<ScriptComponentRegistration>())
                    {
                        return Ok(out);
                    }
                }
                // If it exists but returned the wrong thing, fall through to the type path.
            }

            // 2) Fallback: `World::get_type_by_name(string)` then `.as_component()`
            let get_type = match world.lookup_function(
                [TypeId::of::<BevyWorld>()],
                "get_type_by_name",
            ) {
                Ok(f) => f,
                Err(_e) => {
                    return Err(InteropError::missing_function(
                        TypeId::of::<BevyWorld>(),
                        "get_type_by_name",
                    ))
                }
            };

            let ty_out =
                get_type.call(vec![ScriptValue::String(name.clone())], JS_CALLER_CONTEXT)?;
            if let ScriptValue::Reference(rr) = ty_out {
                // If this is already a component reg, accept it.
                if rr.tail_type_id(world.clone())?
                    == Some(TypeId::of::<ScriptComponentRegistration>())
                {
                    return Ok(ScriptValue::Reference(rr));
                }
                // If it's a type reg, try to convert.
                if rr.tail_type_id(world.clone())?
                    == Some(TypeId::of::<ScriptTypeRegistration>())
                {
                    let out = world.try_call_overloads(
                        TypeId::of::<ScriptTypeRegistration>(),
                        "as_component",
                        vec![ScriptValue::Reference(rr)],
                        JS_CALLER_CONTEXT,
                    )?;
                    if let ScriptValue::Reference(cand) = &out {
                        if cand.tail_type_id(world.clone())?
                            == Some(TypeId::of::<ScriptComponentRegistration>())
                        {
                            return Ok(out);
                        }
                    }
                }
            }

            Err(InteropError::type_mismatch(
                TypeId::of::<ScriptComponentRegistration>(),
                None,
            ))
        }
        // Anything else: leave as-is.
        other => Ok(other),
    }
}

fn rr_call(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let world = ThreadWorldContainer
        .try_get_world()
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    let self_ref = rr_this(this)?;

    let Some(name_js) = args.get(0).and_then(|v| v.as_string()) else {
        return Err(JsNativeError::typ()
            .with_message("call(name, ...args): name must be a string")
            .into());
    };
    let name = name_js.to_std_string_escaped();

    // Convert JS args -> ScriptValue (no pre-coercion)
    let mut sv_args = Vec::with_capacity(args.len().saturating_sub(1));
    for v in &args[1..] {
        sv_args.push(
            JsScriptValue::from_js(v, ctx)
                .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?
                .0,
        );
    }

    // Normalize component-ish args for query-builder APIs.
    if is_qb_componentish_method(&name) {
        for a in &mut sv_args {
            *a = resolve_componentish_arg(world.clone(), std::mem::take(a))
                .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
        }
    }

    let target_type_id = self_ref.base.type_id();

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

fn rr_method(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let self_ref = rr_this(this)?;

    let Some(name_js) = args.get(0).and_then(|v| v.as_string()) else {
        return Err(
            JsNativeError::typ()
                .with_message("method(name): name must be a string")
                .into(),
        );
    };
    let name = name_js.to_std_string_escaped();

    let target_type_id = self_ref.base.type_id();
    let recv = self_ref.clone();
    let name_owned = name.clone();

    let nf = unsafe {
        NativeFunction::from_closure(move |_this, js_args, js_ctx| {
            // Convert only — no pre-coercion
            let mut sv_args = Vec::with_capacity(js_args.len());
            for v in js_args {
                let sv = JsScriptValue::from_js(v, js_ctx)
                    .map_err(|e| JsNativeError::error().with_message(e.to_string()))?
                    .0;
                sv_args.push(sv);
            }

            // Normalize component-ish args when this is a QB method.
            if is_qb_componentish_method(&name_owned) {
                let w = ThreadWorldContainer
                    .try_get_world()
                    .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
                for a in &mut sv_args {
                    *a = resolve_componentish_arg(w.clone(), std::mem::take(a))
                        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
                }
            }

            let mut all = Vec::with_capacity(1 + sv_args.len());
            all.push(ScriptValue::Reference(recv.clone()));
            all.extend(sv_args);

            let out = ThreadWorldContainer
                .try_get_world()
                .map_err(|e| JsNativeError::error().with_message(e.to_string()))?
                .try_call_overloads(target_type_id, name_owned.clone(), all, JS_CALLER_CONTEXT)
                .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

            JsScriptValue(out)
                .into_js(js_ctx)
                .map_err(|e| JsNativeError::error().with_message(e.to_string()).into())
        })
    };

    let fobj = FunctionObjectBuilder::new(ctx.realm(), nf)
        .name(JsString::from(format!("method:{}", name)))
        .length(0)
        .constructor(false)
        .build();

    Ok(JsValue::from(fobj))
}

fn rr_len(this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let world = ThreadWorldContainer
        .try_get_world()
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    let self_ref = rr_this(this)?;
    let len = self_ref
        .len(world)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    Ok(match len {
        Some(n) => JsValue::from(n as i32),
        None => JsValue::undefined(),
    })
}

fn rr_to_string(this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let world = ThreadWorldContainer
        .try_get_world()
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    let self_ref = rr_this(this)?;

    let func = world.lookup_function([TypeId::of::<ReflectReference>()], "display_ref").map_err(
        |f| {
            JsNativeError::error()
                .with_message(format!("missing function display_ref: {f}"))
        },
    )?;

    let out = func
        .call(vec![ScriptValue::Reference(self_ref)], JS_CALLER_CONTEXT)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    JsScriptValue(out)
        .into_js(ctx)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()).into())
}

/// `[Symbol.iterator]` implementation: returns `{ next() { ... } }` object.
fn rr_symbol_iterator(this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let world = ThreadWorldContainer
        .try_get_world()
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    let self_ref = rr_this(this)?;

    // Resolve the `iter` function that returns a stateful next-function.
    let iter_func = world.lookup_function([TypeId::of::<ReflectReference>()], "iter").map_err(
        |f| JsNativeError::error().with_message(format!("missing function iter: {f}")),
    )?;

    let next_sv = iter_func
        .call(vec![ScriptValue::Reference(self_ref)], JS_CALLER_CONTEXT)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    let next_fn: DynamicScriptFunctionMut = match next_sv {
        ScriptValue::FunctionMut(f) => f,
        _ => {
            return Err(
                JsNativeError::error()
                    .with_message("iter did not return a function")
                    .into(),
            )
        }
    };

    // Build iterator object with a `next()` method that calls `next_fn` until it returns Unit.
    let nf = unsafe {
        NativeFunction::from_closure(move |_this, _args, js_ctx| {
            let out = next_fn
                .call(vec![], JS_CALLER_CONTEXT)
                .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

            let result = JsObject::with_null_proto();
            match out {
                ScriptValue::Unit => {
                    result
                        .set(js_string!("done"), true, false, js_ctx)
                        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
                }
                v => {
                    let val_js = JsScriptValue(v)
                        .into_js(js_ctx)
                        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
                    result
                        .set(js_string!("value"), val_js, false, js_ctx)
                        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
                    result
                        .set(js_string!("done"), false, false, js_ctx)
                        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
                }
            }
            Ok(JsValue::from(result))
        })
    };

    let iter_obj = JsObject::with_null_proto();
    let next = FunctionObjectBuilder::new(ctx.realm(), nf)
        .name("next")
        .length(0)
        .constructor(false)
        .build();
    iter_obj
        .set(js_string!("next"), next, false, ctx)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    Ok(JsValue::from(iter_obj))
}

fn sref_this(this: &JsValue) -> JsResult<TypeId> {
    let obj = this.as_object().ok_or_else(|| {
        JsNativeError::typ().with_message("StaticReflectReference method called on non-object")
    })?;
    let inner = obj
        .downcast_ref::<JsStaticReflectReference>()
        .ok_or_else(|| {
            JsNativeError::typ().with_message("this is not a StaticReflectReference")
        })?;
    Ok(inner.0)
}

/// Return a callable for a specific static function name.
fn sref_fn(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let type_id = sref_this(this)?;

    let Some(name_js) = args.get(0).and_then(|v| v.as_string()) else {
        return Err(
            JsNativeError::typ()
                .with_message("fn(name): name must be a string")
                .into(),
        );
    };

    // Use two copies: one captured by the closure, one for the function object's display name.
    let name_str = name_js.to_std_string_escaped();
    let name_for_closure = name_str.clone();

    // Callable that dispatches to the registered overloads on each invocation.
    let nf = unsafe {
        NativeFunction::from_closure(move |_this, js_args, js_ctx| {
            let mut sv_args = Vec::with_capacity(js_args.len());
            for v in js_args {
                let sv = JsScriptValue::from_js(v, js_ctx)
                    .map_err(|e| JsNativeError::error().with_message(e.to_string()))?
                    .0;
                sv_args.push(sv);
            }

            let out = ThreadWorldContainer
                .try_get_world()
                .map_err(|e| JsNativeError::error().with_message(e.to_string()))?
                .try_call_overloads(type_id, name_for_closure.clone(), sv_args, JS_CALLER_CONTEXT)
                .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

            JsScriptValue(out)
                .into_js(js_ctx)
                .map_err(|e| JsNativeError::error().with_message(e.to_string()).into())
        })
    };

    let fobj = FunctionObjectBuilder::new(ctx.realm(), nf)
        .name(JsString::from(format!("{}.fn", name_str)))
        .length(0)
        .constructor(false)
        .build();

    Ok(JsValue::from(fobj))
}

/// Convenience: direct call with name and args without creating a bound function.
fn sref_call(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let world = ThreadWorldContainer
        .try_get_world()
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    let type_id = sref_this(this)?;

    let Some(name_js) = args.get(0).and_then(|v| v.as_string()) else {
        return Err(
            JsNativeError::typ()
                .with_message("call(name, ...args): name must be a string")
                .into(),
        );
    };
    let name = name_js.to_std_string_escaped();

    let mut sv_args = Vec::with_capacity(args.len().saturating_sub(1));
    for v in &args[1..] {
        sv_args.push(
            JsScriptValue::from_js(v, ctx)
                .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?
                .0,
        );
    }

    let out = world
        .try_call_overloads(type_id, name, sv_args, JS_CALLER_CONTEXT)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    JsScriptValue(out)
        .into_js(ctx)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()).into())
}

fn define_method(
    proto: &JsObject,
    name: JsString,
    _length: usize,
    f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
    ctx: &mut Context,
) -> JsResult<()> {
    let nf = NativeFunction::from_fn_ptr(f);
    let fun = FunctionObjectBuilder::new(ctx.realm(), nf)
        .name(name.clone())
        .length(0)
        .constructor(false)
        .build();
    proto
        .set(name, fun, false, ctx)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    Ok(())
}

fn arg_as_rr(arg: Option<&JsValue>) -> JsResult<ReflectReference> {
    let default_val = JsValue::undefined(); // bind so it lives long enough
    let v = arg.unwrap_or(&default_val);
    let obj = v
        .as_object()
        .ok_or_else(|| JsNativeError::typ().with_message("expected ReflectReference object as first argument"))?;
    let inner = obj
        .downcast_ref::<JsReflectReference>()
        .ok_or_else(|| JsNativeError::typ().with_message("first argument is not a ReflectReference"))?;
    Ok(inner.0.clone())
}

fn rr_static_set(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let world = ThreadWorldContainer
        .try_get_world()
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    let self_ref = arg_as_rr(args.get(0))?;

    let key_sv = JsScriptValue::from_js(args.get(1).unwrap_or(&JsValue::undefined()), ctx)
        .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?
        .0;

    let val_sv = JsScriptValue::from_js(args.get(2).unwrap_or(&JsValue::undefined()), ctx)
        .map_err(|e| JsNativeError::typ().with_message(e.to_string()))?
        .0;

    let registry = world.script_function_registry();
    let registry = registry.read();
    registry
        .magic_functions
        .set(JS_CALLER_CONTEXT, self_ref, key_sv, val_sv)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    Ok(JsValue::undefined())
}

fn define_symbol_iterator(
    proto: &JsObject,
    f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
    ctx: &mut Context,
) -> JsResult<()> {
    let nf = NativeFunction::from_fn_ptr(f);
    let fun = FunctionObjectBuilder::new(ctx.realm(), nf)
        .name(js_string!("[Symbol.iterator]"))
        .length(0)
        .constructor(false)
        .build();

    // Resolve Symbol.iterator
    let symbol_ctor = ctx.global_object()
        .get(js_string!("Symbol"), ctx)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?
        .as_object()
        .ok_or_else(|| JsNativeError::error().with_message("global Symbol is not an object"))?
        .clone();

    let iterator_val = symbol_ctor
        .get(js_string!("iterator"), ctx)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    let iterator_sym = iterator_val
        .as_symbol()
        .ok_or_else(|| JsNativeError::error().with_message("Symbol.iterator is not a symbol"))?
        .clone();

    proto
        .set(PropertyKey::from(iterator_sym), fun, false, ctx)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;

    Ok(())
}
