# bridge-smoke

End-to-end vet for `tardigrade-bridge` (docs/BRIDGE_CONTRACT.md).

    TARDIGRADE_BIN=<release>/tardigrade TARDIGRADE_BRIDGE_BIN=<release>/tardigrade-bridge \
      python3 smoke.py 240

The argument is the soak duration in seconds; use hours (e.g. 7200) on a
quiet host for a conclusive memory-plateau check. Exits 0 only when every
check passes; SOAK lines sample bridge/worker RSS, worker count, zombies
and bridge fd counts every 30 s.
