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
// over the original constructor. The original stays reachable only from
// this closure: the rebuilt Date's [[Prototype]] is Function.prototype
// (chaining it to the original would hand the guest the host clock
// through Object.getPrototypeOf), its statics are own copies, and every
// intrinsic the override touches at call time is captured here too - so
// no date object, prototype, subclass chain or replaced Reflect yields a
// constructor bound to the host clock.
if (typeof globalThis.__virtualDateMs === "function") {
  const __vnow = globalThis.__virtualDateMs;
  delete globalThis.__virtualDateMs;
  const HostDate = Date;
  const __construct = Reflect.construct;
  const __apply = Reflect.apply;
  // Date() as a function ignores its arguments (ECMA-262 21.4.2.1) and
  // builds its string without consulting the (replaceable)
  // Date.prototype methods; the captured toString does the same.
  const __dateString = HostDate.prototype.toString;
  // A plain function, not a class, so Date() without new still works;
  // strict with a simple parameter list, so it carries no own
  // arguments/caller accessors the platform's constructor does not have.
  // Reflect.construct keeps new.target (subclasses, prototype changes)
  // honest, an explicit argument is passed through untouched, so
  // new Date(ms) (and every other form) stays exact, and the strict
  // arguments object - whose indices are own data properties - goes to
  // Reflect.construct as-is: copying through any array would run guest
  // index accessors on Array.prototype/Object.prototype instead.
  // The no-argument [__vnow()] is an array literal, whose elements are
  // own data properties from creation, so it cannot hit them either.
  const VDate = function Date() {
    "use strict";
    if (new.target === undefined) {
      return __apply(__dateString, new HostDate(__vnow()), []);
    }
    return __construct(HostDate, arguments.length ? arguments : [__vnow()], new.target);
  };
  VDate.prototype = HostDate.prototype;
  // now stays non-constructable like the platform's (an arrow), with
  // the platform's name.
  const nowFn = () => __vnow();
  Object.defineProperty(nowFn, "name", { value: "now", configurable: true });
  // Platform constructor shape: own length, name, prototype, now, parse
  // and UTC, all non-enumerable (Object.keys(Date) is empty), with the
  // prototype property non-writable like a class's.
  Object.defineProperty(VDate, "prototype", { writable: false });
  Object.defineProperty(VDate, "length", { value: 7, configurable: true });
  Object.defineProperty(VDate, "now", {
    value: nowFn,
    writable: true, configurable: true,
  });
  for (const staticName of ["parse", "UTC"]) {
    Object.defineProperty(
      VDate, staticName, Object.getOwnPropertyDescriptor(HostDate, staticName)
    );
  }
  HostDate.prototype.constructor = VDate;
  globalThis.Date = VDate;
}

// ICU data is not loaded in this V8 build. Several Intl/locale entry points
// abort or segfault the whole process instead of throwing (see
// docs/KNOWN_BUGS.md history), so replace every locale-sensitive surface with
// a catchable throw before any guest code can reach the native paths. The
// original Intl object is dropped from the global: no instance of a crashing
// formatter or segmenter can be created once this has run.
{
  const unavailable = function () {
    throw new Error("Intl is not available in this build (no ICU data)");
  };
  const stubCtor = function () {
    throw new Error("Intl is not available in this build (no ICU data)");
  };
  for (const proto of [Date.prototype, Number.prototype, String.prototype, BigInt.prototype, Array.prototype]) {
    for (const name of Object.getOwnPropertyNames(proto)) {
      if (name.startsWith("toLocale") || name === "localeCompare") {
        try { proto[name] = unavailable; } catch (_) { /* non-writable: leave it */ }
      }
    }
  }
  const intlStub = {};
  for (const name of [
    "Collator", "DateTimeFormat", "DisplayNames", "ListFormat", "Locale",
    "NumberFormat", "PluralRules", "RelativeTimeFormat", "Segmenter",
  ]) {
    intlStub[name] = stubCtor;
  }
  intlStub.getCanonicalLocales = unavailable;
  intlStub.supportedValuesOf = unavailable;
  globalThis.Intl = Object.freeze(intlStub);
}

// Counted bound for the memory V8 does not route through the host's
// ArrayBuffer allocator: WebAssembly.Memory pages, resizable ArrayBuffer
// growth headroom, and growable SharedArrayBuffer headroom. The host
// installs __memoryCapBytes (the effective --memory cap); capture and
// delete it, then wrap the constructors so reservations charge a shared
// ledger and refuse past the cap with a catchable RangeError. Charges
// are released by a FinalizationRegistry when an object is collected.
// Residual (docs/KNOWN_RISKS.md): a memory a WebAssembly module defines
// internally is created by the engine, not by these constructors, and
// growing it through the module's own exports cannot be intercepted
// here — the OS limit stays the backstop for that path.
if (typeof globalThis.__memoryCapBytes === "number") {
  const capBytes = globalThis.__memoryCapBytes;
  delete globalThis.__memoryCapBytes;
  const PAGE = 65536;
  const RErr = RangeError;
  const TErr = TypeError;
  let reserved = 0;
  const release = (n) => { reserved = Math.max(0, reserved - n); };
  const registry = typeof FinalizationRegistry === "function"
    ? new FinalizationRegistry(release) : null;
  const charge = (obj, n) => {
    if (n > capBytes - reserved) {
      throw new RErr("memory cap exceeded (WebAssembly/resizable memory is bounded by --memory)");
    }
    reserved += n;
    if (registry && obj) registry.register(obj, n);
  };

  // --- WebAssembly.Memory ---
  const OrigMemory = WebAssembly.Memory;
  const memState = new WeakMap(); // instance -> {chargedPages, currentPages}
  const WasmMemory = function Memory(descriptor) {
    if (!new.target) throw new TErr("WebAssembly.Memory must be constructed with new");
    const d = descriptor == null ? {} : Object(descriptor);
    const initial = Number(d.initial) || 0;
    const hasMax = d.maximum !== undefined;
    const maximum = hasMax ? Number(d.maximum) : initial;
    let mem;
    charge(null, maximum * PAGE);
    try {
      mem = Reflect.construct(OrigMemory, [descriptor], new.target);
    } catch (e) { release(maximum * PAGE); throw e; }
    if (registry) registry.register(mem, maximum * PAGE);
    memState.set(mem, { chargedPages: maximum, currentPages: initial });
    return mem;
  };
  WasmMemory.prototype = OrigMemory.prototype;
  try {
    Object.defineProperty(OrigMemory.prototype, "constructor", { value: WasmMemory, writable: true, configurable: true });
    const origGrow = OrigMemory.prototype.grow;
    OrigMemory.prototype.grow = function (delta) {
      const st = memState.get(this);
      const d = Number(delta) || 0;
      if (st && d > 0) {
        const target = st.currentPages + d;
        if (target > st.chargedPages) {
          charge(this, (target - st.chargedPages) * PAGE);
          st.chargedPages = target;
        }
        const r = origGrow.call(this, delta);
        st.currentPages = target;
        return r;
      }
      return origGrow.call(this, delta);
    };
  } catch (_) { /* prototype locked: construction cap still applies */ }
  WebAssembly.Memory = WasmMemory;

  // --- Resizable ArrayBuffer / growable SharedArrayBuffer ---
  const wrapResizable = (Orig, name) => {
    if (typeof Orig !== "function") return Orig;
    const chargedMax = new WeakMap(); // instance -> charged maxByteLength
    const Wrapped = function (length, options) {
      if (!new.target) throw new TErr(name + " must be constructed with new");
      const max = options != null && options.maxByteLength !== undefined
        ? Number(options.maxByteLength) : undefined;
      const initial = Number(length) || 0;
      let buf;
      if (max !== undefined && max > initial) charge(null, max - initial);
      try {
        buf = Reflect.construct(Orig, max !== undefined ? [length, options] : [length], new.target);
      } catch (e) {
        if (max !== undefined && max > initial) release(max - initial);
        throw e;
      }
      if (max !== undefined) {
        if (max > initial && registry) registry.register(buf, max - initial);
        chargedMax.set(buf, max);
      }
      return buf;
    };
    Wrapped.prototype = Orig.prototype;
    // Growing into already-charged headroom needs no new charge; the
    // prototype methods stay the originals, bounded by maxByteLength,
    // which was charged at construction.
    try {
      Object.defineProperty(Orig.prototype, "constructor", { value: Wrapped, writable: true, configurable: true });
    } catch (_) { /* construction cap still applies */ }
    for (const key of Object.getOwnPropertyNames(Orig)) {
      if (key !== "prototype" && key !== "length" && key !== "name") {
        try { Wrapped[key] = Orig[key]; } catch (_) { /* read-only static */ }
      }
    }
    return Wrapped;
  };
  globalThis.ArrayBuffer = wrapResizable(globalThis.ArrayBuffer, "ArrayBuffer");
  if (typeof globalThis.SharedArrayBuffer === "function") {
    globalThis.SharedArrayBuffer = wrapResizable(globalThis.SharedArrayBuffer, "SharedArrayBuffer");
  }
}
