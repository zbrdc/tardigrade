# The pinned tsc-rs npm package every TypeScript probe and run uses.
# Override with TARDIGRADE_TSC_RS_PKG; the cli tests pass their own pin
# through that variable, so the tests' probes and the loop cannot disagree
# about which checker runs. Sourced by agent-loop-ts.sh and ts-smoke.sh.
TSC_RS_PKG=${TARDIGRADE_TSC_RS_PKG:-tsc-rs@0.3.0}
