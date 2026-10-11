//! Console protocol: one JS expression per input line, one JSON line out.

use std::io::BufRead;
use std::sync::mpsc::Sender;

use crate::diag::{report, to_rust_string};
use crate::event_loop::event_loop;
use crate::eval_src;
use crate::world::world_mut;

/// Wraps one command line as a guest expression that always yields a JSON
/// string. The line sits between newlines so a trailing `//` comment ends at
/// the newline instead of swallowing the closing brackets. JSON.stringify
/// returns undefined for undefined/functions; that becomes `null` so every
/// output line is valid JSON. Concatenation, not format!: a missing brace in
/// an escaped template becomes a guest SyntaxError.
fn wrap_serve_expr(line: &str) -> String {
    String::from("(function(){try{var r=JSON.stringify((\n")
        + line
        + "\n));return r===undefined?\"null\":r}catch(e){return JSON.stringify({$error:String(e&&e.message||e)})}})()"
}

/// All encoding happens guest-side (JSON.stringify), so the host stays
/// JSON-free. Output lines (newline included) go to `lines`, which streams
/// them to the calling thread.
pub(crate) fn serve_loop<R: BufRead>(
    scope: &mut v8::HandleScope,
    input: R,
    lines: &Sender<String>,
) -> i32 {
    for line in input.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        let wrapped = wrap_serve_expr(&line);
        let mut tc = v8::TryCatch::new(scope);
        match eval_src(&mut tc, &wrapped, "serve") {
            Some(v) => {
                let out = to_rust_string(&mut tc, &v);
                let _ = lines.send(format!("{}\n", out));
            }
            None => {
                if tc.is_execution_terminating() {
                    tc.cancel_terminate_execution();
                    let code = world_mut(&mut tc).exit_code();
                    return if code >= 0 { code } else { 124 };
                }
                report(&mut tc);
                return 1;
            }
        }
        drop(tc);
        let code = event_loop(scope); // settle timers/net between commands
        let exit = world_mut(scope).exit_code();
        if exit >= 0 {
            return exit;
        }
        if code != 0 {
            return code;
        }
    }
    let exit = world_mut(scope).exit_code();
    if exit >= 0 {
        exit
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_wrapper_isolates_the_line() {
        let w = wrap_serve_expr("1 + 1 // note");
        assert!(w.contains("\n1 + 1 // note\n"));
        assert!(w.ends_with("})()"));
    }
}
