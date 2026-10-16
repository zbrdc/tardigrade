//! Focused structured-clone conformance checks for tardigrade's documented API.

const OVERLAPPING_VIEWS: &str = r#"
  function check(condition, message) {
    if (!condition) throw new Error("clone assertion failed: " + message);
  }

  const backing = new ArrayBuffer(12);
  const bytes = new Uint8Array(backing);
  bytes.set([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
  const words = new Uint16Array(backing, 2, 4);
  const tail = new Uint8Array(backing, 5, 5);
  const window = new DataView(backing, 3, 7);
  const original = { backing, words, tail, window };

  const copy = structuredClone(original);
  check(copy.backing !== backing, "backing store is copied");
  check(copy.words !== words && copy.tail !== tail && copy.window !== window,
        "each view is copied");
  check(copy.words.buffer === copy.backing &&
        copy.tail.buffer === copy.backing &&
        copy.window.buffer === copy.backing,
        "all overlapping views alias the same cloned buffer");
  check(copy.words.byteOffset === 2 && copy.words.byteLength === 8,
        "typed-array offset and byte length survive");
  check(copy.tail.byteOffset === 5 && copy.tail.byteLength === 5,
        "overlapping byte view offset and length survive");
  check(copy.window.byteOffset === 3 && copy.window.byteLength === 7,
        "DataView offset and length survive");
  check(copy.tail[0] === 6 && copy.window.getUint8(3) === 7,
        "cloned views see the original bytes");

  // Write through one view and observe the exact same byte through both
  // overlapping views; this asserts alias identity rather than a count.
  copy.window.setUint8(3, 0xA5);
  check(copy.tail[1] === 0xA5 && copy.words.buffer === copy.tail.buffer,
        "DataView writes are visible through the overlapping typed array");
  copy.tail[2] = 0xB6;
  check(copy.window.getUint8(4) === 0xB6 &&
        new Uint8Array(copy.words.buffer, copy.words.byteOffset + 4, 2)[1] === 0xB6,
        "typed-array writes are visible through the overlapping views");
  check(bytes[6] === 7 && bytes[7] === 8,
        "mutating the clone does not mutate the original backing store");
"#;

const MAP_SET_CYCLES: &str = r#"
  function check(condition, message) {
    if (!condition) throw new Error("clone assertion failed: " + message);
  }

  const map = new Map();
  const set = new Set();
  const node = { map, set };
  node.self = node;
  map.set(map, set);
  map.set(node, node);
  set.add(set);
  set.add(map);
  set.add(node);

  const copy = structuredClone({ map, set, node });
  check(copy.map !== map && copy.set !== set && copy.node !== node,
        "cyclic containers and entry object are cloned");
  check(copy.node.self === copy.node, "object cycle points to cloned object");
  check(copy.node.map === copy.map && copy.node.set === copy.set,
        "object references point to cloned containers");
  check(copy.map.get(copy.map) === copy.set,
        "Map key and value preserve cyclic Map/Set identity");
  check(copy.map.get(copy.node) === copy.node,
        "Map entry key and value preserve cloned object identity");
  check(copy.set.has(copy.set) && copy.set.has(copy.map) &&
        copy.set.has(copy.node),
        "Set entries preserve self, Map, and object identities");
"#;

const DETACHED_BUFFER_REFUSALS: &str = r#"
  function check(condition, message) {
    if (!condition) throw new Error("clone assertion failed: " + message);
  }
  function expectErrorName(name, callback) {
    let caught;
    try {
      callback();
    } catch (error) {
      caught = error;
    }
    check(caught instanceof Error && caught.name === name,
          "expected " + name + ", got " + (caught && caught.name));
  }

  // Detached buffers/views are also refused rather than silently copied.
  const detached = new ArrayBuffer(4);
  const detachedView = new Uint8Array(detached);
  detached.transfer();
  check(detached.byteLength === 0, "test buffer is detached");
  expectErrorName("DataCloneError", () => structuredClone(detached));
  expectErrorName("DataCloneError", () => structuredClone(detachedView));
"#;

fn run_assertion_guest(source: &str, name: &str) {
    let config = tardigrade::Config {
        timeout_ms: 5_000,
        heap_mb: 32,
        stdin_consumed: true,
        ..tardigrade::Config::new()
    };
    let exit = tardigrade::run(source, name, &config);
    assert_eq!(exit, 0, "guest assertion failed in {name}");
}

#[test]
fn clone_preserves_aliases_across_overlapping_views() {
    run_assertion_guest(OVERLAPPING_VIEWS, "clone-overlapping-views.js");
}

#[test]
fn clone_preserves_cycles_through_map_and_set_entries() {
    run_assertion_guest(MAP_SET_CYCLES, "clone-map-set-cycles.js");
}

#[test]
fn clone_refuses_detached_buffers_and_views() {
    run_assertion_guest(DETACHED_BUFFER_REFUSALS, "clone-detached-buffers.js");
}
