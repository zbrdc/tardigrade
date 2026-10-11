globalThis.queueMicrotask = (fn) => Promise.resolve().then(fn);
const __setTimeout = setTimeout, __setInterval = setInterval;
globalThis.setTimeout = (fn, delay, ...args) =>
  __setTimeout(args.length ? () => fn(...args) : fn, delay);
globalThis.setInterval = (fn, delay, ...args) =>
  __setInterval(args.length ? () => fn(...args) : fn, delay);
vm.snapshot = () => JSON.stringify(
  Object.fromEntries(vm.fs.list("/").map((p) => [p, vm.fs.read(p)])));
vm.restore = (s) => {
  const m = JSON.parse(s);
  for (const p in m) vm.fs.write(p, m[p]);
};
vm.run = (p) => (0, eval)(vm.fs.read(p));
