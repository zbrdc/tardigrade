globalThis.queueMicrotask = (fn) => Promise.resolve().then(fn);
const __setTimeout = setTimeout, __setInterval = setInterval;
globalThis.setTimeout = (fn, delay, ...args) =>
  __setTimeout(args.length ? () => fn(...args) : fn, delay);
globalThis.setInterval = (fn, delay, ...args) =>
  __setInterval(args.length ? () => fn(...args) : fn, delay);
vm.run = (p) => (0, eval)(vm.fs.read(p));

// TextEncoder / TextDecoder (UTF-8 only). All byte-level encoding lives in
// the host callbacks installed by src/webapi; these classes exist only to
// hold per-instance state (the streaming carry and the undecided BOM) in
// #private fields. The helper functions are bound here and removed from the
// global object.
{
  const __encode = globalThis.__encodeUtf8;
  const __encodeInto = globalThis.__encodeIntoUtf8;
  const __decode = globalThis.__decodeUtf8;
  delete globalThis.__encodeUtf8;
  delete globalThis.__encodeIntoUtf8;
  delete globalThis.__decodeUtf8;

  class TextEncoder {
    get encoding() { return "utf-8"; }
    encode(s = "") {
      if (!(this instanceof TextEncoder)) throw new TypeError("Illegal invocation");
      return __encode(s);
    }
    encodeInto(source, destination) {
      if (!(this instanceof TextEncoder)) throw new TypeError("Illegal invocation");
      return __encodeInto(source, destination);
    }
  }
  Object.defineProperty(TextEncoder.prototype, Symbol.toStringTag, { value: "TextEncoder" });

  const UTF8_LABELS = new Set([
    "unicode-1-1-utf-8", "unicode11utf8", "unicode20utf8",
    "utf-8", "utf8", "x-unicode20utf8",
  ]);
  // WebIDL-style option handling: null/undefined is the empty dictionary,
  // anything non-object is a TypeError.
  const asOptions = (options) => {
    if (options == null) return {};
    if (typeof options !== "object") throw new TypeError("The provided value is not of type 'object'");
    return options;
  };

  class TextDecoder {
    #fatal;
    #ignoreBOM;
    #carry;
    #bom;
    constructor(label = "utf-8", options = null) {
      const text = String(label).replace(/^[\t\n\f\r ]+|[\t\n\f\r ]+$/g, "").toLowerCase();
      if (!UTF8_LABELS.has(text))
        throw new RangeError(`The "${String(label)}" encoding is not supported`);
      const o = asOptions(options);
      this.#fatal = !!o.fatal;
      this.#ignoreBOM = !!o.ignoreBOM;
      this.#carry = new Uint8Array(0);
      // No leading code point has been seen yet, so a BOM may still come.
      this.#bom = true;
    }
    get encoding() { return "utf-8"; }
    get fatal() { return this.#fatal; }
    get ignoreBOM() { return this.#ignoreBOM; }
    #reset() { this.#carry = new Uint8Array(0); this.#bom = true; }
    decode(input = new Uint8Array(0), options = null) {
      if (!(this instanceof TextDecoder)) throw new TypeError("Illegal invocation");
      const o = asOptions(options);
      const stream = o.stream === true;
      // A non-stream decode ends the stream: the next call starts fresh
      // (new BOM decision, no carry), also after a throwing decode.
      let r;
      try {
        r = __decode(this.#carry, input, {
          fatal: this.#fatal,
          ignoreBOM: this.#ignoreBOM,
          bomPending: this.#bom,
          flush: !stream,
        });
        this.#carry = r.carry;
        this.#bom = r.bomPending;
      } catch (e) {
        if (!stream) this.#reset();
        throw e;
      }
      if (!stream) this.#reset();
      return r.text;
    }
  }
  Object.defineProperty(TextDecoder.prototype, Symbol.toStringTag, { value: "TextDecoder" });
  globalThis.TextEncoder = TextEncoder;
  globalThis.TextDecoder = TextDecoder;
}
