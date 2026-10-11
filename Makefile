CARGO ?= cargo
# Override CARGO_TARGET_DIR (env or make var) to relocate build output.
CARGO_TARGET_DIR ?= target
export CARGO_TARGET_DIR
OUT := $(CARGO_TARGET_DIR)/release/v8vm
BRIDGE := $(CARGO_TARGET_DIR)/release/serve_bridge

build:
	$(CARGO) build --release

test:
	$(CARGO) test --release

check: build
	@$(OUT) examples/hello.js | grep -qx 'hello from ram'
	@$(OUT) --sandbox examples/hello.js | grep -qx 'hello from ram'
	@$(OUT) examples/ramfs-demo.js | grep -qx 'files: /greeting.txt,/note.txt'
	@$(OUT) --timeout=100 examples/spin.js >/dev/null 2>&1; test $$? -eq 124
	@$(OUT) examples/timers.js | grep -qx 'done after 3 ticks'
	@$(OUT) examples/net-demo.js | grep -qx 'client got: echo:ping'
	@printf 'hello stdin' | $(OUT) examples/echo-stdin.js | grep -qx 'stdin bytes: 11'
	@P=$(dir $(OUT))fs.json; $(OUT) --dump=$$P examples/ramfs-demo.js >/dev/null; printf 'console.log(vm.fs.list("/").join(","))' | $(OUT) --preload=$$P - | grep -qx '/greeting.txt'
	@W=$(dir $(OUT))ws.json; printf 'vm.fs.write("/app/app.js", "globalThis.sum = (xs) => xs.reduce((a, b) => a - b, 0);");' | $(OUT) - --dump=$$W >/dev/null; $(OUT) --preload=$$W examples/agent-driver.js >/dev/null 2>&1; test $$? -eq 1
	@W=$(dir $(OUT))ws.json; printf 'vm.fs.write("/app/app.js", "globalThis.sum = (xs) => xs.reduce((a, b) => a + b, 0);");' | $(OUT) - --preload=$$W --dump=$$W >/dev/null; $(OUT) --preload=$$W examples/agent-driver.js | grep -qx PASS
	@OUT=$(OUT) BRIDGE=$(BRIDGE) bash examples/serve-demo.sh
	@echo OK

clean:
	$(CARGO) clean

.PHONY: check clean build test
