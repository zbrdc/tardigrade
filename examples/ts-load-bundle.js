// Loads a bundled app into the RAM workspace for examples/agent-loop-ts.sh:
// reads a base64-encoded bundle from stdin and stores it as /app/app.js.
// stdin (not an argv entry) carries it because a single argument is capped
// at ~128 KiB by the kernel - bundles are routinely larger; base64 keeps
// the text safe across chunk boundaries (plain UTF-8 could split a code
// point between two vm.stdin chunks).
const collect = (acc) =>
  vm.stdin.read().then((chunk) => (chunk === "" ? acc : collect(acc + chunk)));
collect("").then((b64) => {
  const bin = atob(b64.trim());
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  vm.fs.write("/app/app.js", new TextDecoder().decode(bytes));
});
