//! End-to-end tests: drive the v8vm binary the way users and the Makefile do.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod common;

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Writes `source` to a per-test file under cargo's scratch tmp dir.
fn script(name: &str, source: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let path = dir.join(format!("{}.js", name));
    std::fs::write(&path, source).expect("write test script");
    path
}

fn v8vm(args: &[&str], stdin: &[u8]) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_v8vm"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn v8vm");
    let mut pipe = child.stdin.take().unwrap();
    let input = stdin.to_vec();
    // Feed on a thread: a large input must not deadlock against the child.
    let feeder = std::thread::spawn(move || {
        let _ = pipe.write_all(&input);
    });
    let out = child.wait_with_output().expect("wait v8vm");
    feeder.join().unwrap();
    Run {
        code: out.status.code().expect("exit code, not a signal"),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

fn run_js(name: &str, source: &str) -> Run {
    let path = script(name, source);
    v8vm(&[path.to_str().unwrap()], b"")
}

#[test]
fn console_log_and_error_use_separate_streams() {
    let r = run_js("console", "console.log('out', 1); console.error('err');");
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "out 1\n");
    assert_eq!(r.stderr, "err\n");
}

#[test]
fn vm_exit_sets_the_exit_code() {
    assert_eq!(run_js("exit", "vm.exit(7)").code, 7);
}

#[test]
fn uncaught_exception_exits_1() {
    let r = run_js("throw", "throw new Error('boom')");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("boom"), "stderr: {}", r.stderr);
}

#[test]
fn unhandled_rejection_exits_1() {
    let r = run_js("reject", "Promise.reject(new Error('nope'))");
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("nope"), "stderr: {}", r.stderr);
}

#[test]
fn timeout_kills_a_spinning_guest_with_124() {
    let path = script("spin", "for (;;) {}");
    let r = v8vm(&["--timeout=100", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 124);
}

#[test]
fn usage_errors_exit_2() {
    assert_eq!(v8vm(&[], b"").code, 2);
    assert_eq!(v8vm(&["--bogus", "x.js"], b"").code, 2);
    assert_eq!(v8vm(&["/nonexistent/entry.js"], b"").code, 2);
}

#[test]
fn timers_fire_in_due_order_and_clear_works() {
    let r = run_js(
        "timers",
        "const seen = [];
         setTimeout(() => seen.push('b'), 40);
         setTimeout(() => seen.push('a'), 5);
         const c = setTimeout(() => seen.push('cleared'), 10);
         clearTimeout(c);
         let n = 0;
         const iv = setInterval(() => { if (++n === 3) { clearInterval(iv); seen.push('iv'); } }, 20);
         setTimeout((x, y) => { seen.push(x + y); console.log(seen.join(',')); }, 200, 'x', 'y');",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "a,b,iv,xy\n");
}

#[test]
fn net_messages_arrive_one_generation_per_turn() {
    let r = run_js(
        "net",
        "const server = vm.net.port(), client = vm.net.port();
         vm.net.listen(server, (p, from) => vm.net.send(server, from, 'echo:' + p));
         vm.net.listen(client, (p) => { console.log(p); vm.net.close(client); vm.net.close(server); });
         vm.net.send(client, server, 'ping');
         try { vm.net.send(client, 999, 'x'); } catch (e) { console.log(e.message); }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "no listener on port 999\necho:ping\n");
}

#[test]
fn ramfs_read_write_list_remove() {
    let r = run_js(
        "ramfs",
        "vm.fs.write('/d/a.txt', 'A'); vm.fs.write('/d/b.txt', 'B'); vm.fs.write('/other', 'O');
         console.log(vm.fs.list('/d').join(','));
         vm.fs.remove('/d/a.txt');
         console.log(vm.fs.list('/d').join(','));
         for (const f of [() => vm.fs.read('/d/a.txt'), () => vm.fs.remove('/d/a.txt')]) {
           try { f(); console.log('no throw'); } catch (e) { console.log(e.message); }
         }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(
        r.stdout,
        "/d/a.txt,/d/b.txt\n/d/b.txt\nno such file: /d/a.txt\nno such file: /d/a.txt\n"
    );
}

#[test]
fn fs_stores_bytes_and_write_accepts_strings_arrays_and_views() {
    let r = run_js(
        "fsbytes",
        "vm.fs.write('/b.bin', new Uint8Array([255, 254, 0, 65]));
         const bs = vm.fs.readBytes('/b.bin');
         console.log(bs.constructor.name, bs.length, bs[0], bs[3]);
         const u8 = new Uint8Array([1, 2, 3, 4]);
         vm.fs.write('/v.bin', u8.buffer);
         vm.fs.write('/dv.bin', new DataView(u8.buffer));
         vm.fs.write('/off.bin', new Uint8Array(u8.buffer, 1, 2));
         vm.fs.write('/s.bin', 'hé');
         vm.fs.write('/z', new Uint8Array(0));
         const rd = (p) => Array.from(vm.fs.readBytes(p)).join(',');
         console.log(rd('/v.bin'), rd('/dv.bin'), rd('/off.bin'), rd('/s.bin'), vm.fs.readBytes('/z').length);
         const text = vm.fs.read('/b.bin');
         console.log(text.charCodeAt(0) === 0xFFFD, text.charCodeAt(3));
         try { vm.fs.write('/x', 42); console.log('no throw'); } catch (e) { console.log(e.message); }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(
        r.stdout,
        "Uint8Array 4 255 65\n1,2,3,4 1,2,3,4 2,3 104,195,169 0\ntrue 65\nwrite data must be a string, ArrayBuffer, or typed array\n"
    );
}

#[test]
fn fs_quota_flag_enforces_the_limit_and_leaves_state_unchanged() {
    // 1 MiB quota; a root-level file of quota minus the 64-byte entry
    // overhead fits exactly, one byte more does not.
    let path = script(
        "quota-cli",
        "const CAP = 1048576 - 64 - '/f'.length;
         vm.fs.write('/f', new Uint8Array(CAP));
         console.log(vm.fs.stat('/f').size);
         try { vm.fs.write('/f', new Uint8Array(CAP + 1)); console.log('no throw'); }
         catch (e) { console.log(e.message, vm.fs.stat('/f').size); }
         try { vm.fs.mkdir('/d', {recursive: true}); } catch (e) { console.log(e.message, vm.fs.exists('/d')); }",
    );
    let r = v8vm(&["--fs-quota=1", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "1048510\nfs quota exceeded 1048510\nfs quota exceeded false\n");
}

#[test]
fn fs_quota_zero_disables_the_limit_and_bad_values_exit_2() {
    let path = script("quota-off", "vm.fs.write('/f', new Uint8Array(1048576 + 1)); console.log(vm.fs.stat('/f').size);");
    let r = v8vm(&["--fs-quota=0", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "1048577\n");
    assert_eq!(v8vm(&["--fs-quota=x", "-"], b"").code, 2);
    assert_eq!(v8vm(&["--fs-quota=18446744073709551616", "-"], b"").code, 2);
}

#[test]
fn snapshot_reports_version_2_with_text_and_b64_entries() {
    let r = run_js(
        "snap-shape",
        "vm.fs.write('/t.txt', 'plain');
         vm.fs.write('/b.bin', new Uint8Array([255, 254]));
         const snap = JSON.parse(vm.snapshot());
         console.log(snap.version, snap.dirs.join(','), snap.files['/t.txt'].text, snap.files['/b.bin'].b64);",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "2 / plain //4=\n");
}

#[test]
fn dump_preload_roundtrips_binary_files_and_dirs() {
    let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("bin-roundtrip.json");
    let writer = script(
        "bin-writer",
        "vm.fs.write('/img.bin', new Uint8Array([0, 255, 254, 1, 254, 255, 0]));
         vm.fs.mkdir('/cfg', {recursive: true});
         vm.fs.write('/cfg/x', 't');",
    );
    let w = v8vm(&[&format!("--dump={}", dump.display()), writer.to_str().unwrap()], b"");
    assert_eq!(w.code, 0, "stderr: {}", w.stderr);
    let reader = script(
        "bin-reader",
        "const b = vm.fs.readBytes('/img.bin');
         console.log(Array.from(b).join(','), vm.fs.stat('/img.bin').size, vm.fs.exists('/cfg'), vm.fs.read('/cfg/x'), vm.fs.stat('/cfg').type);",
    );
    let r = v8vm(&[&format!("--preload={}", dump.display()), reader.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "0,255,254,1,254,255,0 7 true t dir\n");
}

#[test]
fn preload_accepts_the_legacy_snapshot_fixture() {
    let fixture =
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/legacy-snapshot.json");
    let path = script(
        "legacy-reader",
        "console.log(vm.fs.read('/greeting.txt'), vm.fs.exists('/lib'), vm.fs.list('/').join(','));
         vm.run('/lib/util.js');
         console.log(globalThis.seven);",
    );
    let r = v8vm(&[&format!("--preload={}", fixture), path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "hello from the old format true /greeting.txt,/lib/util.js\n7\n");
}

#[test]
fn restore_in_the_guest_accepts_both_formats_and_reports_bad_ones() {
    let r = run_js(
        "restore-formats",
        "vm.fs.write('/now', 'x');
         vm.restore('{\"/old\":\"legacy\"}');
         console.log(vm.fs.read('/old'), vm.fs.exists('/now'));
         const snap = vm.snapshot();
         vm.fs.remove('/old', {recursive: true});
         vm.restore(snap);
         console.log(vm.fs.read('/old'));
         try { vm.restore('[]'); } catch (e) { console.log(e.message); }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "legacy true\nlegacy\ninvalid snapshot: expected an object\n");
}


#[test]
fn fs_write_rejects_detached_but_out_of_bounds_views_write_empty() {
    let r = run_js(
        "fs-buffers",
        "const ab = new ArrayBuffer(8);
         const view = new Uint8Array(ab);
         ab.transfer(); // detaches ab and view with it
         try { vm.fs.write('/d1', view); console.log('view no throw'); }
         catch (e) { console.log(e.message); }
         try { vm.fs.write('/d2', ab); console.log('ab no throw'); }
         catch (e) { console.log(e.message); }
         const rab = new ArrayBuffer(16, {maxByteLength: 32});
         const oob = new Uint8Array(rab, 8, 4);
         rab.resize(4); // the view no longer fits the buffer
         try { vm.fs.write('/d3', oob); console.log('oob wrote', vm.fs.readBytes('/d3').length, 'bytes'); }
         catch (e) { console.log('oob threw', e.message); }
         // shared buffers and views over them are accepted
         const sab = new SharedArrayBuffer(4);
         const sv = new Uint8Array(sab);
         sv.set([9, 8, 7, 6]);
         vm.fs.write('/s1', sv);
         vm.fs.write('/s2', sab);
         console.log(Array.from(vm.fs.readBytes('/s1')).join(','), Array.from(vm.fs.readBytes('/s2')).join(','));
         try { vm.fs.write('/x', 42); console.log('no throw'); } catch (e) { console.log(e.message); }",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "cannot write from a detached ArrayBuffer\ncannot write from a detached ArrayBuffer\noob wrote 0 bytes\n9,8,7,6 9,8,7,6\nwrite data must be a string, ArrayBuffer, or typed array\n"
    );
}

#[test]
fn fs_partially_and_fully_out_of_bounds_views_write_empty() {
    // A view left out of bounds by a resizable shrink has byteLength 0,
    // exactly as the guest sees it, so the write stores an empty file.
    let r = run_js(
        "fs-oob-views",
        "const cases = [];
         const mk = (off, len) => { const r = new ArrayBuffer(16, {maxByteLength: 32});
                                    const v = new Uint8Array(r, off, len); r.resize(4); return v; };
         vm.fs.write('/partial', mk(2, 8));
         vm.fs.write('/far', mk(8, 4));
         const dv = (() => { const r = new ArrayBuffer(16, {maxByteLength: 32});
                             const v = new DataView(r, 8, 4); r.resize(4); return v; })();
         vm.fs.write('/dv', dv);
         console.log(vm.fs.readBytes('/partial').length, vm.fs.readBytes('/far').length, vm.fs.readBytes('/dv').length);
         // regrown: the view is valid again and writes its real bytes
         const reg = (() => { const r = new ArrayBuffer(16, {maxByteLength: 32});
                              const v = new Uint8Array(r, 8, 4);
                              r.resize(4); r.resize(16); v.set([1,2,3,4]); return v; })();
         vm.fs.write('/reg', reg);
         console.log(Array.from(vm.fs.readBytes('/reg')).join(','));",
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "0 0 0\n1,2,3,4\n");
}

#[test]
fn fs_paths_reject_lone_surrogates_but_allow_real_replacement_chars() {
    let r = run_js(
        "fs-surrogates",
        "try { vm.fs.write('/\\uD800', 'x'); console.log('no throw'); } catch (e) { console.log(e.message); }
         vm.fs.write('/real', 'x');
         console.log(vm.fs.exists('/\\uD801'));
         try { vm.fs.read('/\\uD800'); } catch (e) { console.log('read:', e.message); }
         // a genuine U+FFFD is a legal path byte and must round-trip
         vm.fs.write('/a\\uFFFDb', 'x');
         console.log(vm.fs.exists('/a\\uFFFDb'), vm.fs.read('/a\\uFFFDb'));",
    );
    assert_eq!(r.code, 0);
    assert_eq!(
        r.stdout,
        "path contains lone surrogates\nfalse\nread: path contains lone surrogates\ntrue x\n"
    );
}

#[test]
fn fs_replacement_char_paths_round_trip_through_dump_and_preload() {
    let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fffd-path.json");
    let writer = script(
        "fffd-writer",
        "vm.fs.write('/a\\uFFFDb', 'x'); vm.fs.mkdir('/d\\uFFFD', {recursive: true}); vm.fs.write('/d\\uFFFD/f', 'y');",
    );
    let w = v8vm(&[&format!("--dump={}", dump.display()), writer.to_str().unwrap()], b"");
    assert_eq!(w.code, 0, "stderr: {}", w.stderr);
    let reader = script(
        "fffd-reader",
        "console.log(vm.fs.read('/a\\uFFFDb'), vm.fs.exists('/d\\uFFFD'), vm.fs.read('/d\\uFFFD/f'));\n         console.log(vm.fs.list('/').includes('/a\\uFFFDb'), vm.fs.readdir('/').includes('d\\uFFFD'));\n         console.log(vm.snapshot().includes('a\\uFFFDb'));",
    );
    let r = v8vm(&[&format!("--preload={}", dump.display()), reader.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "x true y\ntrue true\ntrue\n");
}

#[test]
fn fs_path_arguments_propagate_to_string_exceptions() {
    let r = run_js(
        "fs-tostring",
        "try { vm.fs.read({ toString() { throw new Error('boom'); } }); console.log('no throw'); }
         catch (e) { console.log(e.message); }
         try { vm.fs.write({ toString() { throw new Error('bad path'); } }, 'x'); console.log('no throw'); }
         catch (e) { console.log(e.message); }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "boom\nbad path\n");
}

#[test]
fn fs_read_and_read_bytes_of_a_directory_throw() {
    let r = run_js(
        "fs-dirs",
        "vm.fs.mkdir('/d', {recursive: true});
         for (const f of [() => vm.fs.read('/d'), () => vm.fs.readBytes('/d')]) {
           try { f(); console.log('no throw'); } catch (e) { console.log(e.message); }
         }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "is a directory: /d\nis a directory: /d\n");
}

#[test]
fn fs_remove_returns_undefined() {
    let r = run_js(
        "fs-remove-undef",
        "vm.fs.write('/f', 'x');
         const r = vm.fs.remove('/f');
         console.log(typeof r);",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "undefined\n");
}

#[test]
fn restore_rejects_bad_versions_and_non_canonical_base64() {
    let r = run_js(
        "restore-strict",
        "try { vm.restore('{\"version\":3,\"files\":{}}'); } catch (e) { console.log(e.message); }
         try { vm.restore('{\"version\":2,\"files\":{\"/x\":{\"b64\":\"AA==AAAA\"}}}'); } catch (e) { console.log(e.message); }
         try { vm.restore('{\"version\":2,\"files\":{\"/x\":{\"b64\":\"AB==\"}}}'); } catch (e) { console.log(e.message); }
         vm.restore('{\"version\":2,\"files\":{}}');
         console.log('empty v2 ok');",
    );
    assert_eq!(r.code, 0);
    assert_eq!(
        r.stdout,
        "unsupported snapshot version 3\ninvalid snapshot: bad base64\ninvalid snapshot: bad base64\nempty v2 ok\n"
    );
}

#[test]
fn fs_quota_flag_multiplication_overflow_exits_2() {
    // 2^64 bytes would need an mb value whose << 20 overflows u64.
    assert_eq!(v8vm(&["--fs-quota=17592186044416", "-"], b"").code, 2);
    assert_eq!(v8vm(&["--fs-quota=17592186044415", "-"], b"").code, 0);
}

#[test]
fn preload_accepts_relative_legacy_keys() {
    let fixture =
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/legacy-relative.json");
    let path = script(
        "legacy-relative",
        "console.log(vm.fs.read('/rel/a.txt'), vm.fs.read('/abs.txt'), vm.fs.list('/').join(','));",
    );
    let r = v8vm(&[&format!("--preload={}", fixture), path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "relative key ok absolute key ok /abs.txt,/rel/a.txt\n");
}

#[test]
#[ignore] // memory-heavy: run with `make test-heavy`
fn fs_file_too_large_to_read_as_a_string() {
    // Above V8's maximum string length (~536.87 M chars), read must throw
    // a normal guest Error while readBytes still works. Memory-heavy on
    // purpose: nothing smaller can cross the engine's string limit.
    let path = script(
        "fs-huge-read",
        "vm.fs.write('/f', new Uint8Array(540 * 1024 * 1024));
         try { vm.fs.read('/f'); console.log('no throw'); }
         catch (e) { console.log(e.message); }
         console.log(vm.fs.readBytes('/f').length);",
    );
    let r = v8vm(&["--fs-quota=0", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "file too large to read as a string\n566231040\n");
}

#[test]
fn restore_rejects_duplicate_paths_and_keeps_the_quota_honest() {
    // bypass.js shape, scaled: fill the quota, then try to shrink the
    // file with many spellings of its own path.
    let path = script(
        "dup-bypass",
        "const SIZE = 1048576 - 66;
         vm.fs.write('/a', new Uint8Array(SIZE));
         const files = {};
         for (let i = 1; i <= 50; i++) files['/'.repeat(i) + 'a'] = { text: '' };
         try { vm.restore(JSON.stringify({ version: 2, files })); console.log('restored'); }
         catch (e) { console.log(e.message); }
         console.log('a size', vm.fs.stat('/a').size);
         try { vm.fs.write('/big', new Uint8Array(1048576)); console.log('over-quota write ok'); }
         catch (e) { console.log(e.message); }
         try { vm.restore('{\"b\":\"1\",\"/b\":\"2\"}'); } catch (e) { console.log(e.message); }",
    );
    let r = v8vm(&["--fs-quota=1", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        "duplicate path in snapshot: /a\na size 1048510\nfs quota exceeded\nduplicate path in snapshot: /b\n"
    );
}

#[test]
fn rename_converts_arguments_one_at_a_time() {
    let r = run_js(
        "rename-order",
        "let secondRan = false;
         try { vm.fs.rename({ toString() { throw new Error('first'); } },
                            { toString() { secondRan = true; throw new Error('second'); } }); }
         catch (e) { console.log('caught', e.message, 'secondRan', secondRan); }
         try { vm.fs.rename('/x', { toString() { throw new Error('to-throws'); } }); }
         catch (e) { console.log('caught', e.message); }",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "caught first secondRan false\ncaught to-throws\n");
}

#[test]
fn write_probes_string_lengths_against_the_quota() {
    // A multi-byte string is charged by its UTF-8 length, and rejected
    // before the copy, like buffers are.
    let path = script(
        "str-probe",
        "try { vm.fs.write('/s', '\\u00e9'.repeat(600000)); console.log('no throw'); }
         catch (e) { console.log(e.message); }
         vm.fs.write('/s', '\\u00e9'.repeat(100000));
         console.log(vm.fs.stat('/s').size);
         try { vm.fs.write('/t', { toString() { throw new Error('never runs'); } }); }
         catch (e) { console.log(e.message.slice(0, 20)); }",
    );
    let r = v8vm(&["--fs-quota=1", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "fs quota exceeded\n200000\nwrite data must be a\n");
}

#[test]
fn a_failed_dump_exits_74_when_the_guest_succeeded() {
    let path = script("dump-fail", "vm.fs.write('/f', 'x');");
    // Writing the dump into a directory fails; the guest itself was fine.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let r = v8vm(&[&format!("--dump={}", dir.display()), path.to_str().unwrap()], b"");
    assert_eq!(r.code, 74);
    assert!(r.stderr.contains("cannot write dump"), "stderr: {}", r.stderr);
    // A guest that already failed keeps its own exit code.
    let bad = script("dump-fail-2", "throw new Error('boom');");
    let r2 = v8vm(&[&format!("--dump={}", dir.display()), bad.to_str().unwrap()], b"");
    assert_eq!(r2.code, 1);
    // A healthy dump still exits 0.
    let ok_dump = dir.join("dump-ok.json");
    let r3 = v8vm(&[&format!("--dump={}", ok_dump.display()), path.to_str().unwrap()], b"");
    assert_eq!(r3.code, 0);
}

#[test]
fn full_quota_control_characters_survive_a_snapshot_round_trip() {
    // ctrl.js shape, scaled down: JSON escapes control characters to six
    // bytes each, so the gate must be 6x the quota for a full-quota
    // filesystem of them to restore from its own snapshot.
    let path = script(
        "ctrl-roundtrip",
        "const base = '/' + '\\u0001'.repeat(3990);
         let n = 0;
         try { for (;;) { vm.fs.write(base + n, '\\u0001\\u0001\\u0001'); n++; } }
         catch (e) { }
         console.log('quota full after', n, 'files');
         const snap = vm.snapshot();
         vm.fs.remove(base + '0', {});
         vm.restore(snap);
         console.log('restored', vm.fs.stat('/').size === n, vm.fs.read(base + '0').length);",
    );
    // A 1 MiB quota: ~260 long control-character keys fill it, and their
    // ~6 MB snapshot (control bytes escape to six each) must clear the
    // 6x gate and restore cleanly - a 4x gate would reject it.
    let r = v8vm(&["--fs-quota=1", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let mut lines = r.stdout.lines();
    let full = lines.next().unwrap();
    assert!(full.starts_with("quota full after "), "stdout: {}", r.stdout);
    let n: usize = full
        .trim_start_matches("quota full after ")
        .trim_end_matches(" files")
        .parse()
        .expect("file count");
    assert!(n > 200, "expected the quota to hold many entries: {}", n);
    assert_eq!(lines.next().unwrap(), "restored true 3");
}

#[test]
fn dump_is_written_for_every_ending() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let dump_has_f = |name: &str| {
        let path = dir.join(name);
        let json = std::fs::read_to_string(&path).unwrap_or_default();
        path.exists() && json.contains("\"/f\"")
    };
    let endings = [
        ("vm.exit(0) at top level", "vm.fs.write('/f','x'); vm.exit(0);", 0),
        ("vm.exit(0) in a timer", "vm.fs.write('/f','x'); setTimeout(() => vm.exit(0), 5);", 0),
        ("vm.exit(0) in a microtask", "vm.fs.write('/f','x'); Promise.resolve().then(() => vm.exit(0));", 0),
        ("vm.exit(3)", "vm.fs.write('/f','x'); vm.exit(3);", 3),
        (
            "timeout",
            "vm.fs.write('/f','x'); for (;;) {}",
            124,
        ),
    ];
    for (i, (what, src, want)) in endings.iter().enumerate() {
        let script_path = script(&format!("ending-{}", i), src);
        let dump = dir.join(format!("ending-{}.json", i));
        let _ = std::fs::remove_file(&dump); // only this run's dump counts
        let r = v8vm(
            &[
                "--timeout=200",
                &format!("--dump={}", dump.display()),
                script_path.to_str().unwrap(),
            ],
            b"",
        );
        assert_eq!(r.code, *want, "{}: exit code (stderr: {})", what, r.stderr);
        assert!(dump_has_f(&format!("ending-{}.json", i)), "{}: dump missing /f", what);
    }
}

#[test]
fn fs_error_messages_truncate_huge_paths() {
    let r = run_js(
        "huge-path-msg",
        "const p = 'a'.repeat(300000);
         try { vm.fs.read(p); console.log('no throw'); }
         catch (e) { console.log(e.message.length < 300, e.message.startsWith('path must be absolute: a'), e.message.endsWith('...')); }
         console.log(vm.fs.exists(p));",
    );
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "true true true\nfalse\n");
}

#[test]
fn dump_then_preload_restores_the_filesystem() {
    let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("roundtrip.json");
    let writer = script("dump-writer", "vm.fs.write('/k', 'v\\u00e9');");
    let w = v8vm(&[&format!("--dump={}", dump.display()), writer.to_str().unwrap()], b"");
    assert_eq!(w.code, 0);

    let reader = script("preload-reader", "console.log(vm.fs.read('/k'));");
    let r = v8vm(&[&format!("--preload={}", dump.display()), reader.to_str().unwrap()], b"");
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "vé\n");
}

#[test]
fn missing_preload_exits_2() {
    let path = script("noop", "");
    let r = v8vm(&["--preload=/nonexistent.json", path.to_str().unwrap()], b"");
    assert_eq!(r.code, 2);
}

#[test]
fn vm_run_evals_a_file_from_the_ramfs() {
    let r = run_js(
        "vmrun",
        "vm.fs.write('/lib.js', 'globalThis.twice = (x) => x * 2;'); vm.run('/lib.js'); console.log(twice(21));",
    );
    assert_eq!(r.stdout, "42\n");
}

const STDIN_ECHO: &str = "
(async () => {
  let total = '';
  for (;;) { const c = await vm.stdin.read(); if (c === '') break; total += c; }
  console.log(total.length + ':' + (total === EXPECTED ? 'same' : 'different'));
})();";

#[test]
fn stdin_keeps_multibyte_characters_whole_across_read_boundaries() {
    // A 2-, 3- and 4-byte character each placed so a 64 KiB read can cut it.
    let mut payload = String::new();
    for ch in ["é", "日", "😀"] {
        payload.push_str(&"a".repeat(65535 - payload.len() % 65536));
        payload.push_str(ch);
        payload.push_str(&"b".repeat(100));
    }
    let expected = payload.encode_utf16().count();
    let source = format!(
        "const EXPECTED = {}; {}",
        serde_json::to_string(&payload).unwrap(),
        STDIN_ECHO
    );
    let path = script("stdin-multibyte", &source);
    let r = v8vm(&[path.to_str().unwrap()], payload.as_bytes());
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, format!("{}:same\n", expected));
}

#[test]
fn stdin_from_entry_dash_is_at_eof() {
    let r = v8vm(
        &["-"],
        b"vm.stdin.read().then((c) => console.log('[' + c + ']'));",
    );
    assert_eq!(r.stdout, "[]\n");
}

/// Each serve test gets its own entry file: they run in parallel and a
/// shared path races writer against reader (an empty read silently skips
/// the entry script).
fn serve(entry: &str, commands: &str) -> Run {
    let path = script(entry, "globalThis.state = { n: 0 };");
    v8vm(&["--serve", path.to_str().unwrap()], commands.as_bytes())
}

#[test]
fn serve_answers_one_json_line_per_command() {
    let r = serve("serve-answers", "1 + 1\nstate.n += 5\n({a: [1, 'x']})\n");
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "2\n5\n{\"a\":[1,\"x\"]}\n");
}

#[test]
fn serve_accepts_a_trailing_line_comment() {
    let r = serve("serve-comment", "40 + 2 // the answer\n");
    assert_eq!(r.stdout, "42\n");
}

#[test]
fn serve_prints_null_for_undefined_results() {
    let r = serve("serve-null", "undefined\n(() => 1)\nstate.missing\n");
    assert_eq!(r.stdout, "null\nnull\nnull\n");
}

#[test]
fn serve_reports_thrown_errors_as_json_and_keeps_going() {
    let r = serve("serve-errors", "(() => { throw new Error('bad') })()\n7\n");
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, "{\"$error\":\"bad\"}\n7\n");
}

#[test]
fn serve_settles_timers_between_commands() {
    let r = serve("serve-timers", "setTimeout(() => { state.n = 9 }, 5)\nstate.n\n");
    assert_eq!(r.stdout, "1\n9\n"); // first line is the timer id
}

#[test]
fn serve_replies_reach_stdout_before_later_guest_output() {
    // Rendezvous: reply N must be on stdout before anything the guest prints
    // after reply N. A queued net message is delivered by the event loop's
    // first phase with no timer floor, so a missing ack shows up as a
    // misplaced line; the 0 ms timer covers the scheduled case. Loop to
    // catch the race.
    let path = script("serve-order", "globalThis.n = 0;");
    // One expression per command: the comma operator schedules work and
    // yields the reply value.
    let commands = b"(vm.net.listen(1, () => console.log('n1')), vm.net.send(1, 1, 'x'), 'A')\n(vm.net.listen(2, () => console.log('n2')), vm.net.send(2, 2, 'x'), 'B')\nsetTimeout(() => console.log('t'), 0), 'C'\n";
    let expected = "\"A\"\nn1\n\"B\"\nn2\n\"C\"\nt\n";
    let mut violations = 0;
    for _ in 0..50 {
        let r = v8vm(&["--serve", path.to_str().unwrap()], commands);
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        if r.stdout != expected {
            violations += 1;
        }
    }
    assert_eq!(violations, 0, "reply ordering violated {} of 50 times", violations);
}

/// Like `v8vm`, but kills the child and fails if it outlives `limit`, so a
/// hang regression is a test failure instead of a stuck test run.
fn v8vm_bounded(args: &[&str], stdin: &[u8], limit: std::time::Duration) -> Run {
    v8vm_bounded_started(args, stdin, limit, |_| {})
}

/// `v8vm_bounded` that also calls `started` with the child's pid right after
/// the spawn, before any input is sent.
fn v8vm_bounded_started(
    args: &[&str],
    stdin: &[u8],
    limit: std::time::Duration,
    started: impl FnOnce(u32),
) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_v8vm"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn v8vm");
    started(child.id());
    let mut pipe = child.stdin.take().unwrap();
    let input = stdin.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = pipe.write_all(&input);
        // pipe drops here: EOF without any trailing newline from us
    });
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll v8vm") {
            break status;
        }
        if start.elapsed() > limit {
            child.kill().expect("kill hung v8vm");
            child.wait().expect("reap v8vm");
            panic!("v8vm still running after {:?}", limit);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    feeder.join().unwrap();
    let out = child.wait_with_output().expect("collect v8vm output");
    Run {
        code: status.code().expect("exit code, not a signal"),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

#[test]
fn serve_answers_a_final_command_without_a_trailing_newline_and_exits() {
    let path = script("serve-no-newline", "globalThis.s = 1;");
    let r = v8vm_bounded(
        &["--serve", path.to_str().unwrap()],
        b"1+1",
        std::time::Duration::from_secs(10),
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "2\n");
}

// ---- --sandbox (OS self-containment) ----

const SANDBOX_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The flag scripts gate their skips on: exit 0 with a word when sandboxing
/// is possible and 2 with the reason otherwise. The kernel is queried
/// independently of the library under test, so a lying answer fails.
#[test]
fn sandbox_check_matches_the_kernel() {
    let r = v8vm(&["--sandbox-check"], b"");
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if common::kernel_supports_sandbox() {
        assert_eq!((r.code, r.stdout.trim()), (0, "supported"), "stderr: {}", r.stderr);
        return;
    }
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("--sandbox is not available"), "stderr: {}", r.stderr);
}

/// The probe flag only means something as the sole argument. Anywhere before
/// the entry it is a usage error, so a typo cannot turn a run into a check.
#[test]
fn sandbox_check_before_the_entry_or_with_other_flags_is_a_usage_error() {
    let path = script("sb-check-misplaced", "console.log('ran');");
    let entry = path.to_str().unwrap();
    for args in [vec!["--sandbox-check", "--bogus"], vec!["--sandbox-check", entry], vec!["--", "--sandbox-check"]] {
        let r = v8vm(&args, b"");
        assert_eq!(r.code, 2, "{:?} stderr: {}", args, r.stderr);
        assert_eq!(r.stdout, "", "{:?}: no probe answer and no guest run", args);
        assert!(r.stderr.contains("--sandbox-check must be the only argument"), "stderr: {}", r.stderr);
    }
}

/// After the entry (with or without `--`) it is an ordinary guest argument:
/// the script runs and sees it in vm.args.
#[test]
fn sandbox_check_after_the_entry_is_a_guest_argument() {
    let path = script("sb-check-arg", "console.log(JSON.stringify(vm.args));");
    let entry = path.to_str().unwrap();
    for args in [vec![entry, "--sandbox-check"], vec![entry, "--", "--sandbox-check"]] {
        let r = v8vm(&args, b"");
        assert_eq!(r.code, 0, "{:?} stderr: {}", args, r.stderr);
        assert_eq!(r.stdout, "[\"--sandbox-check\"]\n", "{:?}", args);
    }
}

/// A dump the sandbox cannot anchor is refused while it prepares, before any
/// kernel layer is installed, so these hold on every kernel.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod sandbox_prepare {
    use super::*;

    #[test]
    fn sandbox_refuses_a_dump_it_cannot_prepare() {
        let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("no-such-dir/sb.json");
        let path = script("sb-refused", "console.log('ran');");
        let dump_arg = format!("--dump={}", dump.display());
        let r = v8vm_bounded(&["--sandbox", &dump_arg, path.to_str().unwrap()], b"", SANDBOX_LIMIT);
        assert_eq!(r.code, 2);
        assert_eq!(r.stdout, "", "guest must not run");
        assert!(r.stderr.contains("sandbox"), "stderr: {}", r.stderr);
    }

    #[test]
    fn sandbox_refuses_a_dump_that_is_not_a_regular_file() {
        // The child's stdout is a pipe, so /dev/stdout is one too.
        let path = script("sb-pipe-dump", "console.log('ran');");
        let r = v8vm_bounded(
            &["--sandbox", "--dump=/dev/stdout", path.to_str().unwrap()],
            b"",
            SANDBOX_LIMIT,
        );
        assert_eq!(r.code, 2);
        assert_eq!(r.stdout, "", "guest must not run");
        assert!(r.stderr.contains("--dump must be a regular file"), "stderr: {}", r.stderr);
    }
}

/// Whether this kernel can sandbox. Where it cannot, the test must then
/// check the refusal with `assert_refused` and return, and a skip notice is
/// printed. V8VM_REQUIRE_SANDBOX=1 makes such a kernel a failure instead.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn sandbox_available(test: &str) -> bool {
    if common::kernel_supports_sandbox() {
        return true;
    }
    assert!(
        !common::sandbox_required(),
        "V8VM_REQUIRE_SANDBOX=1 but the kernel reports Landlock ABI {}, need {}",
        common::landlock_abi(),
        common::REQUIRED_LANDLOCK_ABI
    );
    common::skipped(test);
    false
}

/// On a kernel without Landlock ABI 6 the contract is a clear exit 2 and no
/// guest run, never an unsandboxed run.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn assert_refused(args: &[&str], stdin: &[u8]) {
    let r = v8vm_bounded(args, stdin, SANDBOX_LIMIT);
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "", "guest must not run");
    assert!(r.stderr.contains("v8vm: --sandbox:"), "stderr: {}", r.stderr);
    assert!(r.stderr.contains("Landlock"), "stderr: {}", r.stderr);
}

/// Runs v8vm under `--sandbox`; None means the kernel cannot sandbox and the
/// refusal has been checked.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn sandboxed(test: &str, args: &[&str], stdin: &[u8]) -> Option<Run> {
    let mut full = vec!["--sandbox"];
    full.extend_from_slice(args);
    if sandbox_available(test) {
        return Some(v8vm_bounded(&full, stdin, SANDBOX_LIMIT));
    }
    assert_refused(&full, stdin);
    None
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod sandbox_run {
    use super::*;

    #[test]
    fn sandbox_runs_a_plain_guest_end_to_end() {
        let path = script(
            "sb-hello",
            "const arr = new Array(100000).fill('x'.repeat(50));
             setTimeout(() => console.log('tick ' + arr.length), 5);
             console.log('hello');",
        );
        let Some(r) = sandboxed("sandbox_runs_a_plain_guest_end_to_end", &[path.to_str().unwrap()], b"")
        else {
            return;
        };
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        assert_eq!(r.stdout, "hello\ntick 100000\n");
    }

    #[test]
    fn sandbox_timeout_kills_a_spinning_guest_with_124() {
        let path = script("sb-spin", "for (;;) {}");
        let args = ["--timeout=100", path.to_str().unwrap()];
        let Some(r) = sandboxed("sandbox_timeout_kills_a_spinning_guest_with_124", &args, b"") else {
            return;
        };
        assert_eq!(r.code, 124);
    }

    #[test]
    fn sandbox_dump_writes_the_allowed_file() {
        let dump = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sb-dump.json");
        // Start from no file: a leftover snapshot from an earlier run would
        // mask a broken dump write.
        let _ = std::fs::remove_file(&dump);
        let path = script("sb-dump-guest", "vm.fs.write('/g', 'v');");
        let dump_arg = format!("--dump={}", dump.display());
        let args = [dump_arg.as_str(), path.to_str().unwrap()];
        let Some(r) = sandboxed("sandbox_dump_writes_the_allowed_file", &args, b"") else {
            return;
        };
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&dump).expect("dump file")).expect("dump json");
        assert_eq!(
            json,
            serde_json::json!({"dirs": ["/"], "files": {"/g": {"text": "v"}}, "version": 2})
        );
    }

    /// Waits for the child to carry a seccomp filter, so a serve run that
    /// silently skipped the sandbox fails instead of passing on output alone.
    fn wait_for_seccomp(pid: u32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let status = std::fs::read_to_string(format!("/proc/{}/status", pid)).unwrap_or_default();
            if status.contains("Seccomp:\t2") {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "serve never became sandboxed: {}", status);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn sandbox_serve_answers_commands_over_stdin() {
        let test = "sandbox_serve_answers_commands_over_stdin";
        let path = script("sb-serve", "globalThis.state = { n: 0 };");
        let args = ["--sandbox", "--serve", path.to_str().unwrap()];
        let commands = b"1 + 1\nstate.n += 5\n";
        if !sandbox_available(test) {
            assert_refused(&args, commands);
            return;
        }
        let r = v8vm_bounded_started(&args, commands, SANDBOX_LIMIT, wait_for_seccomp);
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        assert_eq!(r.stdout, "2\n5\n");
    }
}

/// Where the layers cannot be built, `--sandbox` must still be a clear
/// refusal and never an unsandboxed run.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
#[test]
fn sandbox_flag_is_refused_on_unsupported_platforms() {
    let path = script("sb-unsupported", "console.log('ran');");
    let r = v8vm_bounded(&["--sandbox", path.to_str().unwrap()], b"", SANDBOX_LIMIT);
    assert_eq!(r.code, 2);
    assert_eq!(r.stdout, "", "guest must not run");
    assert!(
        r.stderr.contains("--sandbox is only supported on Linux x86_64"),
        "stderr: {}",
        r.stderr
    );
}
