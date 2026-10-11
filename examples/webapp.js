// A "web application" living entirely in RAM: no sockets, no disk. The host
// bridge relays HTTP requests to handler() and ships responses back.
const wasmBytes = new Uint8Array([
  0, 97, 115, 109, 1, 0, 0, 0, 1, 7, 1, 96, 2, 127, 127, 1, 127, 3, 2, 1, 0,
  7, 7, 1, 3, 97, 100, 100, 0, 0, 10, 9, 1, 7, 0, 32, 0, 32, 1, 106, 11,
]);
const add = new WebAssembly.Instance(new WebAssembly.Module(wasmBytes)).exports.add;

globalThis.handler = (req) => {
  if (req.path === "/") {
    return {
      status: 200,
      body: "<h1>hello from RAM</h1><p>19+23=" + add(19, 23) + "</p>",
    };
  }
  if (req.path === "/api/sum" && req.method === "POST") {
    const xs = JSON.parse(req.body).xs;
    return { status: 200, type: "application/json", body: String(xs.reduce((a, b) => add(a, b), 0)) };
  }
  return { status: 404, body: "not found: " + req.path };
};
