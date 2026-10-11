globalThis.queueMicrotask = (fn) => Promise.resolve().then(fn);
const __setTimeout = setTimeout, __setInterval = setInterval;
globalThis.setTimeout = (fn, delay, ...args) =>
  __setTimeout(args.length ? () => fn(...args) : fn, delay);
globalThis.setInterval = (fn, delay, ...args) =>
  __setInterval(args.length ? () => fn(...args) : fn, delay);
vm.run = (p) => (0, eval)(vm.fs.read(p));

// TextEncoder / TextDecoder (UTF-8 only). All byte-level encoding lives in
// the host callbacks installed by src/webapi.rs; these classes exist only
// to hold the per-instance state (the streaming carry and the undecided
// BOM) that has no home on the Rust side without per-object storage. The
// helper functions are bound here and removed from the global object.
{
  const __encode = globalThis.__encodeUtf8;
  const __encodeInto = globalThis.__encodeIntoUtf8;
  const __decode = globalThis.__decodeUtf8;
  delete globalThis.__encodeUtf8;
  delete globalThis.__encodeIntoUtf8;
  delete globalThis.__decodeUtf8;

  class TextEncoder {
    encode(s = "") {
      if (!(this instanceof TextEncoder)) throw new TypeError("Illegal invocation");
      return __encode(s);
    }
    encodeInto(source, destination) {
      if (!(this instanceof TextEncoder)) throw new TypeError("Illegal invocation");
      return __encodeInto(source, destination);
    }
  }

  const UTF8_LABELS = new Set([
    "unicode-1-1-utf-8", "unicode11utf8", "unicode20utf8",
    "utf-8", "utf8", "x-unicode20utf8",
  ]);

  class TextDecoder {
    constructor(label = "utf-8", options = {}) {
      if (!UTF8_LABELS.has(String(label).toLowerCase()))
        throw new RangeError("unsupported encoding label (only UTF-8 is available)");
      this.__fatal = !!options.fatal;
      this.__ignoreBOM = !!options.ignoreBOM;
      this.__carry = new Uint8Array(0);
      // No leading code point has been seen yet, so a BOM may still come.
      this.__bom = true;
    }
    get encoding() { return "utf-8"; }
    get fatal() { return this.__fatal; }
    get ignoreBOM() { return this.__ignoreBOM; }
    decode(input = new Uint8Array(0), options = {}) {
      if (!(this instanceof TextDecoder)) throw new TypeError("Illegal invocation");
      const r = __decode(this.__carry, input, {
        fatal: this.__fatal,
        ignoreBOM: this.__ignoreBOM,
        bomPending: this.__bom,
        flush: !options.stream,
      });
      // A throwing decode (bad input or fatal data) leaves the state as it
      // was before the call; the next call starts from there.
      this.__carry = r.carry;
      this.__bom = r.bomPending;
      return r.text;
    }
  }
  globalThis.TextEncoder = TextEncoder;
  globalThis.TextDecoder = TextDecoder;
}
