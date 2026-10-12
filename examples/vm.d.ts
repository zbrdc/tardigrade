// Type definitions for the tardigrade guest API, compared with the runtime
// by tests/cli.rs (vm_dts_members_match_the_runtime): every vm.* and
// WebAssembly member named here exists in a live guest, and every runtime
// member is named.
//
// tardigrade guests are not DOM environments: the tsconfig should not pull in
// lib.dom (the tsconfig that examples/agent-loop-ts.sh generates sets
// "lib": ["esnext"]), or TextEncoder, WebAssembly and friends would be
// declared twice - under lib esnext they are otherwise absent. This file
// declares the whole WebAssembly namespace the engine exposes here;
// instantiateStreaming and compileStreaming stay undeclared because this
// runtime lacks them.

declare namespace vm {
  /** The V8 engine version string. */
  const version: string;

  /** The guest arguments after the entry path. */
  const args: string[];

  /** Stop the program with an exit code; the first exit wins. */
  function exit(code?: number): never;

  /** Load-and-eval a file from the RAM filesystem; the eval's result. */
  function run(path: string): unknown;

  /** The whole filesystem as snapshot JSON (the --dump/--preload format). */
  function snapshot(): string;

  /** Replace the filesystem from a snapshot JSON (version 2 or legacy). */
  function restore(json: string): void;

  namespace fs {
    /**
     * Write a file, creating missing parent directories; an existing file
     * is replaced wholesale.
     */
    function write(
      path: string,
      data: string | ArrayBuffer | SharedArrayBuffer | ArrayBufferView,
    ): void;

    /** Read as a string; bytes that are not valid UTF-8 become U+FFFD. */
    function read(path: string): string;

    /** Read as a freshly copied Uint8Array. */
    function readBytes(path: string): Uint8Array;

    /** Recursive listing of file paths under dir (default "/"), sorted. */
    function list(dir?: string): string[];

    /** Immediate child names (files and directories), sorted. */
    function readdir(path: string): string[];

    /** Create a directory; recursive also creates parents. */
    function mkdir(path: string, options?: { recursive?: boolean }): void;

    /** True when the path is a file or directory. */
    function exists(path: string): boolean;

    /**
     * Content bytes for a file, immediate child count for a directory.
     */
    function stat(path: string): { type: "file" | "dir"; size: number };

    /** POSIX-shaped move: a file replaces a file, a directory moves onto
     *  an empty directory. */
    function rename(from: string, to: string): void;

    /** Delete a file, an empty directory, or with recursive a subtree. */
    function remove(path: string, options?: { recursive?: boolean }): void;
  }

  namespace net {
    /** A fresh port number. */
    function port(): number;

    /** Receive messages on a port; payload is always a string. */
    function listen(
      port: number,
      handler: (payload: string, from: number) => void,
    ): void;

    /**
     * Queue a message to a port; throws when nothing listens there. Queued
     * bytes count against --memory: a send that would cross the cap throws
     * a catchable RangeError.
     */
    function send(fromPort: number, toPort: number, payload: string): void;

    /** Stop listening on a port. */
    function close(port: number): void;
  }

  namespace stdin {
    /** The next stdin chunk as a string; "" at EOF. */
    function read(): Promise<string>;
  }
}

declare const console: {
  log(...data: unknown[]): void;
  info(...data: unknown[]): void;
  warn(...data: unknown[]): void;
  error(...data: unknown[]): void;
};

declare function setTimeout<A extends unknown[]>(
  handler: (...args: A) => void,
  delay?: number,
  ...args: A
): number;
declare function setInterval<A extends unknown[]>(
  handler: (...args: A) => void,
  delay?: number,
  ...args: A
): number;
declare function clearTimeout(id?: number): void;
declare function clearInterval(id?: number): void;
declare function queueMicrotask(fn: () => void): void;

declare class TextEncoder {
  get encoding(): string;
  encode(input?: string): Uint8Array;
  encodeInto(
    source: string,
    destination: Uint8Array,
  ): { read: number; written: number };
}

declare class TextDecoder {
  constructor(
    label?: string,
    options?: { fatal?: boolean; ignoreBOM?: boolean },
  );
  get encoding(): string;
  get fatal(): boolean;
  get ignoreBOM(): boolean;
  decode(
    input?: ArrayBuffer | ArrayBufferView,
    options?: { stream?: boolean },
  ): string;
}

declare function atob(data: string): string;
declare function btoa(data: string): string;

/** No transfer list exists: a { transfer } option throws at runtime. */
declare function structuredClone<T>(value: T): T;

/** Monotonic milliseconds since the run started (virtual under
 *  --clock=virtual). */
declare const performance: { now(): number };

declare namespace WebAssembly {
  class Module {
    constructor(bytes: ArrayBuffer | ArrayBufferView);
    static exports(module: Module): { name: string; kind: string }[];
    static imports(module: Module): { module: string; name: string; kind: string }[];
  }
  class Instance {
    constructor(module: Module, imports?: Record<string, unknown>);
    readonly exports: any;
  }
  class Memory {
    constructor(descriptor: { initial: number; maximum?: number; shared?: boolean });
    readonly buffer: ArrayBuffer;
    grow(delta: number): number;
  }
  class Table {
    constructor(descriptor: {
      element: "anyfunc" | "externref";
      initial: number;
      maximum?: number;
    });
    readonly length: number;
    get(index: number): any;
    set(index: number, value: any): void;
    grow(delta: number, value?: any): number;
  }
  class Global {
    constructor(
      descriptor: {
        value: "i32" | "i64" | "f32" | "f64" | "v128" | "externref" | "anyfunc";
        mutable?: boolean;
      },
      value?: any,
    );
    value: any;
    valueOf(): any;
  }
  class Tag {
    constructor(descriptor: { types: string[] });
  }
  class Exception {
    constructor(tag: Tag, payload: unknown[]);
    getArg(tag: Tag, index: number): any;
    is(tag: Tag): boolean;
  }
  class CompileError extends Error {}
  class LinkError extends Error {}
  class RuntimeError extends Error {}
  /** The built-in tag for JavaScript values crossing an exception
   *  boundary. */
  const JSTag: Tag;
  function compile(bytes: ArrayBuffer | ArrayBufferView): Promise<Module>;
  function instantiate(
    bytes: ArrayBuffer | ArrayBufferView,
    imports?: Record<string, unknown>,
  ): Promise<{ module: Module; instance: Instance }>;
  function instantiate(
    module: Module,
    imports?: Record<string, unknown>,
  ): Promise<Instance>;
  function validate(bytes: ArrayBuffer | ArrayBufferView): boolean;
}
