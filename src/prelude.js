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
  // Label handling must not depend on mutable builtins at call time
  // (String.prototype methods, Set, RegExp are all replaceable), so the
  // trim and ASCII-lowercase are hand-rolled over captured code-unit
  // readers, and the label table is a plain indexed array.
  const cca = String.prototype.charCodeAt;
  const fromC = String.fromCharCode;
  const UTF8_LABELS = [
    "unicode-1-1-utf-8", "unicode11utf8", "unicode20utf8",
    "utf-8", "utf8", "x-unicode20utf8",
  ];
  const isWs = (c) => c === 32 || c === 9 || c === 10 || c === 12 || c === 13;
  const normLabel = (s) => {
    let a = 0, b = s.length;
    while (a < b && isWs(cca.call(s, a))) a++;
    while (b > a && isWs(cca.call(s, b - 1))) b--;
    let out = "";
    for (let i = a; i < b; i++) {
      const c = cca.call(s, i);
      out += fromC(c >= 65 && c <= 90 ? c + 32 : c);
    }
    return out;
  };
  // WebIDL-style option handling: null/undefined is the empty dictionary,
  // functions are callable objects and count, anything else is a TypeError.
  const asOptions = (options) => {
    if (options == null) return {};
    const t = typeof options;
    if (t !== "object" && t !== "function") throw new TErr("The provided value is not of type 'object'");
    return options;
  };

  class TextEncoder {
    get encoding() {
      if (!(this instanceof TextEncoder)) throw new TErr("Illegal invocation");
      return "utf-8";
    }
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
      // A template literal converts with the ToString hint (toString
      // over valueOf) and throws on a symbol, like the platform.
      const text = normLabel(`${label}`);
      let known = false;
      for (let i = 0; i < UTF8_LABELS.length; i++) known = known || UTF8_LABELS[i] === text;
      if (!known)
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
    get encoding() {
      if (!(this instanceof TextDecoder)) throw new TErr("Illegal invocation");
      return "utf-8";
    }
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

// --clock=virtual: Date must read the run's virtual clock, never the
// host clock. The host installs __virtualDateMs only for virtual runs;
// capture it like the encoder hooks above, delete it, and rebuild Date
// over the original constructor, which stays reachable only from this
// closure - no date object, prototype or subclass can hand the guest a
// constructor bound to the host clock.
if (typeof globalThis.__virtualDateMs === "function") {
  const __vnow = globalThis.__virtualDateMs;
  delete globalThis.__virtualDateMs;
  const HostDate = Date;
  // A plain function, not a class, so Date() without new still returns
  // the time string; Reflect.construct keeps new.target (subclasses,
  // prototype changes) honest. An explicit argument is passed through
  // untouched, so new Date(ms) (and every other form) stays exact.
  const VDate = function Date(...args) {
    if (new.target === undefined) {
      // Date() as a function ignores its arguments (ECMA-262 21.4.2.1)
      // and returns the current time as a string - here the virtual one.
      return `${new HostDate(__vnow())}`;
    }
    return Reflect.construct(HostDate, args.length ? args : [__vnow()], new.target);
  };
  VDate.prototype = HostDate.prototype;
  // The statics (UTC, parse) are inherited from the original; now is
  // the virtual clock, and Date.prototype.constructor names the visible
  // constructor instead of the captured original.
  Object.setPrototypeOf(VDate, HostDate);
  HostDate.prototype.constructor = VDate;
  VDate.now = () => __vnow();
  Object.defineProperty(VDate, "length", { value: 7 });
  globalThis.Date = VDate;
}
