//! vm.fs: the RAM filesystem. Throwing read/remove, as documented.

use crate::diag::{throw, to_rust_string};
use crate::guest_cb;
use crate::world::world_mut;

guest_cb!(fs_read, |args, rv, scope| {
    let path = to_rust_string(scope, &args.get(0));
    // The clone releases the world borrow before the scope is used again.
    let contents = world_mut(scope).fs.get(&path).cloned();
    match contents {
        Some(contents) => {
            let s =
                v8::String::new_from_utf8(scope, contents.as_bytes(), v8::NewStringType::Normal)
                    .unwrap();
            rv.set(s.into());
        }
        None => throw(scope, &format!("no such file: {}", path)),
    }
});

guest_cb!(fs_write, |args, _rv, scope| {
    let path = to_rust_string(scope, &args.get(0));
    let data = to_rust_string(scope, &args.get(1));
    world_mut(scope).fs.insert(path, data);
});

guest_cb!(fs_list, |args, rv, scope| {
    let mut dir = if args.length() > 0 && !args.get(0).is_undefined() {
        to_rust_string(scope, &args.get(0))
    } else {
        "/".to_string()
    };
    if !dir.ends_with('/') {
        dir.push('/');
    }
    let names: Vec<String> = world_mut(scope)
        .fs
        .keys()
        .filter(|p| p.starts_with(&dir))
        .cloned()
        .collect();
    let array = v8::Array::new(scope, names.len() as i32);
    for (i, p) in names.iter().enumerate() {
        let s = v8::String::new(scope, p).unwrap();
        array.set_index(scope, i as u32, s.into());
    }
    rv.set(array.into());
});

guest_cb!(fs_remove, |args, _rv, scope| {
    let path = to_rust_string(scope, &args.get(0));
    if world_mut(scope).fs.remove(&path).is_none() {
        throw(scope, &format!("no such file: {}", path));
    }
});
