const state = { hits: 0, boot: Date.now() };
const wasmBytes = new Uint8Array([0,97,115,109,1,0,0,0,1,7,1,96,2,127,127,1,127,3,2,1,0,7,7,1,3,97,100,100,0,0,10,9,1,7,0,32,0,32,1,106,11]);
const add = new WebAssembly.Instance(new WebAssembly.Module(wasmBytes)).exports.add;
if (!vm.fs.exists("/var")) vm.fs.mkdir("/var");
globalThis.handler = async (req) => {
  const path = req.path, body = req.body;
  state.hits += 1;
  if (path === "/state") return { status: 200, type: "application/json", body: JSON.stringify({ hits: state.hits, bootAgoMs: Date.now() - state.boot }) };
  if (path === "/fs/write") { vm.fs.write("/var/last.txt", body === "" ? "empty" : body); return { status: 200, body: "ok" }; }
  if (path === "/fs/read") return { status: 200, body: vm.fs.read("/var/last.txt") };
  if (path === "/fs/list") return { status: 200, type: "application/json", body: JSON.stringify(vm.fs.list("/")) };
  if (path === "/wasm") { const [a, b] = (body || "19,23").split(",").map(Number); return { status: 200, body: String(add(a, b)) }; }
  if (path === "/net/echo") { const r = await vm.net.request("backend", body || "ping"); return { status: 200, type: "application/json", body: JSON.stringify({ up: r.status, echoed: r.text }) }; }
  if (path === "/encode") { const bytes = new TextEncoder().encode(body); const b64 = btoa(String.fromCharCode.apply(null, bytes)); const back = new TextDecoder().decode(Uint8Array.from(atob(b64), (c) => c.charCodeAt(0))); return { status: 200, type: "application/json", body: JSON.stringify({ roundtrip: back }) }; }
  if (path === "/clone") { const orig = { list: [1, 2, 3], nested: { ok: true } }; const copy = structuredClone(orig); copy.nested.ok = false; return { status: 200, type: "application/json", body: JSON.stringify({ orig, copy }) }; }
  if (path === "/cpu") { let s = 0; for (let i = 0; i < 2000000; i++) s = (s + i) | 0; return { status: 200, body: "cpu " + s }; }
  if (path === "/sleep") { const ms = Math.min(Number(body || "100"), 2000); await new Promise((r) => setTimeout(r, ms)); return { status: 200, body: "slept " + ms }; }
  if (path === "/boom") { throw new Error("intentional"); }
  if (path === "/die") { vm.exit(3); }
  return { status: 200, body: "hello from RAM" };
};
