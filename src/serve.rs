//! Console protocol: one JS expression per input line, one JSON line out.
//!
//! Commands and values travel through scratch globals (`__v8vm_line`,
//! `__v8vm_val`) so the host never has to escape or encode guest text:
//! one fixed expression per role does the work - indirect eval for the
//! command (a line that fails to compile throws inside the eval instead
//! of failing the wrapper, so a bad command is a reply, not the end of
//! the session), JSON.stringify for values, and a then-chain for
//! promises. All JSON encoding stays guest-side.

use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use crate::diag::to_rust_string;
use crate::event_loop::{event_loop, stopped};
use crate::eval_src;
use crate::stdin::{self, StdinSub};
use crate::watchdog::CommandWatchdog;
use crate::world::{world_mut, TermReason, COMMAND_TIMEOUT_CODE};

/// A serve reply on its way to the caller's writer: the line plus the ack
/// the guest waits on until the line has been written and flushed.
pub(crate) type Reply = (String, Sender<()>);

/// Where serve_loop sends replies; created by `serve`.
pub(crate) type ReplySink = mpsc::SyncSender<Reply>;

/// Compiles and runs the command line parked in `__v8vm_line`. Indirect
/// eval compiles it in the global scope; a line that fails to compile
/// throws a SyntaxError at this call, which is a reply, not a session
/// end.
const RUN_CMD: &str = "(0,eval)(globalThis.__v8vm_line)";

/// The guest-side error-message rule, shared by the two expressions that
/// build `$error` replies (a thrown command value, a rejection): the
/// message alone for plain Errors, name-colon-message for named ones
/// (SyntaxError, TypeError, ...), String(value) for anything else. The
/// try keeps an unprintable value (a symbol) from ending the session.
const ERR_MSG: &str = "var m;try{m=(e instanceof Error&&e.name!==\"Error\"&&e.message)\
?e.name+\": \"+e.message:String(e&&e.message||e);}catch(_){m=\"<unprintable>\";}";

/// Clears the scratch globals; folded into the encoders so every path
/// leaves the guest clean, and run directly by a recovered timeout.
const CLEAR: &str = "globalThis.__v8vm_val=undefined;globalThis.__v8vm_line=undefined;";

/// Fixed replies no guest code can influence.
const TIMED_OUT: &str = "{\"$error\":\"command timed out\"}";
const NEVER_SETTLED: &str = "{\"$error\":\"promise never settled\"}";
const ENCODE_FAILED: &str = "{\"$error\":\"<encoding failed>\"}";
const LINE_TOO_LONG: &str = "{\"$error\":\"command line too long\"}";

fn enc_val() -> String {
    format!("(function(v){{{CLEAR}var j=JSON.stringify(v);\
return j===undefined?\"null\":j;}})(globalThis.__v8vm_val)")
}

fn enc_err() -> String {
    format!("(function(e){{{CLEAR}{ERR_MSG}\
return JSON.stringify({{$error:m}});}})(globalThis.__v8vm_val)")
}

/// Attaches the encoders to the promise in `__v8vm_val`: the reply line
/// arrives as the fulfillment value of the returned promise, and the
/// await itself is the rejection handler, so a later rejection is never
/// an unhandled one.
fn enc_then() -> String {
    let ok = "function(v){var j=JSON.stringify(v);return j===undefined?\"null\":j;}";
    let err = format!("function(e){{{ERR_MSG}return JSON.stringify({{$error:m}});}}");
    format!("(function(p){{{CLEAR}return p.then({ok},{err});}})(globalThis.__v8vm_val)")
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

/// Splits one command line (without its newline, one trailing CR
/// stripped) off `pending`. Err means invalid UTF-8, which ends the
/// session just like the old BufRead::lines read error did.
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

/// One full command line when one is buffered.
fn split_done(pending: &mut Vec<u8>) -> Option<Result<String, ()>> {
    match split_line(pending) {
        Ok(Some(line)) => Some(Ok(line)),
        Err(()) => Some(Err(())),
        Ok(None) => None,
    }
}

/// EOF: nothing more will arrive, so the leftover bytes are the final
/// (unterminated) command - unless they are invalid UTF-8, which ends the
/// session like the old read error did. The final line strips one
/// trailing CR exactly like a terminated one, so "cmd", "cmd\r\n" and a
/// final "cmd\r" are one command.
fn final_command(pending: &mut Vec<u8>) -> Option<Result<String, ()>> {
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
    Some(Ok(line))
}

/// Next command line, blocking for more chunks through the hub; at EOF a
/// trailing line without a newline is still a command.
fn next_command(sub: &StdinSub, pending: &mut Vec<u8>) -> Option<Result<String, ()>> {
    loop {
        if let Some(line) = split_done(pending) {
            return Some(line);
        }
        let Some(chunk) = sub.wait_take() else {
            return final_command(pending);
        };
        pending.extend_from_slice(&chunk);
    }
}

/// What one fixed serve expression produced.
enum Step<'s> {
    Value(v8::Local<'s, v8::Value>),
    /// A thrown value (undefined when V8 gave none).
    Exception(v8::Local<'s, v8::Value>),
    Terminated,
}

fn eval_step<'s>(scope: &mut v8::HandleScope<'s>, src: &str) -> Step<'s> {
    let mut tc = v8::TryCatch::new(scope);
    match eval_src(&mut tc, src, "serve") {
        Some(v) => Step::Value(v),
        None if tc.is_execution_terminating() => Step::Terminated,
        None => {
            let exc = tc
                .exception()
                .unwrap_or_else(|| v8::undefined(&mut tc).into());
            Step::Exception(exc)
        }
    }
}

/// Parks a value in a scratch global for the next fixed expression.
fn park(scope: &mut v8::HandleScope, name: &str, value: v8::Local<v8::Value>) {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, name).unwrap();
    global.set(scope, key.into(), value);
}

/// Builds the command line as a V8 string and parks it. False when the
/// line is past the engine's string limit: an error reply, not a session
/// end.
fn park_line(scope: &mut v8::HandleScope, line: &str) -> bool {
    match v8::String::new_from_utf8(scope, line.as_bytes(), v8::NewStringType::Normal) {
        Some(s) => {
            park(scope, "__v8vm_line", s.into());
            true
        }
        None => false,
    }
}

/// What a command produced: a reply line (without newline), or the code
/// that ends the session.
enum Outcome {
    Reply(String),
    End(i32),
}

/// The per-session command context: where replies go, the handle the
/// per-command watchdog terminates through, and the command budget.
struct Serve<'a> {
    replies: &'a ReplySink,
    iso: v8::IsolateHandle,
    timeout_ms: u64,
}

fn command_deadline(timeout_ms: u64) -> Option<Instant> {
    (timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(timeout_ms))
}

/// Recovers from a fired per-command timeout so the session continues:
/// settle the watchdog first - joined before the cancel, or its last
/// re-request would re-arm V8's flag right after we clear it (the known
/// poisoning failure mode) - then clear the flag and our reason, which
/// also drops the leaving guard. A run-level reason that won the flag
/// meanwhile still ends the session with its own code.
fn recover_timeout(scope: &mut v8::HandleScope, wd: &mut CommandWatchdog) -> Outcome {
    wd.settle();
    if scope.is_execution_terminating() {
        scope.cancel_terminate_execution();
    }
    match world_mut(scope).term.reason() {
        Some(TermReason::CommandTimeout) | None => {
            let _ = world_mut(scope).term.take_reason();
            let _ = eval_step(scope, CLEAR); // no encoder ran to clear
            Outcome::Reply(TIMED_OUT.to_string())
        }
        _ => Outcome::End(world_mut(scope).take_terminated_exit()),
    }
}

/// A termination reached us mid-command: a per-command timeout is
/// recovered from; anything else (vm.exit, run timeout, heap cap) ends
/// the session with its own code.
fn terminated(scope: &mut v8::HandleScope, wd: &mut CommandWatchdog) -> Outcome {
    if world_mut(scope).term.reason() == Some(TermReason::CommandTimeout) {
        return recover_timeout(scope, wd);
    }
    Outcome::End(world_mut(scope).take_terminated_exit())
}

/// A command raised (or V8 swallowed a termination into one): unless a
/// run-level decision (vm.exit, an outside termination) outranks it, the
/// exception becomes an `$error` reply and the session continues.
fn command_exception(
    scope: &mut v8::HandleScope,
    exc: v8::Local<v8::Value>,
    wd: &mut CommandWatchdog,
) -> Outcome {
    if world_mut(scope).term.reason() == Some(TermReason::CommandTimeout) {
        return recover_timeout(scope, wd);
    }
    if world_mut(scope).exception_decides() {
        return encode_error(scope, exc, wd);
    }
    Outcome::End(world_mut(scope).exception_exit())
}

/// Encodes a thrown or rejection value as an `$error` reply. The encoder
/// itself can still fail (guest-replaced JSON globals): a constant line
/// beats ending the session over an encoding problem.
fn encode_error(
    scope: &mut v8::HandleScope,
    exc: v8::Local<v8::Value>,
    wd: &mut CommandWatchdog,
) -> Outcome {
    park(scope, "__v8vm_val", exc);
    match eval_step(scope, &enc_err()) {
        Step::Value(s) => Outcome::Reply(to_rust_string(scope, &s)),
        Step::Terminated => terminated(scope, wd),
        Step::Exception(_) => Outcome::Reply(ENCODE_FAILED.to_string()),
    }
}

/// Encodes the command's plain value (not a promise) as the reply line;
/// a stringification that throws (a circular structure) is an `$error`
/// reply too.
fn encode_value(
    scope: &mut v8::HandleScope,
    v: v8::Local<v8::Value>,
    wd: &mut CommandWatchdog,
) -> Outcome {
    park(scope, "__v8vm_val", v);
    match eval_step(scope, &enc_val()) {
        Step::Value(s) => Outcome::Reply(to_rust_string(scope, &s)),
        Step::Terminated => terminated(scope, wd),
        Step::Exception(e) => encode_error(scope, e, wd),
    }
}

/// What waiting for a command promise came to.
enum Settled {
    Yes,
    Never,
    /// A run-level exit code, or COMMAND_TIMEOUT_CODE.
    Ended(i32),
}

/// Runs the event loop until the promise settles; no busy-wait (the loop
/// parks between turns). The loop returning idle with the promise still
/// pending means no guest work exists that could ever settle it: without
/// a command deadline that is the answer; with one, park until it fires -
/// the watchdog is the one authority on the deadline, so this wait and
/// the recovery cannot race.
fn settle(
    scope: &mut v8::HandleScope,
    p: v8::Local<v8::Promise>,
    deadline: Option<Instant>,
) -> Settled {
    if p.state() != v8::PromiseState::Pending {
        return Settled::Yes;
    }
    let code = event_loop(scope);
    if code != 0 {
        return Settled::Ended(code);
    }
    if p.state() != v8::PromiseState::Pending {
        return Settled::Yes;
    }
    let Some(d) = deadline else {
        return Settled::Never;
    };
    let now = Instant::now();
    if now < d {
        std::thread::sleep(d - now);
    }
    Settled::Ended(COMMAND_TIMEOUT_CODE)
}

/// Replies with the settled inner promise: fulfilled carries the already
/// encoded line; rejected means an encoder threw (a circular value, a
/// replaced JSON global) and the reason becomes the `$error`.
fn reply_settled(
    scope: &mut v8::HandleScope,
    p: v8::Local<v8::Promise>,
    wd: &mut CommandWatchdog,
) -> Outcome {
    let result = p.result(scope);
    match p.state() {
        v8::PromiseState::Fulfilled => Outcome::Reply(to_rust_string(scope, &result)),
        _ => encode_error(scope, result, wd),
    }
}

/// The promise the then-chain returned: await it.
fn await_outcome(
    scope: &mut v8::HandleScope,
    inner: v8::Local<v8::Value>,
    wd: &mut CommandWatchdog,
    deadline: Option<Instant>,
) -> Outcome {
    let Ok(p) = v8::Local::<v8::Promise>::try_from(inner) else {
        return encode_value(scope, inner, wd); // a replaced then() gave a value
    };
    match settle(scope, p, deadline) {
        Settled::Yes => reply_settled(scope, p, wd),
        Settled::Never => Outcome::Reply(NEVER_SETTLED.to_string()),
        Settled::Ended(COMMAND_TIMEOUT_CODE) => recover_timeout(scope, wd),
        Settled::Ended(code) => Outcome::End(code),
    }
}

/// True when the promise rejected without a handler and none has arrived
/// since: the run's unhandled-rejection boundary owns it. The console must
/// not "handle" it by awaiting - attaching a handler would silently
/// unfail the run - so it keeps its old outcome (`{}`, and the checkpoint
/// decides the session).
fn unhandled_rejection(scope: &mut v8::HandleScope, p: &v8::Local<v8::Promise>) -> bool {
    let hash = p.get_identity_hash().get();
    let bucket = world_mut(scope).pending_rejections.get(&hash).cloned();
    bucket.is_some_and(|b| b.iter().any(|e| e.promise.open(scope) == &**p))
}

/// Replies with the command's value; a promise is settled first. A
/// promise that is pending or fulfilled when the expression returns is
/// the async result: the encoders are attached and the event loop runs
/// until it settles. A promise still sitting unhandled in the rejection
/// map (a synchronous `Promise.reject(...)`) is left to the boundary, so
/// that failure keeps its run-level meaning.
fn value_outcome(
    scope: &mut v8::HandleScope,
    v: v8::Local<v8::Value>,
    wd: &mut CommandWatchdog,
    deadline: Option<Instant>,
) -> Outcome {
    let Ok(p) = v8::Local::<v8::Promise>::try_from(v) else {
        return encode_value(scope, v, wd);
    };
    if p.state() == v8::PromiseState::Rejected && unhandled_rejection(scope, &p) {
        return Outcome::Reply("{}".to_string());
    }
    park(scope, "__v8vm_val", p.into());
    match eval_step(scope, &enc_then()) {
        Step::Value(inner) => await_outcome(scope, inner, wd, deadline),
        Step::Terminated => terminated(scope, wd),
        Step::Exception(e) => encode_error(scope, e, wd),
    }
}

/// Runs one command end to end. Some(code) ends the session.
fn run_command(scope: &mut v8::HandleScope, srv: &Serve, line: &str) -> Option<i32> {
    let deadline = command_deadline(srv.timeout_ms);
    let mut wd = CommandWatchdog::start(srv.iso.clone(), world_mut(scope).term.clone(), deadline);
    let out = if !park_line(scope, line) {
        Outcome::Reply(LINE_TOO_LONG.to_string())
    } else {
        // The command's own run holds back V8's automatic microtask
        // checkpoint: it would settle the command's promise (and record
        // its rejection as unhandled) before the host can attach the
        // await, turning every synchronous rejection into the boundary
        // case. With it held back the host sees the promise as the script
        // left it; the encoders and the event loop perform the explicit
        // checkpoints (microtask output stays before the reply, as
        // before).
        scope.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        let step = eval_step(scope, RUN_CMD);
        scope.set_microtasks_policy(v8::MicrotasksPolicy::Auto);
        match step {
            Step::Value(v) => value_outcome(scope, v, &mut wd, deadline),
            Step::Exception(e) => command_exception(scope, e, &mut wd),
            Step::Terminated => terminated(scope, &mut wd),
        }
    };
    wd.settle();
    // The watchdog is dead now, so nothing can mark or terminate for this
    // command any more. If it fired at the very end - after the last V8
    // call that could have observed it - clear its leftovers here instead
    // of letting the next boundary read them as a run-level stop (-2 must
    // never escape as a session code). A run-level reason is never
    // scrubbed.
    if world_mut(scope).term.reason() == Some(TermReason::CommandTimeout) {
        let _ = world_mut(scope).term.take_reason();
        if scope.is_execution_terminating() {
            scope.cancel_terminate_execution();
        }
    }
    match out {
        Outcome::Reply(text) => {
            if send_reply(srv.replies, format!("{}\n", text)) {
                None
            } else {
                Some(0) // the consumer is gone; stop the session
            }
        }
        Outcome::End(code) => Some(code),
    }
}

/// After each command: settle timers, net and microtasks, then apply the
/// run-level decisions (a vm.exit code, an outside termination, an
/// unhandled rejection from the command's work).
fn boundary(scope: &mut v8::HandleScope) -> Option<i32> {
    let code = event_loop(scope);
    let exit = world_mut(scope).exit_code();
    if exit >= 0 {
        return Some(exit);
    }
    if code != 0 {
        return Some(code);
    }
    None
}

/// EOF (or a stream error): the session ends like a finished program - a
/// termination that landed as the last command's work wound down (the
/// reason flag, not V8's possibly swallowed state) still ends it.
fn session_end(scope: &mut v8::HandleScope) -> i32 {
    let exit = world_mut(scope).exit_code();
    if exit >= 0 {
        return exit;
    }
    stopped(scope).unwrap_or(0)
}

/// Reads commands until EOF or a session-ending condition; `pending`
/// ends up holding the bytes pulled from the hub but never consumed.
fn session(scope: &mut v8::HandleScope, srv: &Serve, sub: &StdinSub, pending: &mut Vec<u8>) -> i32 {
    loop {
        let line = match next_command(sub, pending) {
            None | Some(Err(())) => break, // EOF; invalid UTF-8 like the old read error
            Some(Ok(line)) => line,
        };
        if line.is_empty() {
            continue;
        }
        if let Some(code) = run_command(scope, srv, &line) {
            return code;
        }
        if let Some(code) = boundary(scope) {
            return code;
        }
    }
    session_end(scope)
}

/// All encoding happens guest-side (JSON.stringify), so the host stays
/// JSON-free. Commands come through the process-wide stdin hub (same reader
/// as vm.stdin), so a serve session coexists with runs that read stdin.
/// Output lines (newline included) rendezvous with the caller's writer.
/// A command that fails to compile or throws is a reply; only host-level
/// failure ends the session (see README, Console protocol).
pub(crate) fn serve_loop(
    scope: &mut v8::HandleScope,
    iso: &v8::IsolateHandle,
    replies: &ReplySink,
    command_timeout_ms: u64,
) -> i32 {
    let sub = stdin::subscribe();
    let mut pending: Vec<u8> = Vec::new();
    let srv = Serve {
        replies,
        iso: iso.clone(),
        timeout_ms: command_timeout_ms,
    };
    let code = session(scope, &srv, &sub, &mut pending);
    // Bytes already pulled but never read belong to whoever subscribes
    // next ("input a run does not read is kept"), so hand them back.
    if !pending.is_empty() {
        stdin::push_front(std::mem::take(&mut pending));
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_final_line_strips_one_cr_like_a_terminated_one() {
        let mut pending = b"cmd\r".to_vec();
        assert_eq!(final_command(&mut pending), Some(Ok("cmd".to_string())));
        assert!(pending.is_empty());
        let mut none = Vec::new();
        assert_eq!(final_command(&mut none), None);
        let mut bad = b"\xff".to_vec();
        assert_eq!(final_command(&mut bad), Some(Err(())));
    }

    #[test]
    fn the_encoder_expressions_agree_on_the_error_rule() {
        // Both error paths share ERR_MSG, and every path clears the
        // scratch globals.
        let err = enc_err();
        let then = enc_then();
        assert!(err.contains("instanceof Error"));
        assert!(then.contains("instanceof Error"));
        assert!(err.contains(CLEAR.trim_end_matches(';')));
        assert!(then.contains(CLEAR.trim_end_matches(';')));
        assert!(enc_val().contains(CLEAR.trim_end_matches(';')));
    }
}
