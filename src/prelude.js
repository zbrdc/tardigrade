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
// #private fields. The intrinsics the classes use at call time are captured
// here, so a guest replacing globals between calls cannot change what gets
// allocated or how errors are built.
{
  const __encode = globalThis.__encodeUtf8;
  const __encodeInto = globalThis.__encodeIntoUtf8;
  const __decode = globalThis.__decodeUtf8;
  delete globalThis.__encodeUtf8;
  delete globalThis.__encodeIntoUtf8;
  delete globalThis.__decodeUtf8;

  const U8 = Uint8Array;
  const AB = ArrayBuffer;
  const SAB = SharedArrayBuffer;
  const isView = ArrayBuffer.isView;
  const Str = String;
  const TErr = TypeError;
  const RErr = RangeError;
  const UTF8_LABELS = new Set([
    "unicode-1-1-utf-8", "unicode11utf8", "unicode20utf8",
    "utf-8", "utf8", "x-unicode20utf8",
  ]);
  // WebIDL-style option handling: null/undefined is the empty dictionary,
  // functions are callable objects and count, anything else is a TypeError.
  const asOptions = (options) => {
    if (options == null) return {};
    const t = typeof options;
    if (t !== "object" && t !== "function") throw new TErr("The provided value is not of type 'object'");
    return options;
  };

  class TextEncoder {
    get encoding() { return "utf-8"; }
    encode(s = "") {
      if (!(this instanceof TextEncoder)) throw new TErr("Illegal invocation");
      return __encode(s);
    }
    encodeInto(source, destination) {
      if (!(this instanceof TextEncoder)) throw new TErr("Illegal invocation");
      return __encodeInto(source, destination);
    }
  }

  class TextDecoder {
    #fatal;
    #ignoreBOM;
    #carry;
    #bom;
    constructor(label = "utf-8", options = null) {
      // label + "" converts like Str() and throws on a symbol, as the
      // platform's string conversion does.
      const text = (label + "").replace(/^[\t\n\f\r ]+|[\t\n\f\r ]+$/g, "").toLowerCase();
      if (!UTF8_LABELS.has(text))
        throw new RErr(`The "${Str(label)}" encoding is not supported`);
      const o = asOptions(options);
      this.#fatal = !!o.fatal;
      this.#ignoreBOM = !!o.ignoreBOM;
      this.#carry = new U8(0);
      // No leading code point has been seen yet, so a BOM may still come.
      this.#bom = true;
    }
    decode(input = new U8(0), options = null) {
      if (!(this instanceof TextDecoder)) throw new TErr("Illegal invocation");
      // Options convert before the input's type is checked (Node runs a
      // stream getter and still throws the input TypeError), and a bad
      // BufferSource throws before any state change: the stream survives.
      const o = asOptions(options);
      // The stream getter runs before the input's type is checked (Node
      // behaves the same), and neither changes any state.
      const stream = !!o.stream;
      if (!(input instanceof AB || input instanceof SAB || isView(input)))
        throw new TErr("decode input must be an ArrayBuffer or a typed array");
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
    get encoding() { return "utf-8"; }
    get fatal() { return this.#fatal; }
    get ignoreBOM() { return this.#ignoreBOM; }
    #reset() { this.#carry = new U8(0); this.#bom = true; }
  }

  // WebIDL operations and attribute getters are enumerable on the
  // prototype, and the brand tag is configurable, like the platform's.
  const webidlStyle = (proto, tag) => {
    for (const name of Object.getOwnPropertyNames(proto)) {
      if (name !== "constructor") {
        const d = Object.getOwnPropertyDescriptor(proto, name);
        Object.defineProperty(proto, name, { ...d, enumerable: true });
      }
    }
    Object.defineProperty(proto, Symbol.toStringTag, { value: tag, configurable: true });
  };
  webidlStyle(TextEncoder.prototype, "TextEncoder");
  webidlStyle(TextDecoder.prototype, "TextDecoder");

  globalThis.TextEncoder = TextEncoder;
  globalThis.TextDecoder = TextDecoder;
}
