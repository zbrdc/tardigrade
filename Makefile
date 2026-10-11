CARGO ?= cargo
# Override CARGO_TARGET_DIR (env or make var) to relocate build output.
CARGO_TARGET_DIR ?= target
export CARGO_TARGET_DIR
# Extra flags for build, test and clippy, e.g. CARGOFLAGS=--locked in CI.
CARGOFLAGS ?=
OUT := $(CARGO_TARGET_DIR)/release/v8vm
BRIDGE := $(CARGO_TARGET_DIR)/release/serve_bridge

build:
	$(CARGO) build --release $(CARGOFLAGS)

test:
	$(CARGO) test --release $(CARGOFLAGS)

clippy:
	$(CARGO) clippy --release --all-targets $(CARGOFLAGS) -- -D warnings

check: build
	@$(OUT) examples/hello.js | grep -qx 'hello from ram'
	@OUT=$(OUT) bash examples/sandbox-smoke.sh
	@$(OUT) examples/ramfs-demo.js | grep -qx 'files: /greeting.txt,/note.txt'
	@$(OUT) --timeout=100 examples/spin.js >/dev/null 2>&1; test $$? -eq 124
	@$(OUT) examples/timers.js | grep -qx 'done after 3 ticks'
	@$(OUT) examples/net-demo.js | grep -qx 'client got: echo:ping'
	@printf 'hello stdin' | $(OUT) examples/echo-stdin.js | grep -qx 'stdin bytes: 11'
	@P=$(dir $(OUT))fs.json; $(OUT) --dump=$$P examples/ramfs-demo.js >/dev/null; printf 'console.log(vm.fs.list("/").join(","))' | $(OUT) --preload=$$P - | grep -qx '/greeting.txt'
	@W=$(dir $(OUT))ws.json; printf 'vm.fs.write("/app/app.js", "globalThis.sum = (xs) => xs.reduce((a, b) => a - b, 0);");' | $(OUT) - --dump=$$W >/dev/null; $(OUT) --preload=$$W examples/agent-driver.js >/dev/null 2>&1; test $$? -eq 1
	@W=$(dir $(OUT))ws.json; printf 'vm.fs.write("/app/app.js", "globalThis.sum = (xs) => xs.reduce((a, b) => a + b, 0);");' | $(OUT) - --preload=$$W --dump=$$W >/dev/null; R=$$($(OUT) --preload=$$W --dump=$$W examples/agent-driver.js) || exit 1; printf '%s\n' "$$R" | grep -qx PASS
	@OUT=$(OUT) BRIDGE=$(BRIDGE) bash examples/serve-demo.sh
	@echo OK

# The TypeScript agent loop (examples/agent-loop-ts.sh): bun bundles the
# app, tsc-rs (or tsc) type-checks it. Both are host-side dev tools; when
# they are missing this skips loudly (V8VM_REQUIRE_TS=1 fails instead).
check-ts:
	@OUT=$(OUT) bash examples/ts-smoke.sh
	@echo TS-OK

# The CI gate: unit/integration tests, clippy, then the scripted smoke checks.
# Sub-makes run one after another, so a parallel make (-j) cannot interleave them.
ci:
	$(MAKE) test
	$(MAKE) clippy
	$(MAKE) check

# Memory-heavy tests (multi-hundred-megabyte guests); not part of ci.
test-heavy:
	$(CARGO) test --release $(CARGOFLAGS) -- --ignored

clean:
	$(CARGO) clean

.PHONY: check clean build test clippy ci test-heavy check-ts
