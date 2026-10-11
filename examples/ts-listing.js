// Guest side of source extraction for examples/agent-loop-ts.sh: prints
// one line per /app TypeScript or config file,
//   <base64 of the path> <base64 of the content>
// Base64 has no spaces or newlines, so the host's `read -r p64 c64` can
// never mistake file content for framing - the marker-line scheme this
// replaces let a workspace file choose host paths by embedding markers.
// Extension filtering happens here so the loop's own /app/app.js bundle is
// never offered; the host validates paths again regardless.
const b64 = (s) => {
  let bin = "";
  for (const b of new TextEncoder().encode(s)) bin += String.fromCharCode(b);
  return btoa(bin);
};
for (const p of vm.fs.list("/app")) {
  if (/(\.d\.ts|\.tsx?|\.json)$/.test(p)) {
    console.log(b64(p) + " " + b64(vm.fs.read(p)));
  }
}
