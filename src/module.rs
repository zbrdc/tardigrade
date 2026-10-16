//! ES module entry support.
//!
//! With `Config::module` (CLI `--module`, or an entry whose name ends in
//! `.mjs`), the entry source is parsed as an ES module instead of a
//! classic script: `import`/`export` and top-level `await` work.
//! Specifiers resolve inside the run's RAM filesystem — absolute
//! (`/lib.mjs`) or relative to the importing module (`./lib.mjs`) — so a
//! module graph ships through `--preload` snapshots like everything
//! else. Bare specifiers are refused. Modules are cached per run, so
//! two imports of one path share one instance.

use crate::diag::report;
use crate::world::{world_mut, World};

/// The fs path the entry module answers to: relative imports from the
/// entry resolve against its directory. The entry source itself still
/// comes from the host; only its name moves into the fs namespace.
fn entry_fs_path(entry: &str) -> String {
    if entry == "-" || entry.is_empty() {
        return "/main.mjs".to_string();
    }
    let base = entry.rsplit('/').next().unwrap_or(entry);
    format!("/{}", base)
}

/// Resolves a specifier against the importing module's fs path.
fn resolve_spec(referrer: &str, spec: &str) -> Result<String, String> {
    if spec.starts_with('/') {
        return crate::fs::normalize(spec);
    }
    if spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == ".." {
        let dir = referrer.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        return crate::fs::normalize(&format!("{}/{}", dir, spec));
    }
    Err(format!(
        "bare module specifier is not supported: {spec:?} (use an absolute or relative RAM-fs path)"
    ))
}

/// Compiles `text` as the module at `path`, or returns the cached module
/// for that path. Parse only: no guest code runs here.
fn compile_cached(
    scope: &mut v8::PinScope<'_, '_>,
    path: &str,
    text: &str,
) -> Option<v8::Global<v8::Module>> {
    if let Some(global) = world_mut(scope).modules.get(path) {
        return Some(global.clone());
    }
    let source_text = v8::String::new_from_utf8(scope, text.as_bytes(), v8::NewStringType::Normal)?;
    let name = v8::String::new(scope, path)?;
    let origin = v8::ScriptOrigin::new(
        scope,
        name.into(),
        0,
        0,
        true,
        0,
        None,
        false,
        false,
        true,
        None,
    );
    let mut source = v8::script_compiler::Source::new(source_text, Some(&origin));
    let module = v8::script_compiler::compile_module(scope, &mut source)?;
    let identity = module.get_identity_hash().get();
    let global = v8::Global::new(scope, module);
    let world: &mut World = world_mut(scope);
    world.module_paths.insert(identity, path.to_string());
    world.modules.insert(path.to_string(), global.clone());
    Some(global)
}

/// Loads (compiling if needed) the module at an fs path from the RAM fs.
fn load_module(scope: &mut v8::PinScope<'_, '_>, path: &str) -> Option<v8::Global<v8::Module>> {
    if let Some(global) = world_mut(scope).modules.get(path) {
        return Some(global.clone());
    }
    let text = match world_mut(scope).fs.read_string(path) {
        Ok(text) => text,
        Err(e) => {
            let msg = v8::String::new(scope, &format!("cannot load module {path}: {e}"))?;
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
            return None;
        }
    };
    compile_cached(scope, path, &text)
}

/// The static-import resolver: V8 asks for every specifier in the graph;
/// each answer is compiled from the RAM fs and cached by resolved path.
fn resolve_module<'a>(
    context: v8::Local<'a, v8::Context>,
    specifier: v8::Local<'a, v8::String>,
    _import_assertions: v8::Local<'a, v8::FixedArray>,
    referrer: v8::Local<'a, v8::Module>,
) -> Option<v8::Local<'a, v8::Module>> {
    // Bootstrap like the crate's own callback trampolines: the pinned
    // CallbackScope storage inits into the handle scope every V8 API
    // call takes, then a nested HandleScope scopes the created module
    // handles to this callback, exactly as the old double-wrap did.
    let storage = std::pin::pin!(unsafe { v8::CallbackScope::new(context) });
    let mut scope = storage.init();
    v8::scope!(let scope, &mut scope);
    let spec = specifier.to_rust_string_lossy(scope);
    let referrer_path = world_mut(scope)
        .module_paths
        .get(&referrer.get_identity_hash().get())
        .cloned()
        .unwrap_or_else(|| "/main.mjs".to_string());
    let path = match resolve_spec(&referrer_path, &spec) {
        Ok(path) => path,
        Err(e) => {
            let msg = v8::String::new(scope, &e)?;
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
            return None;
        }
    };
    let global = load_module(scope, &path)?;
    let local = v8::Local::new(scope, &global);
    // SAFETY: a Local is a handle into the isolate's handle space, and
    // its Rust lifetime is a conservative scope marker. V8 takes its own
    // reference to the module for the graph it is instantiating, on this
    // same isolate and context, before the callback scope closes — the
    // same pattern the v8 crate's own module tests rely on when they
    // return a module compiled in this callback.
    Some(unsafe {
        std::mem::transmute::<v8::Local<'_, v8::Module>, v8::Local<'a, v8::Module>>(local)
    })
}

/// Dynamic `import()`: compile, instantiate and evaluate the target,
/// then settle the promise with its namespace (or the failure).
pub(crate) fn import_dynamically<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _host_defined_options: v8::Local<'s, v8::Data>,
    resource_name: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    _import_assertions: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let resolver = v8::PromiseResolver::new(scope)?;
    let promise = resolver.get_promise(scope);
    let spec = specifier.to_rust_string_lossy(scope);
    // The referrer's resource name is the fs path for modules; classic
    // scripts carry a host path, whose fs meaning is the root.
    let referrer_name = resource_name.to_rust_string_lossy(scope);
    let referrer_path = if referrer_name.starts_with('/') && !referrer_name.contains(".js:") {
        referrer_name
    } else {
        "/main.mjs".to_string()
    };
    // Resolve inside the closure: the namespace handle is borrowed from
    // the module local, so it must be consumed (by the resolver) before
    // the closure returns rather than returned through it.
    let outcome = (|| -> Result<(), String> {
        let path = resolve_spec(&referrer_path, &spec)?;
        let global =
            load_module(scope, &path).ok_or_else(|| "module failed to compile".to_string())?;
        let module = v8::Local::new(scope, &global);
        if module.get_status() == v8::ModuleStatus::Uninstantiated {
            module
                .instantiate_module(scope, resolve_module)
                .ok_or_else(|| "module failed to instantiate".to_string())?;
        }
        if module.get_status() == v8::ModuleStatus::Instantiated {
            module
                .evaluate(scope)
                .ok_or_else(|| "module failed to evaluate".to_string())?;
        }
        resolver.resolve(scope, module.get_module_namespace());
        Ok(())
    })();
    match outcome {
        Ok(()) => {}
        Err(e) => {
            // A compile failure may already have thrown into the scope;
            // prefer that exception, else reject with the message.
            let msg = v8::String::new(scope, &e).map(|m| -> v8::Local<'s, v8::Value> { m.into() });
            if let Some(msg) = msg {
                resolver.reject(scope, msg);
            }
        }
    }
    Some(promise)
}

/// Evaluates the entry source as an ES module, with the same exit-code
/// contract as the classic-script path (vm.exit wins, then termination
/// reasons, then unhandled rejections).
pub(crate) fn eval_entry_module(
    scope: &mut v8::PinScope<'_, '_>,
    source: &str,
    entry: &str,
) -> i32 {
    if source.is_empty() {
        return 0;
    }
    let path = entry_fs_path(entry);
    {
        v8::tc_scope!(let tc, scope);
        let global = compile_cached(tc, &path, source);
        let global = match global {
            Some(m) => m,
            None => return exception_outcome(tc),
        };
        let module = v8::Local::new(tc, &global);
        if module.get_status() == v8::ModuleStatus::Uninstantiated
            && module.instantiate_module(tc, resolve_module).is_none()
        {
            return exception_outcome(tc);
        }
        if module.evaluate(tc).is_none() {
            return exception_outcome(tc);
        }
    }
    let exit = world_mut(scope).exit_code();
    if world_mut(scope).has_exit() {
        return exit;
    }
    crate::event_loop::stopped(scope)
        .or_else(|| crate::event_loop::check_unhandled(scope))
        .unwrap_or(0)
}

/// The failed-compile/instantiate/evaluate exit, shared by every stage:
/// a guest vm.exit or an outside termination outranks the exception,
/// which is reported only when it is what decided the run.
fn exception_outcome(tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>) -> i32 {
    if tc.as_mut().is_execution_terminating() {
        tc.as_mut().cancel_terminate_execution();
        return world_mut(tc).take_terminated_exit();
    }
    let decides = world_mut(tc).exception_decides();
    let code = world_mut(tc).exception_exit();
    if decides {
        report(tc);
    }
    code
}

#[cfg(test)]
mod tests {
    use super::resolve_spec;

    #[test]
    fn specifiers_resolve_inside_the_ram_fs() {
        assert_eq!(
            resolve_spec("/app/main.mjs", "./lib.mjs").unwrap(),
            "/app/lib.mjs"
        );
        assert_eq!(resolve_spec("/app/main.mjs", "../x.mjs").unwrap(), "/x.mjs");
        assert_eq!(resolve_spec("/main.mjs", "/abs.mjs").unwrap(), "/abs.mjs");
        assert!(resolve_spec("/main.mjs", "bare-pkg").is_err());
        assert!(resolve_spec("/main.mjs", "../../escape.mjs").is_err());
    }
}
