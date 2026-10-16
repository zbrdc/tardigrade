//! vm.exit: stop the program with a code.

use crate::guest_cb;
use crate::world::world_mut;

guest_cb!(vm_exit, |args, _rv, scope| {
    let code = if args.length() > 0 {
        args.get(0).int32_value(scope).unwrap_or(0)
    } else {
        0
    };
    world_mut(scope).set_exit_code(code);
    scope.as_mut().terminate_execution();
});
