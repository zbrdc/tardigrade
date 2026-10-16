//! Guest-facing web-platform APIs: the byte level of TextEncoder /
//! TextDecoder ([text]), atob / btoa ([base64]), structuredClone over
//! V8's own value serializer ([clone]), and performance.now plus the
//! seeded Math.random ([random]). The JS classes that need per-instance
//! state live in prelude.js; everything byte-shaped is a small pure
//! function here so it can be unit-tested without V8. Nothing in this
//! tree allocates more than a bounded buffer for a guest-sized input:
//! allocations are reserved fallibly (try_reserve) and surface as guest
//! errors instead of aborting an embedder's process.

mod base64;
mod clone;
mod random;
mod text;

use crate::api::{put_fn, set};
use crate::clock;
use crate::Config;

/// Builds a guest string; realistic lengths always convert, so the
/// empty-string fallback only guards against a V8 refusal.
pub(crate) fn v8_str<'s>(scope: &mut v8::PinScope<'s, '_>, s: &str) -> v8::Local<'s, v8::String> {
    v8::String::new(scope, s).unwrap_or_else(|| v8::String::empty(scope))
}

fn throw_type(scope: &mut v8::PinScope<'_, '_>, msg: &str) {
    let m = v8_str(scope, msg);
    let exc = v8::Exception::type_error(scope, m);
    scope.throw_exception(exc);
}

pub(crate) fn throw_range(scope: &mut v8::PinScope<'_, '_>, msg: &str) {
    let m = v8_str(scope, msg);
    let exc = v8::Exception::range_error(scope, m);
    scope.throw_exception(exc);
}

/// Throws an already-built Error with its `name` property set
/// non-enumerable, the way platform error properties are, so
/// `Object.keys(e)` stays empty.
pub(crate) fn throw_named_error(
    scope: &mut v8::PinScope<'_, '_>,
    exc: v8::Local<v8::Value>,
    name: &str,
) {
    match v8::Local::<v8::Object>::try_from(exc) {
        Ok(obj) => {
            let key = v8_str(scope, "name");
            let val = v8_str(scope, name);
            let _ = obj.define_own_property(
                scope,
                key.into(),
                val.into(),
                v8::PropertyAttribute::DONT_ENUM,
            );
            scope.throw_exception(obj.into());
        }
        Err(_) => {
            scope.throw_exception(exc);
        }
    }
}

/// Throws an Error whose `name` property is set (browsers throw
/// DOMExceptions here; without that class an Error with the platform
/// name keeps `catch (e) => e.name` checks working). The name is
/// installed non-enumerable, the way platform error properties are, so
/// `Object.keys(e)` stays empty.
pub(crate) fn throw_named(scope: &mut v8::PinScope<'_, '_>, name: &str, msg: &str) {
    let m = v8_str(scope, msg);
    let exc = v8::Exception::error(scope, m);
    throw_named_error(scope, exc, name);
}

pub(crate) fn install(scope: &mut v8::PinScope<'_, '_>, cfg: &Config) {
    let global = scope.get_current_context().global(scope);

    put_fn(global, scope, "atob", base64::atob);
    put_fn(global, scope, "btoa", base64::btoa);
    put_fn(global, scope, "structuredClone", clone::structured_clone);
    // Helper callbacks the prelude's TextEncoder/TextDecoder classes
    // bind and then remove from the global object.
    put_fn(global, scope, "__encodeUtf8", text::encode_utf8);
    put_fn(global, scope, "__encodeIntoUtf8", text::encode_into_utf8);
    put_fn(global, scope, "__decodeUtf8", text::decode_utf8);
    // The byte budget the prelude's WebAssembly.Memory / resizable-buffer
    // wrappers charge against (they capture and delete this). It is the
    // effective --memory cap, i.e. with the 8 MiB floor applied.
    let cap_bytes = crate::heap::plan(cfg.heap_mb).cap_bytes;
    let cap = v8::Number::new(scope, cap_bytes as f64);
    set(global, scope, "__memoryCapBytes", cap.into());

    let performance = v8::Object::new(scope);
    put_fn(performance, scope, "now", random::performance_now);
    set(global, scope, "performance", performance.into());

    // Without a seed Math.random stays V8's own; with one, runs become
    // reproducible (per-run state lives in the World).
    if cfg.seed.is_some() {
        math_random_override(scope, global);
    }
    // Virtual runs expose the timeline to the prelude, which captures it
    // and removes it, then rebuilds Date over it (real runs keep V8's
    // own Date untouched).
    if cfg.clock.is_virtual() {
        put_fn(global, scope, "__virtualDateMs", clock::virtual_date_now);
    }
}

/// Installs the seeded Math.random override on the Math object.
fn math_random_override(scope: &mut v8::PinScope<'_, '_>, global: v8::Local<v8::Object>) {
    let math_key = v8_str(scope, "Math");
    let Some(math) = global.get(scope, math_key.into()) else {
        return;
    };
    let Ok(math) = v8::Local::<v8::Object>::try_from(math) else {
        return;
    };
    put_fn(math, scope, "random", random::math_random_seeded);
}
