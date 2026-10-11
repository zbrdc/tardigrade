//! structuredClone over V8's own ValueSerializer / ValueDeserializer, so
//! clone semantics are the engine's: Map, Set, Date, RegExp, typed
//! arrays and their windows, ArrayBuffers, cycles, and shared references
//! inside one clone. Functions, symbols, WebAssembly modules and (here)
//! SharedArrayBuffers are refused with an Error named DataCloneError.
//! There is no transfer list: transfers would detach buffers across the
//! clone, and there is no shared-id registry to carry a SAB within the
//! run.

use std::cell::Cell;
use std::rc::Rc;

use crate::guest_cb;
use crate::webapi::{throw_named, throw_named_error, throw_type, v8_str};
use crate::world::world_mut;

/// Set by the serializer delegate when it declines a SharedArrayBuffer:
/// the only refusal whose error V8 raises directly (unnamed) instead of
/// through the delegate, and therefore the only one the callback has to
/// name itself. Guest exceptions from getters keep their identity.
type SabRefused = Rc<Cell<bool>>;

/// Serializer delegate: V8 refuses functions, symbols, shared buffers
/// and other non-cloneable values by asking the delegate to raise the
/// error.
struct CloneSerializer {
    sab_refused: SabRefused,
}

impl v8::ValueSerializerImpl for CloneSerializer {
    fn throw_data_clone_error<'s>(
        &self,
        scope: &mut v8::HandleScope<'s>,
        message: v8::Local<'s, v8::String>,
    ) {
        let exc = v8::Exception::error(scope, message);
        throw_named_error(scope, exc, "DataCloneError");
    }

    fn get_shared_array_buffer_id(
        &self,
        _scope: &mut v8::HandleScope<'_>,
        _shared_array_buffer: v8::Local<'_, v8::SharedArrayBuffer>,
    ) -> Option<u32> {
        // Without a shared-id registry a SAB cannot be cloned within the
        // run. Declining makes V8 raise its own "#<SharedArrayBuffer>
        // could not be cloned." error; the flag tells the callback that
        // this - and only this - refusal still needs the name.
        self.sab_refused.set(true);
        None
    }

    fn get_wasm_module_transfer_id(
        &self,
        scope: &mut v8::HandleScope<'_>,
        _module: v8::Local<'_, v8::WasmModuleObject>,
    ) -> Option<u32> {
        // There is no transfer registry to move a module within the run,
        // so it fails the way other uncloneables do.
        let msg = v8_str(scope, "WebAssembly.Module could not be cloned");
        self.throw_data_clone_error(scope, msg);
        None
    }
}

/// Deserializer delegate: the ids the defaults refuse (host objects,
/// shared buffers, wasm modules) are never written by CloneSerializer.
struct CloneDeserializer;
impl v8::ValueDeserializerImpl for CloneDeserializer {}

/// What the options argument asked for. Threw means a getter raised
/// while reading `transfer`, and the pending exception must propagate.
enum TransferWant {
    None,
    Refuse,
    Threw,
    BadOptions,
    BadTransfer,
}

/// Reads options.transfer under a TryCatch: a throwing getter must
/// propagate (rethrown before the TryCatch can swallow it), anything
/// but an array is a TypeError (a sequence is required; iterables such
/// as Sets are not accepted), and a non-empty array is refused by the
/// caller.
fn transfer_want(scope: &mut v8::HandleScope, opts: &v8::Local<v8::Value>) -> TransferWant {
    if opts.is_null_or_undefined() {
        return TransferWant::None;
    }
    if !opts.is_object() {
        return TransferWant::BadOptions;
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(*opts) else {
        return TransferWant::Threw;
    };
    let mut tc = v8::TryCatch::new(scope);
    let key = v8_str(&mut tc, "transfer");
    let Some(list) = obj.get(&mut tc, key.into()) else {
        tc.rethrow();
        return TransferWant::Threw;
    };
    // An absent (undefined) transfer member is no transfer at all.
    if list.is_undefined() {
        return TransferWant::None;
    }
    match v8::Local::<v8::Array>::try_from(list) {
        Ok(list) if list.length() > 0 => TransferWant::Refuse,
        Ok(_) => TransferWant::None,
        Err(_) => TransferWant::BadTransfer,
    }
}

/// Validates the options argument; false means the right error was
/// thrown (or a getter's exception is pending) and the call is over.
fn options_ok(scope: &mut v8::HandleScope, opts: &v8::Local<v8::Value>) -> bool {
    match transfer_want(scope, opts) {
        TransferWant::None => true,
        TransferWant::Refuse => {
            throw_named(scope, "DataCloneError", "transfer is not supported");
            false
        }
        TransferWant::Threw => false,
        TransferWant::BadOptions => {
            throw_type(scope, "The provided value is not of type 'object'");
            false
        }
        TransferWant::BadTransfer => {
            throw_type(
                scope,
                "Failed to execute 'structuredClone': transfer in Options can not be converted to sequence.",
            );
            false
        }
    }
}

guest_cb!(structured_clone, |args, rv, scope| {
    if args.length() == 0 {
        return throw_type(scope, "structuredClone requires a value argument");
    }
    if !options_ok(scope, &args.get(1)) {
        return;
    }
    let value = args.get(0);
    let context = scope.get_current_context();
    let sab_refused: SabRefused = Rc::new(Cell::new(false));
    let ser = v8::ValueSerializer::new(
        scope,
        Box::new(CloneSerializer {
            sab_refused: sab_refused.clone(),
        }),
    );
    let mut tc = v8::TryCatch::new(scope);
    // The wire-format header must precede the value (the v8 crate tests
    // pair write_header with read_header).
    use v8::ValueSerializerHelper as _;
    ser.write_header();
    if ser.write_value(context, value) != Some(true) {
        return clone_write_failure(&mut tc, &sab_refused);
    }
    let data = ser.release();
    let de = v8::ValueDeserializer::new(&mut tc, Box::new(CloneDeserializer), &data);
    // The header (wire-format version) precedes any value; read it first.
    use v8::ValueDeserializerHelper as _;
    // deserialize_failure below names its error DataCloneError only when
    // the run's ArrayBuffer budget refused a backing store while V8
    // rebuilt the value: the mark is cleared here and read on failure, so
    // an earlier refusal is never attributed to this clone.
    let budget = world_mut(&mut tc).ab_budget.clone();
    budget.clear_refused();
    if de.read_header(context) != Some(true) {
        return rethrow_or_throw(
            &mut tc,
            "structuredClone: could not decode the serialized value",
        );
    }
    match de.read_value(context) {
        Some(v) => {
            if buffer_mismatch(value, v) {
                return throw_named(
                    &mut tc,
                    "DataCloneError",
                    "value could not be cloned: buffer allocation failed",
                );
            }
            rv.set(v)
        }
        None => deserialize_failure(&mut tc, budget.take_refused()),
    }
});

/// The serializer just produced this data, so a failed read is V8 refusing
/// something while rebuilding it - in practice an ArrayBuffer the run's
/// budget would not grant, which V8 raises as a plain Error and which gets
/// the DataCloneError name the write-side refusals carry. Every other
/// failure keeps its identity: a RangeError from V8's own recursion limit
/// on deeply nested values stays a RangeError (the budget never refused),
/// and a caught exception that is not an Error object keeps its shape.
fn deserialize_failure(tc: &mut v8::TryCatch<v8::HandleScope>, budget_refused: bool) {
    match (tc.exception(), budget_refused) {
        (Some(exc), true) if exc.is_native_error() => {
            throw_named_error(tc, exc, "DataCloneError");
            tc.rethrow();
        }
        _ => rethrow_or_throw(tc, "structuredClone: could not deserialize the value"),
    }
}

/// True when a buffer-shaped value came back a different size than its
/// source. V8's serializer can fail its own wire allocation silently
/// under memory pressure and hand back a truncated buffer; a clone like
/// that must surface as an error, never as wrong data. Only the
/// top-level value is checked: buffers nested deeper have no cheap
/// oracle, and the top level is where a whole-buffer clone is asked for
/// by far the most often.
fn buffer_mismatch(orig: v8::Local<v8::Value>, cloned: v8::Local<v8::Value>) -> bool {
    let size = |v: v8::Local<v8::Value>| -> Option<usize> {
        if let Ok(buffer) = v8::Local::<v8::ArrayBuffer>::try_from(v) {
            return Some(buffer.byte_length());
        }
        v8::Local::<v8::ArrayBufferView>::try_from(v)
            .ok()
            .map(|view| view.byte_length())
    };
    matches!((size(orig), size(cloned)), (Some(a), Some(b)) if a != b)
}

/// Handles a failed write: guest exceptions (a getter that threw while
/// the serializer walked it) rethrow untouched, V8's directly-raised
/// shared-buffer refusal gets the DataCloneError name it is missing, and
/// a failure with no exception at all gets a named one. In every case
/// the exception is rethrown - a TryCatch that drops a caught exception
/// swallows it.
fn clone_write_failure(tc: &mut v8::TryCatch<v8::HandleScope>, sab_refused: &Cell<bool>) {
    let exc = match (tc.exception(), sab_refused.get()) {
        (Some(exc), true) => exc,
        (Some(_), false) => {
            tc.rethrow();
            return;
        }
        (None, _) => {
            let m = v8_str(tc, "value could not be cloned");
            v8::Exception::error(tc, m)
        }
    };
    throw_named_error(tc, exc, "DataCloneError");
    tc.rethrow();
}

/// A caught exception (for example from a throwing getter the
/// deserializer ran) must be rethrown, not replaced: dropping the
/// TryCatch would swallow it.
fn rethrow_or_throw(tc: &mut v8::TryCatch<v8::HandleScope>, msg: &str) {
    if tc.has_caught() {
        tc.rethrow();
    } else {
        crate::diag::throw(tc, msg);
    }
}
