//! Small diagnostics and v8-string helpers shared across modules.

pub(crate) fn to_rust_string(scope: &mut v8::HandleScope, v: &v8::Value) -> String {
    match v.to_string(scope) {
        Some(s) => s.to_rust_string_lossy(scope),
        None => String::new(),
    }
}

/// Stringifies a rejection value for the report. ToString is not defined
/// for symbols (it throws), so the plain conversion yields nothing there;
/// such values get a placeholder instead of an empty reason.
pub(crate) fn rejection_text(scope: &mut v8::HandleScope, v: &v8::Value) -> String {
    match v.to_string(scope) {
        Some(s) => s.to_rust_string_lossy(scope),
        None => "<unprintable rejection value>".to_string(),
    }
}

/// Throws a guest Error. Building the message string cannot fail for any
/// realistic length (paths are capped at 4096 bytes), but if V8 ever
/// refuses, an empty message still beats a panic across extern "C".
pub(crate) fn throw(scope: &mut v8::HandleScope, msg: &str) {
    let msg = v8::String::new(scope, msg).unwrap_or_else(|| v8::String::empty(scope));
    let exc = v8::Exception::error(scope, msg);
    scope.throw_exception(exc);
}

pub(crate) fn report(tc: &mut v8::TryCatch<'_, v8::HandleScope<'_>>) {
    match tc.message() {
        Some(msg) => {
            let resource = msg
                .get_script_resource_name(tc)
                .map(|v| to_rust_string(tc, &v))
                .unwrap_or_default();
            let line = msg.get_line_number(tc).unwrap_or(0);
            let m: v8::Local<v8::Value> = msg.get(tc).into();
            let text = to_rust_string(tc, &m);
            eprintln!("tardigrade: uncaught exception: {}:{}: {}", resource, line, text);
        }
        None => eprintln!("tardigrade: uncaught exception"),
    }
}
