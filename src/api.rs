//! Installing the guest API into a fresh context.

use crate::console::{console_err, console_log};
use crate::exit::vm_exit;
use crate::fs_api::{
    fs_exists, fs_list, fs_mkdir, fs_read, fs_read_bytes, fs_readdir, fs_remove, fs_rename,
    fs_restore, fs_snapshot, fs_stat, fs_write,
};
use crate::net::{net_close, net_listen, net_port, net_send};
use crate::stdin::stdin_read;
use crate::timers::{clear_timer, set_interval, set_timeout};
use crate::Config;

fn make_fn<'s>(
    scope: &mut v8::HandleScope<'s>,
    cb: v8::FunctionCallback,
) -> Option<v8::Local<'s, v8::Function>> {
    v8::Function::new_raw(scope, cb)
}

fn set(
    obj: v8::Local<v8::Object>,
    scope: &mut v8::HandleScope,
    name: &str,
    value: v8::Local<v8::Value>,
) {
    let key = v8::String::new(scope, name).unwrap();
    obj.set(scope, key.into(), value);
}

fn put_fn(
    obj: v8::Local<v8::Object>,
    scope: &mut v8::HandleScope,
    name: &str,
    cb: v8::FunctionCallback,
) {
    let f = make_fn(scope, cb).unwrap();
    set(obj, scope, name, f.into());
}

pub(crate) fn install(scope: &mut v8::HandleScope, cfg: &Config) {
    let global = scope.get_current_context().global(scope);

    // The context snapshot bundled with V8 already provides a native console
    // whose output goes nowhere (no inspector is wired up); direct global
    // assignment overrides it.
    let console = v8::Object::new(scope);
    put_fn(console, scope, "log", console_log);
    put_fn(console, scope, "info", console_log);
    put_fn(console, scope, "warn", console_err);
    put_fn(console, scope, "error", console_err);
    set(global, scope, "console", console.into());

    let vm = v8::Object::new(scope);
    let version = v8::String::new(scope, v8::V8::get_version()).unwrap();
    set(vm, scope, "version", version.into());

    let args = v8::Array::new(scope, cfg.args.len() as i32);
    for (i, a) in cfg.args.iter().enumerate() {
        let s = v8::String::new(scope, a).unwrap();
        args.set_index(scope, i as u32, s.into());
    }
    set(vm, scope, "args", args.into());

    let fs = v8::Object::new(scope);
    put_fn(fs, scope, "read", fs_read);
    put_fn(fs, scope, "readBytes", fs_read_bytes);
    put_fn(fs, scope, "write", fs_write);
    put_fn(fs, scope, "list", fs_list);
    put_fn(fs, scope, "remove", fs_remove);
    put_fn(fs, scope, "mkdir", fs_mkdir);
    put_fn(fs, scope, "exists", fs_exists);
    put_fn(fs, scope, "stat", fs_stat);
    put_fn(fs, scope, "readdir", fs_readdir);
    put_fn(fs, scope, "rename", fs_rename);
    set(vm, scope, "fs", fs.into());

    let net = v8::Object::new(scope);
    put_fn(net, scope, "port", net_port);
    put_fn(net, scope, "listen", net_listen);
    put_fn(net, scope, "send", net_send);
    put_fn(net, scope, "close", net_close);
    set(vm, scope, "net", net.into());

    let stdin_obj = v8::Object::new(scope);
    put_fn(stdin_obj, scope, "read", stdin_read);
    set(vm, scope, "stdin", stdin_obj.into());

    put_fn(vm, scope, "exit", vm_exit);
    put_fn(vm, scope, "snapshot", fs_snapshot);
    put_fn(vm, scope, "restore", fs_restore);
    set(global, scope, "vm", vm.into());

    put_fn(global, scope, "setTimeout", set_timeout);
    put_fn(global, scope, "setInterval", set_interval);
    put_fn(global, scope, "clearTimeout", clear_timer);
    put_fn(global, scope, "clearInterval", clear_timer);
}
