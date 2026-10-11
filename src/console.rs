//! console.log/info/warn/error: straight to the host streams.

use std::io::Write;

use crate::guest_cb;
use crate::world::to_rust_string;

fn print_line(
    args: &v8::FunctionCallbackArguments,
    scope: &mut v8::HandleScope,
    out: &mut dyn Write,
) {
    let mut line = String::new();
    for i in 0..args.length() {
        if i > 0 {
            line.push(' ');
        }
        line.push_str(&to_rust_string(scope, &args.get(i)));
    }
    line.push('\n');
    let _ = out.write_all(line.as_bytes());
}

guest_cb!(console_log, |args, _rv, scope| {
    let mut stdout = std::io::stdout();
    print_line(&args, scope, &mut stdout);
});

guest_cb!(console_err, |args, _rv, scope| {
    let mut stderr = std::io::stderr();
    print_line(&args, scope, &mut stderr);
});
