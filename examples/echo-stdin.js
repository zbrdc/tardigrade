// Streaming stdin: read chunks until EOF.
(async () => {
  let total = "";
  for (;;) {
    const chunk = await vm.stdin.read();
    if (chunk === "") break;
    total += chunk;
  }
  console.log("stdin bytes: " + total.length);
  vm.exit(0);
})();
