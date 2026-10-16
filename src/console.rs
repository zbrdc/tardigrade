//! console.log/info/warn/error: to the host streams. In serve mode
//! stdout is the reply channel, so all console output goes to stderr
//! there: a guest print can never be mistaken for a reply line or shift
//! the reply stream by one, which the bridge and any other serve
//! consumer rely on (see docs/KNOWN_BUGS.md).

use std::io::Write;

use crate::diag::to_rust_string;
use crate::guest_cb;
use crate::webapi::throw_range;
use crate::world::world_mut;

fn print_line(
    args: &v8::FunctionCallbackArguments,
    scope: &mut v8::PinScope<'_, '_>,
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
    // Charge before writing: past the output cap the call throws and
    // writes nothing, so volume is bounded even though a blocked pipe
    // can still stall a single write (see docs/KNOWN_RISKS.md).
    {
        let world = world_mut(scope);
        if world.output_bytes.saturating_add(line.len() as u64) > world.output_cap {
            throw_range(scope, "guest output limit exceeded");
            return;
        }
        world.output_bytes = world.output_bytes.saturating_add(line.len() as u64);
    }
    let _ = out.write_all(line.as_bytes());
}

guest_cb!(console_log, |args, _rv, scope| {
    if world_mut(scope).serve_mode {
        let mut stderr = std::io::stderr();
        print_line(&args, scope, &mut stderr);
    } else {
        let mut stdout = std::io::stdout();
        print_line(&args, scope, &mut stdout);
    }
});

guest_cb!(console_err, |args, _rv, scope| {
    let mut stderr = std::io::stderr();
    print_line(&args, scope, &mut stderr);
});
