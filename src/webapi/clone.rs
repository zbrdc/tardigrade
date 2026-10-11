//! structuredClone over V8's own ValueSerializer / ValueDeserializer, so
//! clone semantics are the engine's: Map, Set, Date, RegExp, typed
//! arrays and their windows, ArrayBuffers, cycles, and shared references
//! inside one clone. Functions, symbols, WebAssembly modules and (here)
//! SharedArrayBuffers are refused through the serializer delegate with
//! an Error named DataCloneError. There is no transfer list: transfers
//! would detach buffers across the clone, and there is no shared-id
//! registry to carry a SAB within the run.

use crate::guest_cb;
use crate::webapi::{throw_named, throw_named_error, throw_type, v8_str};

/// Serializer delegate: V8 refuses functions, symbols, shared buffers
/// and other non-cloneable values by asking the delegate to raise the
/// error.
struct CloneSerializer;

impl v8::ValueSerializerImpl for CloneSerializer {
    fn throw_data_clone_error<'s>(
        &self,
        scope: &mut v8::HandleScope<'s>,
        message: v8::Local<'s, v8::String>,
    ) {
        let exc = v8::Exception::error(scope, message);
        crate::webapi::throw_named_error(scope, exc, "DataCloneError");
    }

    fn get_shared_array_buffer_id(
        &self,
        _scope: &mut v8::HandleScope<'_>,
        _shared_array_buffer: v8::Local<'_, v8::SharedArrayBuffer>,
    ) -> Option<u32> {
        // Without a shared-id registry a SAB cannot be cloned within the
        // run. Declining lets V8 raise its own "could not be cloned"
        // message; the callback below gives it the DataCloneError name.
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
/// but an array is a TypeError (a sequence is required), and a non-empty
/// array is refused by the caller.
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

guest_cb!(structured_clone, |args, rv, scope| {
    if args.length() == 0 {
        return throw_type(
            scope,
            "The \"The value argument must be specified\" argument must be specified",
        );
    }
    match transfer_want(scope, &args.get(1)) {
        TransferWant::Refuse => {
            return throw_named(scope, "DataCloneError", "transfer is not supported");
        }
        TransferWant::Threw => return,
        TransferWant::BadOptions => {
            return throw_type(scope, "The provided value is not of type 'object'");
        }
        TransferWant::BadTransfer => {
            return throw_type(scope, "The provided value is not of type 'sequence'");
        }
        TransferWant::None => {}
    }
    let value = args.get(0);
    let context = scope.get_current_context();
    let ser = v8::ValueSerializer::new(scope, Box::new(CloneSerializer));
    let mut tc = v8::TryCatch::new(scope);
    // The wire-format header must precede the value (the v8 crate tests
    // pair write_header with read_header).
    use v8::ValueSerializerHelper as _;
    ser.write_header();
    if ser.write_value(context, value) != Some(true) {
        // A TryCatch that drops a caught exception swallows it, so the
        // failure is always rethrown. The delegate already named its own
        // refusals (functions, symbols, wasm); V8's directly-raised
        // shared-buffer error still needs the name. Guest exceptions from
        // getters propagate untouched, like the platform.
        if !tc.has_caught() {
            let m = v8_str(&mut tc, "value could not be cloned");
            let exc = v8::Exception::error(&mut tc, m);
            throw_named_error(&mut tc, exc, "DataCloneError");
        } else if is_unnamed_clone_error(&mut tc) {
            if let Some(exc) = tc.exception() {
                throw_named_error(&mut tc, exc, "DataCloneError");
            }
        }
        tc.rethrow();
        return;
    }
    let data = ser.release();
    let de = v8::ValueDeserializer::new(&mut tc, Box::new(CloneDeserializer), &data);
    // The header (wire-format version) precedes any value; read it first.
    use v8::ValueDeserializerHelper as _;
    if de.read_header(context) != Some(true) {
        rethrow_or_throw(
            &mut tc,
            "structuredClone: could not decode the serialized value",
        );
        return;
    }
    match de.read_value(context) {
        Some(v) => rv.set(v),
        None => rethrow_or_throw(&mut tc, "structuredClone: could not deserialize the value"),
    }
});

/// True when the pending exception is one of V8's own "<value> could be
/// cloned." errors - the ones raised directly (not through the delegate)
/// for shared buffers, which are the only ones still missing the
/// DataCloneError name. Reading "message" off a real error never runs
/// guest code; a crafted getter that throws while answering reads as
/// "not ours" and keeps its own exception.
fn is_unnamed_clone_error(tc: &mut v8::TryCatch<v8::HandleScope>) -> bool {
    let Some(exc) = tc.exception() else {
        return false;
    };
    let Ok(obj) = v8::Local::<v8::Object>::try_from(exc) else {
        return false;
    };
    let key = v8_str(tc, "message");
    let Some(msg) = obj.get(tc, key.into()) else {
        return false;
    };
    crate::diag::to_rust_string(tc, &msg).contains("could not be cloned")
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
