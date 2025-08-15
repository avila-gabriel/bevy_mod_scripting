//! JavaScript (Boa) integration for the bevy_mod_scripting system.
use bevy::{
    app::Plugin,
    ecs::world::World,
};
use bevy_mod_scripting_core::{
    asset::Language,
    bindings::{
        function::namespace::Namespace, globals::AppScriptGlobalsRegistry,
        script_value::ScriptValue, ThreadWorldContainer, WorldContainer,
    },
    context::{ContextBuilder, ContextInitializer, ContextPreHandlingInitializer},
    error::{InteropError, ScriptError},
    reflection_extensions::PartialReflectExt,
    runtime::RuntimeSettings,
    script::ScriptId,
    IntoScriptPluginParams, ScriptingPlugin,
};

pub use boa_engine;
use boa_engine::{
    js_string,
    property::{Attribute, PropertyKey},
    Context, JsObject, JsString, JsValue, Source,
};

use bindings::{
    reference::{JsReflectReference, JsStaticReflectReference},
    script_value::{with_js_context, FromJs, IntoJs, JsScriptValue},
};

/// Bindings for JS (host objects, conversions, etc.).
pub mod bindings;

/// Per-app registry of per-script Boa VMs (NonSend).
#[derive(Default)]
struct JsVmRegistry {
    vms: std::collections::HashMap<String, boa_engine::Context>,
}

#[inline]
fn sid_key(id: &ScriptId) -> String {
    id.as_ref().to_string()
}

/// The JS scripting plugin. Adds JavaScript scripting using Boa inside BMS.
pub struct JsScriptingPlugin {
    /// The internal scripting plugin wiring.
    pub scripting_plugin: ScriptingPlugin<Self>,
}

impl Default for JsScriptingPlugin {
    fn default() -> Self {
        JsScriptingPlugin {
            scripting_plugin: ScriptingPlugin {
                context_assignment_strategy: Default::default(),
                runtime_settings: RuntimeSettings::default(),
                callback_handler: js_handler,
                context_builder: ContextBuilder::<JsScriptingPlugin> {
                    load: js_context_load,
                    reload: js_context_reload,
                },
                // We do all Boa registration/mirroring inside load/reload/handler.
                context_initializers: vec![],
                context_pre_handling_initializers: vec![],
                additional_supported_extensions: &["js"],
                language: Language::Js,
            },
        }
    }
}

impl IntoScriptPluginParams for JsScriptingPlugin {
    type C = (); // unit — not Boa::Context (Boa Context is not Send/Sync)
    type R = ();
    const LANGUAGE: Language = Language::Js;

    fn build_runtime() -> Self::R {}
}

// necessary for automatic config goodies
impl AsMut<ScriptingPlugin<Self>> for JsScriptingPlugin {
    fn as_mut(&mut self) -> &mut ScriptingPlugin<JsScriptingPlugin> {
        &mut self.scripting_plugin
    }
}

impl Plugin for JsScriptingPlugin {
    fn build(&self, app: &mut bevy::prelude::App) {
        self.scripting_plugin.build(app);
        // Where we keep per-script VMs. NonSend because Boa is single-threaded.
        app.insert_non_send_resource(JsVmRegistry::default());
    }

    fn finish(&self, app: &mut bevy::app::App) {
        self.scripting_plugin.finish(app);
    }
}

#[profiling::function]
/// Load a JS context from a script (fresh VM).
pub fn js_context_load(
    script_id: &ScriptId,
    content: &[u8],
    _initializers: &[ContextInitializer<JsScriptingPlugin>],
    _pre: &[ContextPreHandlingInitializer<JsScriptingPlugin>],
    _: &(),
) -> Result<(), ScriptError> {
    // Create a fresh Boa VM for this script.
    let mut ctx = Context::default();

    // Ensure host classes/types are registered first.
    register_host_classes(&mut ctx)?;

    // Mirror globals from the app into this VM, bind `world`, etc.
    mirror_app_globals_into_vm(&mut ctx)?;

    // Evaluate the (Gleam-compiled) JS payload.
    ctx.eval(Source::from_bytes(content))
        .map_err(|e| ScriptError::new(InteropError::invariant(format!("eval: {e}"))))?;

    // Stash VM into NonSend resource. We must go through world access to touch NonSend.
    let mut ctx_opt = Some(ctx);
    let wc = ThreadWorldContainer.try_get_world()?;
    let _ = wc.with_global_access(|world| {
        if world.get_non_send_resource::<JsVmRegistry>().is_none() {
            world.insert_non_send_resource(JsVmRegistry::default());
        }
        let mut reg = world.get_non_send_resource_mut::<JsVmRegistry>().unwrap();
        reg.vms
            .insert(sid_key(script_id), ctx_opt.take().expect("ctx present"));
        Ok::<(), InteropError>(())
    })
    .map_err(ScriptError::new)?;

    Ok(())
}

#[profiling::function]
/// Reload an existing JS context from a script (in-place, like Lua).
pub fn js_context_reload(
    script_id: &ScriptId,
    content: &[u8],
    _old_ctx_unit: &mut (),
    _initializers: &[ContextInitializer<JsScriptingPlugin>],
    _pre: &[ContextPreHandlingInitializer<JsScriptingPlugin>],
    _: &(),
) -> Result<(), ScriptError> {
    // Fetch the existing VM and eval the new source.
    let wc = ThreadWorldContainer.try_get_world()?;

    wc.with_global_access(|world| -> Result<(), ScriptError> {
        let mut reg = world
            .get_non_send_resource_mut::<JsVmRegistry>()
            .ok_or_else(|| ScriptError::new(InteropError::invariant("JsVmRegistry missing")))?;
        let ctx: &mut Context = reg
            .vms
            .get_mut(&sid_key(script_id))
            .ok_or_else(|| ScriptError::new(InteropError::invariant("no VM for this ScriptId")))?;

        // Make sure host classes still exist (safe to re-run).
        register_host_classes(ctx)?;

        ctx.eval(Source::from_bytes(content))
            .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?;
        Ok(())
    })
    .map_err(ScriptError::new)??;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[profiling::function]
/// The JS handler for events (host → script).
pub fn js_handler(
    args: Vec<ScriptValue>,
    entity: bevy::ecs::entity::Entity,
    script_id: &bevy_mod_scripting_core::script::ScriptId,
    callback_label: &bevy_mod_scripting_core::event::CallbackLabel,
    _ctx_unit: &mut (),
    _pre: &[bevy_mod_scripting_core::context::ContextPreHandlingInitializer<JsScriptingPlugin>],
    _: &(),
) -> Result<ScriptValue, bevy_mod_scripting_core::error::ScriptError> {
    let wc = ThreadWorldContainer.try_get_world()?;
    let key = script_id.as_ref().to_string();

    // --- Phase A: take the VM out of the registry (so we can drop the &mut World borrow) ---
    let mut ctx = wc.with_global_access(|world| -> Result<boa_engine::Context, ScriptError> {
        let mut reg = world
            .get_non_send_resource_mut::<JsVmRegistry>()
            .ok_or_else(|| ScriptError::new(InteropError::invariant("JsVmRegistry missing")))?;
        reg.vms
            .remove(&key)
            .ok_or_else(|| ScriptError::new(InteropError::invariant("no VM for this ScriptId")))
    }).map_err(ScriptError::new)??;

    // --- Phase B: run the callback with NO world borrow held ---
    let call_result = {
        with_js_context(&mut ctx, |ctx| {
            // Per-call globals: `entity` (reflect ref) + `script_id` (string)
            // Build JS host object for the entity reflect reference.
            let ctor = ctx
                .global_object()
                .get(js_string!("ReflectReference"), ctx)
                .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?
                .as_object()
                .cloned()
                .ok_or_else(|| ScriptError::new(InteropError::invariant("ReflectReference ctor missing")))?;

            let proto = ctor
                .get(js_string!("prototype"), ctx)
                .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?
                .as_object()
                .cloned();

            let reflect = JsReflectReference(<bevy::ecs::entity::Entity>::allocate(Box::new(entity), wc.clone()));
            let entity_obj = JsObject::from_proto_and_data(proto, reflect);

            ctx.register_global_property(
                PropertyKey::from(js_string!("entity")),
                JsValue::from(entity_obj),
                Attribute::all(),
            )
            .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?;

            let id_str: &str = script_id.as_ref();
            ctx.register_global_property(
                PropertyKey::from(js_string!("script_id")),
                JsValue::from(JsString::from(id_str)),
                Attribute::all(),
            )
            .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?;

            // Find the handler function on globalThis.
            let lbl_str: &str = callback_label.as_ref();
            let fun_val = match ctx.global_object().get(JsString::from(lbl_str), ctx) {
                Ok(v) => v,
                Err(_) => {
                    bevy::log::trace!(
                        "Script {} is not subscribed to callback {}",
                        script_id,
                        callback_label.as_ref()
                    );
                    return Ok(ScriptValue::Unit);
                }
            };

            let Some(fun_obj) = fun_val.as_object() else {
                bevy::log::trace!("Callback {} is not a function", callback_label.as_ref());
                return Ok(ScriptValue::Unit);
            };

            // Convert args -> JsValue
            let mut js_args = Vec::with_capacity(args.len());
            for a in args {
                js_args.push(JsScriptValue(a).into_js(ctx).map_err(ScriptError::new)?);
            }

            // Call and convert result back.
            let out = fun_obj
                .call(&JsValue::undefined(), &js_args, ctx)
                .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?;
            let sv = JsScriptValue::from_js(&out, ctx).map_err(ScriptError::new)?.0;
            Ok(sv)
        })
    };

    // --- Phase C: put the VM back into the registry ---
    wc.with_global_access(|world| -> Result<(), ScriptError> {
        let mut reg = world
            .get_non_send_resource_mut::<JsVmRegistry>()
            .ok_or_else(|| ScriptError::new(InteropError::invariant("JsVmRegistry missing (post-call)")))?;
        let prev = reg.vms.insert(key, ctx);
        debug_assert!(prev.is_none(), "VM should have been removed before call");
        Ok(())
    }).map_err(ScriptError::new)??;

    call_result
}
/// Ensure the host-side classes/constructors are available in the global object.
fn register_host_classes(ctx: &mut Context) -> Result<(), ScriptError> {
    // Dynamic reflect reference host object.
    JsReflectReference::install_class(ctx).map_err(|e| {
        ScriptError::new(InteropError::invariant(format!(
            "install ReflectReference: {e}"
        )))
    })?;

    // Static (type-only) reflect reference host object.
    JsStaticReflectReference::install_class(ctx).map_err(|e| {
        ScriptError::new(InteropError::invariant(format!(
            "install StaticReflectReference: {e}"
        )))
    })?;

    // Optional alias for ergonomics.
    if let Ok(rr_ctor) = ctx.global_object().get(js_string!("ReflectReference"), ctx) {
        if let Some(rr) = rr_ctor.as_object() {
            ctx.register_global_property(
                PropertyKey::from(js_string!("ReflectRef")),
                JsValue::from(rr.clone()),
                Attribute::all(),
            )
            .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?;
        }
    }

    Ok(())
}

/// Mirror app-level globals (dynamic makers and static TypeId bindings) into a Boa VM.
/// Also binds `world` as a static reference.
fn mirror_app_globals_into_vm(ctx: &mut Context) -> Result<(), ScriptError> {
    let wc = ThreadWorldContainer.try_get_world()?;

    // Bind `world` = StaticReflectReference(TypeId::<World>())
    {
        // Ensure StaticReflectReference is registered (should be from install_class).
        let ctor = ctx
            .global_object()
            .get(js_string!("StaticReflectReference"), ctx)
            .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?
            .as_object()
            .cloned()
            .ok_or_else(|| {
                ScriptError::new(InteropError::invariant(
                    "StaticReflectReference ctor missing",
                ))
            })?;

        let proto = ctor
            .get(js_string!("prototype"), ctx)
            .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?
            .as_object()
            .cloned();

        let world_ref =
            JsObject::from_proto_and_data(proto, JsStaticReflectReference(std::any::TypeId::of::<World>()));

        ctx.register_global_property(
            PropertyKey::from(js_string!("world")),
            JsValue::from(world_ref),
            Attribute::all(),
        )
        .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?;
    }

    // App globals
    let globals_registry =
        wc.with_resource(|r: &AppScriptGlobalsRegistry| r.clone())?;
    let globals_registry = globals_registry.read();

    for (key, global) in globals_registry.iter() {
        match &global.maker {
            Some(maker) => {
                // dynamic
                let sv = (maker)(wc.clone())?;
                let js_val = JsScriptValue::from(sv).into_js(ctx)?;
                // make the &str type explicit to avoid inference ambiguity
                let key_str: &str = key.as_ref();
                ctx.register_global_property(
                    PropertyKey::from(JsString::from(key_str)),
                    js_val,
                    Attribute::all(),
                )
                .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?;
            }
            None => {
                // static -> StaticReflectReference(type_id)
                let ctor = ctx
                    .global_object()
                    .get(js_string!("StaticReflectReference"), ctx)
                    .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?
                    .as_object()
                    .cloned()
                    .ok_or_else(|| {
                        ScriptError::new(InteropError::invariant(
                            "StaticReflectReference ctor missing",
                        ))
                    })?;

                let proto = ctor
                    .get(js_string!("prototype"), ctx)
                    .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?
                    .as_object()
                    .cloned();

                let obj = JsObject::from_proto_and_data(
                    proto,
                    JsStaticReflectReference(global.type_id),
                );

                let key_str: &str = key.as_ref();
                ctx.register_global_property(
                    PropertyKey::from(JsString::from(key_str)),
                    JsValue::from(obj),
                    Attribute::all(),
                )
                .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?;
            }
        }
    }

    // Global namespace script functions
    let script_fn_registry = wc.script_function_registry();
    let script_fn_registry = script_fn_registry.read();

    for (key, function) in script_fn_registry
        .iter_all()
        .filter(|(k, _)| k.namespace == Namespace::Global)
    {
        let js_fun =
            JsScriptValue::from(ScriptValue::Function(function.clone())).into_js(ctx)?;
        let name_str: &str = key.name.as_ref();
        ctx.register_global_property(
            PropertyKey::from(JsString::from(name_str)),
            js_fun,
            Attribute::all(),
        )
        .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?;
    }

    Ok(())
}
