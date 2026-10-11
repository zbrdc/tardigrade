//! Console protocol: one JS expression per input line, one JSON line out.
//!
//! Each command is compiled as its own wrapper - `(function(){ ... (\n<line>
//! \n) ... })()` - so the line is one expression in a function body, like
//! main's wrapper, and everything guest-side (JSON.stringify, the error
//! messages, promise settlement) happens inside that one script: no
//! guest-visible global carries the command or its value, and no
//! guest-replaceable lookup (eval) sits on the path. The host reads the
//! wrapper's result: the encoded line, or a promise of one when the value
//! is a thenable.

use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use crate::diag::to_rust_string;
use crate::event_loop::{event_loop, forget_rejection_of, stopped};
use crate::eval_src;
use crate::stdin::{self, StdinSub};
use crate::watchdog::CommandWatchdog;
use crate::world::{world_mut, TermReason, Wake, COMMAND_TIMEOUT_CODE};

/// A serve reply on its way to the caller's writer: the line plus the ack
/// the guest waits on until the line has been written and flushed.
pub(crate) type Reply = (String, Sender<()>);

/// Where serve_loop sends replies; created by `serve`.
pub(crate) type ReplySink = mpsc::SyncSender<Reply>;

/// Guest-side error-message rule, shared by every `$error` producer: the
/// message alone for plain Errors, name-colon-message for named ones
/// (SyntaxError, TypeError, ...), String(value) for anything else - a
/// thrown string says itself. The try keeps an unprintable value (a
/// symbol) from killing the session, and the result is always a plain
/// {$error: string} object, which JSON.stringify cannot refuse.
const ERR_BODY: &str = "var m;try{m=(e instanceof Error&&e.name!==\"Error\"&&e.message)\
?e.name+\": \"+e.message:String(e&&e.message||e);}catch(_){m=\"<unprintable>\";}\
return JSON.stringify({$error:m});";

/// Fixed replies no guest code can influence.
const TIMED_OUT: &str = "{\"$error\":\"command timed out\"}";
const NEVER_SETTLED: &str = "{\"$error\":\"promise never settled\"}";
const ENCODE_FAILED: &str = "{\"$error\":\"<encoding failed>\"}";
const LINE_TOO_LONG: &str = "{\"$error\":\"command line too long\"}";

/// A line at or past V8's maximum string length cannot become a script;
/// the wrapper needs a little headroom on top.
const MAX_LINE: usize = (1 << 29) - 1024;

/// Builds the command's wrapper. The line sits between newlines inside
/// parentheses, so it is one expression and a trailing `//` comment ends
/// at the newline; its value is encoded by the ok handler, whose own
/// failures (a BigInt, a circular structure) it catches and answers as
/// `$error` instead of rejecting the chain. A thenable value rides a
/// then-chain with both handlers attached: the await itself is the
/// rejection handler (so the command's own rejection is never unhandled),
/// and the settled encoded line arrives as the chain's fulfillment.
fn build_wrapper(line: &str) -> String {
    let ok = format!(
        "function(v){{var j;try{{j=JSON.stringify(v);}}catch(e){{{ERR_BODY}}}\
return j===undefined?\"null\":j;}}"
    );
    let err = format!("function(e){{{ERR_BODY}}}");
    format!(
        "(function(){{var ok={ok},err={err};\
try{{var r=(\n{line}\n);return r&&typeof r.then===\"function\"?Promise.resolve(r).then(ok,err):ok(r);}}\
catch(e){{return err(e);}}}})()"
    )
}

/// The standalone error encoder, called by the host with a value it holds
/// (the wrapper's own exception, a rejection reason): compiled fresh and
/// applied to its argument, so no guest-visible global is involved.
fn enc_err() -> String {
    format!("(function(e){{{ERR_BODY}}})")
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
    split_line(pending).transpose()
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
    let v = eval_src(&mut tc, src, "serve");
    step_from(&mut tc, v)
}

/// Classifies a V8 outcome: None means an exception (or a termination) is
/// pending in `tc`.
fn step_from<'s>(
    tc: &mut v8::TryCatch<'_, v8::HandleScope<'s>>,
    v: Option<v8::Local<'s, v8::Value>>,
) -> Step<'s> {
    match v {
        Some(v) => Step::Value(v),
        None if tc.is_execution_terminating() => Step::Terminated,
        None => Step::Exception(tc.exception().unwrap_or_else(|| v8::undefined(tc).into())),
    }
}

/// Compiles `src` (a function expression) and applies it to `value`.
fn call_with<'s>(
    scope: &mut v8::HandleScope<'s>,
    src: &str,
    value: v8::Local<v8::Value>,
) -> Step<'s> {
    let mut tc = v8::TryCatch::new(scope);
    let f = match eval_src(&mut tc, src, "serve") {
        Some(f) => f,
        None => return step_from(&mut tc, None),
    };
    let Ok(f) = v8::Local::<v8::Function>::try_from(f) else {
        return Step::Exception(v8::undefined(&mut tc).into());
    };
    let this = v8::undefined(&mut tc).into();
    let called = f.call(&mut tc, this, &[value]);
    step_from(&mut tc, called)
}

/// What a command produced.
enum Outcome {
    /// A reply line (without newline).
    Reply(String),
    /// The timeout reply: the deadline is spent, so the command's
    /// leftover work is not settled under it (the next command's is).
    ReplyTimedOut,
    /// The session ends with this code.
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

/// Recovers from a fired per-command timeout. The watchdog is settled
/// first - joined before the cancel, or its last re-request would re-arm
/// V8's flag right after we clear it (the known poisoning failure mode) -
/// and the cancel is unconditional: a terminate request still pending is
/// invisible to is_execution_terminating(). Only our own reason is
/// cleared; a run-level reason that won the flag meanwhile returns its
/// session-ending code instead.
fn recover_command_timeout(scope: &mut v8::HandleScope, wd: &mut CommandWatchdog) -> Option<i32> {
    wd.settle();
    scope.cancel_terminate_execution();
    match world_mut(scope).term.reason() {
        Some(TermReason::CommandTimeout) | None => {
            let _ = world_mut(scope).term.take_reason();
            None
        }
        _ => Some(world_mut(scope).take_terminated_exit()),
    }
}

/// The outcome of a fired command timeout: the session continues with the
/// timeout reply unless a run-level reason won the race.
fn timeout_outcome(scope: &mut v8::HandleScope, wd: &mut CommandWatchdog) -> Outcome {
    match recover_command_timeout(scope, wd) {
        Some(code) => Outcome::End(code),
        None => Outcome::ReplyTimedOut,
    }
}

/// A termination reached us mid-command: a per-command timeout is
/// recovered from; anything else (vm.exit, run timeout, heap cap) ends
/// the session with its own code.
fn terminated(scope: &mut v8::HandleScope, wd: &mut CommandWatchdog) -> Outcome {
    if world_mut(scope).term.reason() == Some(TermReason::CommandTimeout) {
        return timeout_outcome(scope, wd);
    }
    Outcome::End(world_mut(scope).take_terminated_exit())
}

/// The wrapper itself raised (its machinery, or the line's exception
/// escaped a replaced intrinsic): unless a run-level decision (vm.exit,
/// an outside termination) outranks it, the exception becomes an
/// `$error` reply and the session continues.
fn command_exception(
    scope: &mut v8::HandleScope,
    exc: v8::Local<v8::Value>,
    wd: &mut CommandWatchdog,
) -> Outcome {
    if world_mut(scope).term.reason() == Some(TermReason::CommandTimeout) {
        return timeout_outcome(scope, wd);
    }
    if world_mut(scope).exception_decides() {
        return encode_error(scope, exc, wd);
    }
    Outcome::End(world_mut(scope).exception_exit())
}

/// Encodes a thrown or rejection value as an `$error` line. The encoder
/// itself can still fail (guest-replaced JSON globals): a constant line
/// beats ending the session over an encoding problem.
fn encode_error(
    scope: &mut v8::HandleScope,
    exc: v8::Local<v8::Value>,
    wd: &mut CommandWatchdog,
) -> Outcome {
    match call_with(scope, &enc_err(), exc) {
        Step::Value(s) => Outcome::Reply(to_rust_string(scope, &s)),
        Step::Terminated => terminated(scope, wd),
        Step::Exception(_) => Outcome::Reply(ENCODE_FAILED.to_string()),
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
/// a command deadline that is the answer; with one, the deadline is
/// waited out on the run's termination condvar.
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
    park_deadline(scope, d)
}

/// Waits out the command deadline while the loop is idle, on the run's
/// termination condvar: the command watchdog's mark wakes us exactly at
/// the deadline, and a run-level reason (the wall-clock timeout, the heap
/// cap) wakes us the moment it lands, not at our own deadline.
fn park_deadline(scope: &mut v8::HandleScope, deadline: Instant) -> Settled {
    let term = world_mut(scope).term.clone();
    let woken = match term.wait_for(Some(deadline)) {
        Wake::Deadline => return Settled::Ended(COMMAND_TIMEOUT_CODE),
        Wake::Reason | Wake::Finished => term.reason(),
    };
    match woken {
        // Our watchdog's mark, or a wake with nothing readable: either
        // way the deadline governs the command.
        Some(TermReason::CommandTimeout) | None => Settled::Ended(COMMAND_TIMEOUT_CODE),
        Some(_) => {
            if scope.is_execution_terminating() {
                scope.cancel_terminate_execution();
            }
            Settled::Ended(world_mut(scope).take_terminated_exit())
        }
    }
}

/// Replies with the settled chain: fulfilled carries the encoded line
/// (even for a rejection of the command's promise - the err handler
/// encoded it). A rejected chain means an encoder was defeated by
/// replaced globals: the host consumes that rejection - clearing its
/// unhandled record - and answers with the reason as `$error`.
fn settled_reply(
    scope: &mut v8::HandleScope,
    p: v8::Local<v8::Promise>,
    wd: &mut CommandWatchdog,
) -> Outcome {
    if p.state() == v8::PromiseState::Fulfilled {
        let result = p.result(scope);
        return Outcome::Reply(to_rust_string(scope, &result));
    }
    let result = p.result(scope);
    forget_rejection_of(scope, &p);
    encode_error(scope, result, wd)
}

/// The wrapper's result: the encoded line, or a promise of one.
fn value_outcome(
    scope: &mut v8::HandleScope,
    v: v8::Local<v8::Value>,
    wd: &mut CommandWatchdog,
    deadline: Option<Instant>,
) -> Outcome {
    let Ok(p) = v8::Local::<v8::Promise>::try_from(v) else {
        return Outcome::Reply(to_rust_string(scope, &v));
    };
    match settle(scope, p, deadline) {
        Settled::Yes => settled_reply(scope, p, wd),
        Settled::Never => Outcome::Reply(NEVER_SETTLED.to_string()),
        Settled::Ended(COMMAND_TIMEOUT_CODE) => timeout_outcome(scope, wd),
        Settled::Ended(code) => Outcome::End(code),
    }
}

/// Evaluates one command's expression and produces its outcome. A line
/// past the engine's string limit is a reply, not a session end.
fn evaluate(
    scope: &mut v8::HandleScope,
    line: &str,
    wd: &mut CommandWatchdog,
    deadline: Option<Instant>,
) -> Outcome {
    if line.len() >= MAX_LINE {
        return Outcome::Reply(LINE_TOO_LONG.to_string());
    }
    match eval_step(scope, &build_wrapper(line)) {
        Step::Value(v) => value_outcome(scope, v, wd, deadline),
        Step::Exception(e) => command_exception(scope, e, wd),
        Step::Terminated => terminated(scope, wd),
    }
}

/// After the reply: settle timers, net and microtasks under the command
/// deadline - an expiry here stops the leftover work (the reply is
/// already out) with one stderr line, and the session continues. A guest
/// exit code wins, then an outside termination, then an unhandled
/// rejection from the command's work.
fn settle_after_reply(scope: &mut v8::HandleScope, wd: &mut CommandWatchdog) -> Option<i32> {
    let code = event_loop(scope);
    let timed_out = code == COMMAND_TIMEOUT_CODE
        || world_mut(scope).term.reason() == Some(TermReason::CommandTimeout);
    if !timed_out {
        let exit = world_mut(scope).exit_code();
        if exit >= 0 {
            return Some(exit);
        }
        return (code != 0).then_some(code);
    }
    if let Some(code) = recover_command_timeout(scope, wd) {
        return Some(code);
    }
    eprintln!("v8vm: command timed out after its reply");
    None
}

/// The watchdog is dead here; a terminate request still pending is
/// invisible to is_execution_terminating(), so with a command deadline
/// the cancel is unconditional (it is idempotent): a leftover request
/// must not poison the next command. Only our own reason is taken;
/// run-level state (a set reason, an exit code) is durable and still
/// decides the session at the next turn.
fn scrub_leftovers(scope: &mut v8::HandleScope, deadline: Option<Instant>) {
    if deadline.is_none() {
        return; // no command watchdog ever ran: nothing to scrub
    }
    if world_mut(scope).term.reason() == Some(TermReason::CommandTimeout) {
        let _ = world_mut(scope).term.take_reason();
    }
    scope.cancel_terminate_execution();
}

/// Runs one command end to end: evaluate, reply (rendezvous first, so a
/// reply is on the caller's writer before anything the guest prints
/// after it), then settle the command's leftover work under the same
/// deadline. Some(code) ends the session.
fn run_command(scope: &mut v8::HandleScope, srv: &Serve, line: &str) -> Option<i32> {
    let deadline = command_deadline(srv.timeout_ms);
    let mut wd = CommandWatchdog::start(srv.iso.clone(), world_mut(scope).term.clone(), deadline);
    let outcome = evaluate(scope, line, &mut wd, deadline);
    let mut end = None;
    let mut settle_work = true;
    let reply = match outcome {
        Outcome::Reply(text) => text,
        Outcome::ReplyTimedOut => {
            settle_work = false; // the deadline is spent
            TIMED_OUT.to_string()
        }
        Outcome::End(code) => {
            end = Some(code);
            settle_work = false;
            String::new()
        }
    };
    if end.is_none() && !send_reply(srv.replies, format!("{}\n", reply)) {
        return Some(0); // the consumer is gone; stop the session
    }
    if settle_work {
        end = end.or(settle_after_reply(scope, &mut wd));
    }
    scrub_leftovers(scope, deadline);
    wd.settle();
    end
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
}
