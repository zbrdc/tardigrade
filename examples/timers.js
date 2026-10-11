// The event loop keeps the process alive after the script body ends.
let n = 0;
const iv = setInterval(() => {
  n += 1;
  console.log("tick " + n);
  if (n === 3) {
    clearInterval(iv);
    console.log("done after 3 ticks");
    vm.exit(0);
  }
}, 10);
