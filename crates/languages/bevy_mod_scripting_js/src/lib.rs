//! Javascript integration for the bevy_mod_scripting system.
use bevy_mod_scripting_core::{
    asset::Language,
    bindings::{
        function::namespace::Namespace, globals::AppScriptGlobalsRegistry,
        script_value::ScriptValue, ThreadWorldContainer, WorldContainer,
    },
    context::{ContextBuilder, ContextInitializer, ContextPreHandlingInitializer},
    error::{InteropError, ScriptError},
    event::CallbackLabel,
    runtime::RuntimeSettings,
    script::ScriptId,
    IntoScriptPluginParams, ScriptingPlugin,
};
use bindings::{
    reference::{JsReflectReference, JsStaticReflectReference},
    script_value::{FromJs, IntoJs, JsScriptValue, with_js_context},
};
pub use boa_engine;
use boa_engine::{
    js_string,
    property::{Attribute, PropertyKey},
    Context, JsObject, JsString, Source, JsValue
};

/// Bindings for JS.
pub mod bindings;

#[derive(Default)]
struct JsVmRegistry {
    vms: std::collections::HashMap<String, boa_engine::Context>,
}

#[inline]
fn sid_key(id: &ScriptId) -> String {
    id.as_ref().to_string()
}

/// The Js scripting plugin. Used to add javascript scripting to a bevy app within the context of the BMS framework.
pub struct JsScriptingPlugin {
    /// The internal scripting plugin
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
                // No Context initializers: our Context type is `()`. We do
                // all Boa registration inside `js_context_load/reload`.
                context_initializers: vec![],
                context_pre_handling_initializers: vec![],
                additional_supported_extensions: &["js"],
                language: Language::Js,
            },
        }
    }
}

impl IntoScriptPluginParams for JsScriptingPlugin {
    type C = ();
    type R = ();
    const LANGUAGE: Language = Language::Js;
    fn build_runtime() -> Self::R {}
}

impl AsMut<ScriptingPlugin<Self>> for JsScriptingPlugin {
    fn as_mut(&mut self) -> &mut ScriptingPlugin<JsScriptingPlugin> {
        &mut self.scripting_plugin
    }
}

impl bevy::app::Plugin for JsScriptingPlugin {
    fn build(&self, app: &mut bevy::prelude::App) {
        self.scripting_plugin.build(app);
        app.insert_non_send_resource(JsVmRegistry::default());
    }
    fn finish(&self, app: &mut bevy::app::App) {
        self.scripting_plugin.finish(app);
    }
}

#[profiling::function]
/// Load a js context from a script
pub fn js_context_load(
    script_id: &ScriptId,
    content: &[u8],
    _initializers: &[ContextInitializer<JsScriptingPlugin>],
    _pre: &[ContextPreHandlingInitializer<JsScriptingPlugin>],
    _: &(),
) -> Result<(), ScriptError> {
    // World guard from BMS TLS
    let world_guard = ThreadWorldContainer.try_get_world()?;

    // pull registries we mirror into JS
    let globals_registry = world_guard.with_resource(|r: &AppScriptGlobalsRegistry| r.clone())?;
    let globals_registry = globals_registry.read();

    let script_fn_registry = world_guard.script_function_registry();
    let script_fn_registry = script_fn_registry.read();

    // new Boa VM for this script
    let mut ctx = Context::default();

    // register classes
    ctx.register_global_class::<JsReflectReference>()?;
    ctx.register_global_class::<JsStaticReflectReference>()?;

    // Alias the ReflectReference constructor to `ReflectRef` for static helpers (ref_get/ref_set/...)
    {
        let rr_ctor = ctx
            .global_object()
            .get(js_string!("ReflectReference"), &mut ctx)?
            .as_object()
            .ok_or_else(|| ScriptError::new(InteropError::invariant("ReflectReference ctor missing")))?
            .clone();

        ctx.register_global_property(
            PropertyKey::from(js_string!("ReflectRef")),
            JsValue::from(rr_ctor),
            Attribute::all(),
        )?;
    }

    // Bind `world` as a StaticReflectReference(TypeId::<World>)
    {
        let ctor = ctx
            .global_object()
            .get(js_string!("StaticReflectReference"), &mut ctx)?
            .as_object()
            .ok_or_else(|| ScriptError::new(InteropError::invariant("StaticReflectReference ctor missing")))?
            .clone();

        let proto = ctor
            .get(js_string!("prototype"), &mut ctx)?
            .as_object()
            .cloned();

        let world_ref_obj = JsObject::from_proto_and_data(
            proto,
            JsStaticReflectReference(std::any::TypeId::of::<bevy::ecs::world::World>()),
        );

        ctx.register_global_property(
            PropertyKey::from(js_string!("world")),
            JsValue::from(world_ref_obj),
            Attribute::all(),
        )?;
    }

    // app-level globals (dynamic makers or static TypeId bindings)
    for (key, global) in globals_registry.iter() {
        match &global.maker {
            Some(maker) => {
                // dynamic
                let sv = (maker)(world_guard.clone())?;
                let js_val = JsScriptValue::from(sv).into_js(&mut ctx)?;
                ctx.register_global_property(
                    PropertyKey::from(JsString::from(key.to_string())),
                    js_val,
                    Attribute::all(),
                )?;
            }
            None => {
                // static -> StaticReflectReference(type_id)
                let ctor = ctx
                    .global_object()
                    .get(js_string!("StaticReflectReference"), &mut ctx)?
                    .as_object()
                    .ok_or_else(|| ScriptError::new(InteropError::invariant("StaticReflectReference ctor missing")))?
                    .clone();

                let proto = ctor
                    .get(js_string!("prototype"), &mut ctx)?
                    .as_object()
                    .cloned();

                let obj = JsObject::from_proto_and_data(proto, JsStaticReflectReference(global.type_id));
                ctx.register_global_property(
                    PropertyKey::from(JsString::from(key.to_string())),
                    JsValue::from(obj),
                    Attribute::all(),
                )?;
            }
        }
    }

    // global namespace script functions
    for (key, function) in script_fn_registry
        .iter_all()
        .filter(|(k, _)| k.namespace == Namespace::Global)
    {
        let js_fun = JsScriptValue::from(ScriptValue::Function(function.clone())).into_js(&mut ctx)?;
        ctx.register_global_property(
            PropertyKey::from(JsString::from(key.name.to_string())),
            js_fun,
            Attribute::all(),
        )?;
    }

    // evaluate script
    ctx.eval(Source::from_bytes(content))?;

    // stash VM as NonSend resource — avoid moving `ctx` across a borrow
    let mut ctx_opt = Some(ctx);
    let _ = world_guard
        .with_global_access(|world| {
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
/// Reload a lua context from a script
pub fn js_context_reload(
    script_id: &ScriptId,
    content: &[u8],
    _old_ctx_unit: &mut (),
    _initializers: &[ContextInitializer<JsScriptingPlugin>],
    _pre: &[ContextPreHandlingInitializer<JsScriptingPlugin>],
    _: &(),
) -> Result<(), ScriptError> {
    let world_guard = ThreadWorldContainer.try_get_world()?;

    // Get the existing VM and eval the new source.
    let _ = world_guard.with_global_access(|world| -> Result<(), ScriptError> {
        let mut reg = world
            .get_non_send_resource_mut::<JsVmRegistry>()
            .ok_or_else(|| ScriptError::new(InteropError::invariant("JsVmRegistry missing")))?;
        let ctx: &mut Context = reg.vms
            .get_mut(&sid_key(script_id))
            .ok_or_else(|| ScriptError::new(InteropError::invariant("no VM for this ScriptId")))?;

        // -- debug
        let has_world = !ctx.global_object()
            .get(js_string!("world"), ctx)?   // <- pass `ctx`, not `&mut ctx`
            .is_undefined();
        bevy::log::info!("JS loader (lib.js_context_reload.223): world present? {has_world}");
        // --

        ctx.eval(Source::from_bytes(content))
            .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?;
        Ok(())
    }).map_err(ScriptError::new)?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[profiling::function]
/// The js handler for events
pub fn js_handler(
    args: Vec<ScriptValue>,
    _entity: bevy::ecs::entity::Entity,
    script_id: &ScriptId,
    callback_label: &CallbackLabel,
    _ctx_unit: &mut (),
    _pre: &[ContextPreHandlingInitializer<JsScriptingPlugin>],
    _: &(),
) -> Result<ScriptValue, ScriptError> {
    let world_guard = ThreadWorldContainer.try_get_world()?;

    let out = world_guard
        .with_global_access(|world| -> Result<ScriptValue, ScriptError> {
            let mut reg = match world.get_non_send_resource_mut::<JsVmRegistry>() {
                Some(r) => r,
                None => {
                    bevy::log::trace!(
                        "No JsVmRegistry; script {} ignored callback {}",
                        script_id,
                        callback_label.as_ref()
                    );
                    return Ok(ScriptValue::Unit);
                }
            };

            let Some(ctx) = reg.vms.get_mut(&sid_key(script_id)) else {
                bevy::log::trace!(
                    "Script {} has no VM; callback {} ignored",
                    script_id,
                    callback_label.as_ref()
                );
                return Ok(ScriptValue::Unit);
            };

            // Make ctx visible to nested conversions/callbacks.
            with_js_context(ctx, move |ctx| {
                // Lookup global callback by name.
                let fun_val = match ctx
                    .global_object()
                    .get(JsString::from(callback_label.as_ref()), ctx)
                {
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
                    .call(&boa_engine::JsValue::undefined(), &js_args, ctx)
                    .map_err(|e| ScriptError::new(InteropError::invariant(e.to_string())))?;
                let sv = JsScriptValue::from_js(&out, ctx).map_err(ScriptError::new)?.0;
                Ok(sv)
            })
        })
        .map_err(ScriptError::new)??; // map InteropError -> ScriptError, then flatten

    Ok(out)
}
