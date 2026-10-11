//! vm.net: the same-process message switch.

use crate::guest_cb;
use crate::world::{throw, to_rust_string, world_mut, NetMsg};

guest_cb!(net_port, |_args, rv, scope| {
    let port = {
        let world = world_mut(scope);
        let port = world.next_port;
        world.next_port += 1;
        port
    };
    rv.set(v8::Number::new(scope, port as f64).into());
});

guest_cb!(net_listen, |args, _rv, scope| {
    let port = args.get(0).number_value(scope).unwrap_or(-1.0);
    if port < 0.0 || !args.get(1).is_function() {
        throw(scope, "net.listen(port, handler) requires a function");
        return;
    }
    let cb: v8::Local<v8::Function> = args.get(1).try_into().unwrap();
    let listener = v8::Global::new(scope, cb);
    world_mut(scope).listeners.insert(port as u64, listener);
});

guest_cb!(net_send, |args, _rv, scope| {
    let from = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let to = args.get(1).number_value(scope).unwrap_or(-1.0);
    let to_port = to as u64;
    let has_listener = world_mut(scope).listeners.contains_key(&to_port);
    if to < 0.0 || !has_listener {
        throw(scope, &format!("no listener on port {}", to_port));
        return;
    }
    let payload = to_rust_string(scope, &args.get(2));
    world_mut(scope).net.push_back(NetMsg {
        from,
        to: to_port,
        payload,
    });
});

guest_cb!(net_close, |args, _rv, scope| {
    let port = args.get(0).number_value(scope).unwrap_or(-1.0) as u64;
    world_mut(scope).listeners.remove(&port);
});
