// Agent driver: executes the application under development and reports
// PASS/FAIL to the harness outside the VM.
try {
  vm.run("/app/app.js");
  if (typeof globalThis.sum !== "function") {
    throw new Error("app.js must define sum()");
  }
  const got = sum([1, 2, 3]);
  if (got !== 6) throw new Error("sum([1,2,3]) returned " + got + ", want 6");
  console.log("PASS");
  vm.exit(0);
} catch (e) {
  console.log("FAIL: " + e.message);
  vm.exit(1);
}
