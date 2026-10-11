//! Console protocol: one JS expression per input line, one JSON line out.

use std::sync::mpsc::{self, Sender};

use crate::diag::{report, to_rust_string};
use crate::event_loop::event_loop;
use crate::eval_src;
use crate::stdin::{self, StdinSub};
use crate::world::world_mut;

/// A serve reply on its way to the caller's writer: the line plus the ack
/// the guest waits on until the line has been written and flushed.
pub(crate) type Reply = (String, Sender<()>);

/// Where serve_loop sends replies; created by `serve`.
pub(crate) type ReplySink = mpsc::SyncSender<Reply>;

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

/// Sends one reply and rendezvous with the caller's write, so the reply is
/// on the caller's writer before this guest prints anything else. False:
/// the caller side is gone; the session should stop (there is nothing left
/// to report an error to).
fn send_reply(replies: &ReplySink, line: String) -> bool {
    let (ack_tx, ack_rx) = mpsc::channel();
    if replies.send((line, ack_tx)).is_err() {
        return false;
    }
    ack_rx.recv().is_ok()
}

/// Splits one command line (without its newline, trailing CR stripped) off
/// `pending`. Err means invalid UTF-8, which ends the session just like the
/// old BufRead::lines read error did.
fn split_line(pending: &mut Vec<u8>) -> Result<Option<String>, ()> {
    let Some(nl) = pending.iter().position(|b| *b == b'\n') else {
        return Ok(None);
    };
    let mut line: Vec<u8> = pending.drain(..=nl).collect();
    line.pop(); // the newline
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line).map(Some).map_err(|_| ())
}

/// Next command line, blocking for more chunks through the hub; at EOF a
/// trailing line without a newline is still a command.
fn next_command(sub: &StdinSub, pending: &mut Vec<u8>) -> Option<Result<String, ()>> {
    loop {
        match split_line(pending) {
            Ok(Some(line)) => return Some(Ok(line)),
            Err(()) => return Some(Err(())),
            Ok(None) => {}
        }
        match sub.wait_take() {
            Some(chunk) => pending.extend_from_slice(&chunk),
            None => {
                if pending.is_empty() {
                    return None;
                }
                let mut line = match String::from_utf8(std::mem::take(pending)) {
                    Ok(line) => line,
                    Err(_) => return Some(Err(())),
                };
                if line.ends_with('\r') {
                    line.pop();
                }
                return Some(Ok(line));
            }
        }
    }
}

/// All encoding happens guest-side (JSON.stringify), so the host stays
/// JSON-free. Commands come through the process-wide stdin hub (same reader
/// as vm.stdin), so a serve session coexists with runs that read stdin.
/// Output lines (newline included) rendezvous with the caller's writer.
pub(crate) fn serve_loop(scope: &mut v8::HandleScope, replies: &ReplySink) -> i32 {
    let sub = stdin::subscribe();
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let line = match next_command(&sub, &mut pending) {
            None => break,
            Some(Err(())) => break, // invalid UTF-8, like the old read error
            Some(Ok(line)) => line,
        };
        if line.is_empty() {
            continue;
        }
        let wrapped = wrap_serve_expr(&line);
        let mut tc = v8::TryCatch::new(scope);
        match eval_src(&mut tc, &wrapped, "serve") {
            Some(v) => {
                let out = to_rust_string(&mut tc, &v);
                if !send_reply(replies, format!("{}\n", out)) {
                    return 0; // the consumer is gone; stop the session
                }
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

    #[test]
    fn command_lines_split_on_newlines_and_strip_cr() {
        let mut pending = b"one\r\ntwo\n".to_vec();
        assert_eq!(split_line(&mut pending), Ok(Some("one".to_string())));
        assert_eq!(split_line(&mut pending), Ok(Some("two".to_string())));
        assert_eq!(split_line(&mut pending), Ok(None));
    }

    #[test]
    fn invalid_utf8_ends_the_command_stream() {
        let mut pending = b"\xff\xfe\n".to_vec();
        assert_eq!(split_line(&mut pending), Err(()));
    }
}
