// The ts-rust WebAssembly build running INSIDE a tardigrade guest: it type-checks
// (and, per the project's tsconfig, transpiles) /app, with the whole
// compiler under tardigrade's timeout and --sandbox (--memory does not cap the
// wasm linear memory; see the README's Caveats). The host side is
// examples/ts-in-guest/run.sh; the wasm artifact is never committed (see
// examples/ts-in-guest/README.md).
//
// The module ABI is crates/ts_wasm in the ts-rust repo: two imports
// (ts_host.fs, ts_host.fs_take) bridge its file system onto vm.fs, a small
// WASI preview1 shim answers the rest, and the request is
// NUL-separated [cwd, flags, tsc args...]. Type errors print as tsc-style
// lines and the exit code is tsc's own (2 on type errors), so a harness
// treats this exactly like the host-side loop's FAIL turns.

const enc = new TextEncoder();
const dec = new TextDecoder();
const SUCCESS = 0, EBADF = 8, ENOSYS = 52;
const WASM_PATH = vm.args[0] || "/tsc/ts_rust.wasm";
const PROJECT_DIR = vm.args[1] || "/app";

class WasiExit extends Error {
  constructor(code) { super("exit " + code); this.code = code; }
}

function concat(a, b) {
  const out = new Uint8Array(a.length + b.length);
  out.set(a); out.set(b, a.length);
  return out;
}

// One ts_host file operation (crates/ts_wasm/src/host.rs) over vm.fs.
// undefined means the operation failed; a Uint8Array is its result bytes.
function fsCall(op, request) {
  const text = dec.decode(request);
  const isDir = (p) => vm.fs.exists(p) && vm.fs.stat(p).type === "dir";
  switch (op) {
    case 0:  // Read
      return vm.fs.exists(text) ? vm.fs.readBytes(text) : undefined;
    case 1: {  // Stat: "<kind> <size> <mtime ns>"; the RAM fs has no mtime
      if (!vm.fs.exists(text)) return undefined;
      const st = vm.fs.stat(text);
      return enc.encode((st.type === "dir" ? "d" : "f") + " " + st.size + " 0");
    }
    case 2: {  // ReadDir: "<kind><name>" entries, NUL-ended
      if (!isDir(text)) return undefined;
      const base = text.endsWith("/") ? text : text + "/";
      let out = "";
      for (const name of vm.fs.readdir(text)) {
        out += (isDir(base + name) ? "d" : "f") + name + "\0";
      }
      return enc.encode(out);
    }
    case 3:  // Realpath: every RAM-fs path is already real
      return vm.fs.exists(text) ? enc.encode(text) : undefined;
    case 4:  // Write (tsc emit lands here)
    case 5: {  // Append, which vm.fs only has via read-modify-write
      const nul = request.indexOf(0);
      const path = dec.decode(request.subarray(0, nul));
      const data = request.subarray(nul + 1);
      vm.fs.write(path, op === 5 && vm.fs.exists(path)
        ? concat(vm.fs.readBytes(path), data) : data);
      return new Uint8Array(0);
    }
    case 6:  // Remove (recursive by contract)
      vm.fs.remove(text, { recursive: true });
      return new Uint8Array(0);
    case 7:  // Chtimes: nothing to set, report success
      return new Uint8Array(0);
  }
  return undefined;
}

// Every wasm memory access re-derives its view: growth detaches the old
// buffer, and a Rust compiler grows its heap.
let memory;
const u8 = () => new Uint8Array(memory.buffer);
const dv = () => new DataView(memory.buffer);
let outText = "";

// The WASI preview1 functions the module needs (the npm/wasm loader ships
// the same set); every other import is answered with ENOSYS.
const wasi = {
  args_sizes_get: (n, s) => { dv().setUint32(n, 0, true); dv().setUint32(s, 0, true); return SUCCESS; },
  args_get: () => SUCCESS,
  environ_sizes_get: (n, s) => { dv().setUint32(n, 0, true); dv().setUint32(s, 0, true); return SUCCESS; },
  environ_get: () => SUCCESS,
  clock_time_get: (_id, _precision, out) => {
    dv().setBigUint64(out, BigInt(Math.round(performance.now() * 1e6)), true);
    return SUCCESS;
  },
  clock_res_get: (_id, out) => { dv().setBigUint64(out, 1000n, true); return SUCCESS; },
  random_get: (buf, len) => {
    // The guest has no crypto.getRandomValues; Math.random entropy is
    // enough for what tsc draws it for.
    for (let i = 0; i < len; i++) u8()[buf + i] = (Math.random() * 256) | 0;
    return SUCCESS;
  },
  fd_write: (fd, iovs, iovsLen, nwritten) => {
    if (fd !== 1 && fd !== 2) return EBADF;
    let total = 0;
    for (let i = 0; i < iovsLen; i++) {
      const ptr = dv().getUint32(iovs + i * 8, true);
      const len = dv().getUint32(iovs + i * 8 + 4, true);
      if (fd === 1) outText += dec.decode(u8().subarray(ptr, ptr + len));
      total += len;
    }
    dv().setUint32(nwritten, total, true);
    return SUCCESS;
  },
  fd_fdstat_get: (fd, out) => {
    if (fd > 2) return EBADF;
    u8().fill(0, out, out + 24);
    return SUCCESS;
  },
  fd_prestat_get: () => EBADF,
  proc_exit: (code) => { throw new WasiExit(code); },
  sched_yield: () => SUCCESS,
};

const bytes = vm.fs.readBytes(WASM_PATH);
const mod = new WebAssembly.Module(bytes);
const imports = { wasi_snapshot_preview1: {}, ts_host: {} };
for (const { module, name } of WebAssembly.Module.imports(mod)) {
  imports[module][name] = imports[module][name] || (() => ENOSYS);
}
Object.assign(imports.wasi_snapshot_preview1, wasi);
let staged;
imports.ts_host.fs = (op, ptr, len) => {
  const result = fsCall(op, u8().subarray(ptr, ptr + len));
  if (result === undefined) return -1;
  staged = result;
  return staged.length;
};
imports.ts_host.fs_take = (ptr) => { u8().set(staged, ptr); staged = undefined; };

const inst = new WebAssembly.Instance(mod, imports);
memory = inst.exports.memory;

const FLAG_DIAGNOSTICS_JSON = 1;  // diagnostics come back as the reply
const request = enc.encode([PROJECT_DIR, String(FLAG_DIAGNOSTICS_JSON), "-p", "."].join("\0"));
u8().set(request, inst.exports.ts_input(request.length));
let status;
try {
  status = inst.exports.ts_run();
} catch (e) {
  if (e instanceof WasiExit) status = e.code; else throw e;
}

if (outText) console.log(outText.trimEnd());
const len = inst.exports.ts_output_len();
if (len) {
  const ptr = inst.exports.ts_output();
  const diags = JSON.parse(dec.decode(u8().subarray(ptr, ptr + len)));
  for (const d of diags) {
    const at = d.startPosition
      ? d.fileName + "(" + (d.startPosition.line + 1) + "," +
        (d.startPosition.character + 1) + ")"
      : d.fileName;
    console.log(at + ": error TS" + d.code + ": " + d.text);
  }
}
vm.exit(status);
