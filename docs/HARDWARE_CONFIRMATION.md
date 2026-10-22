# How to hardware-confirm a path

A path starts as **dry run only**. It becomes **live** only after it has been
shown working on a real unit through TerraMine's own safety pipeline, and the
result has been recorded in the code. Mock tests alone never make a path live.

## Rules

- Use **one** unit you own and can reach physically, on a known firmware version.
- Never confirm a path against a miner you cannot recover, or that others depend on.
- Never run factory reset or password change for a confirmation on a production unit.
  Irreversible actions need a dedicated spare.
- Keep IPs, MACs, hostnames, wallet addresses and pool credentials out of commits,
  test output pasted into PRs, and issue text. Use environment variables locally.

## Steps

1. **Check the plan without sending anything.** Open **Capabilities…** for the
   miner. The cell should show *dry run only* with the expected path. Run the
   action with *Dry run* checked, then check **Audit log…** for the planned
   change and the paths tried.
2. **Write an ignored hardware test** under `src-tauri/tests/hw_*.rs`. Model it
   on `hw_axeos_settings.rs`, `hw_bitaxe_identify.rs` or
   `hw_bitaxe_pause_resume.rs`. The test must:
   - read the target from an env var (for example `TM_HW_BITAXE_IP`), never a
     literal;
   - go through the real `SafetyEngine` (limits, snapshot, write, read-back,
     health watch, rollback). For this test only, set
     `SafetyOptions { allow_unconfirmed_paths: true, .. }` (control jobs use
     `JobConfig.allow_unconfirmed`);
   - make the smallest safe change, then return to the exact baseline;
   - restore the baseline directly if the run ends neither verified nor rolled
     back.
3. **Run it by hand** against the one unit:
   `TM_HW_...=<ip> cargo test --test hw_<name> -- --ignored --nocapture --test-threads=1`
4. **Record the evidence** in the test's doc comment: model, firmware version,
   date, path used, values before and after, and health during the watch.
   Leave out addresses.
5. **Flip the gate**, as narrowly as the evidence allows:
   - **Control actions**: change `Capabilities::with_unconfirmed(action, route)`
     to `.with(...)` in the driver or override. Keep it version-gated, like
     `IDENTIFY_HW_CONFIRMED` in `overrides/bitaxe.rs`.
   - **Settings**: make `SettingsDriver::confirmed_for(change)` return true only
     for the confirmed firmware line and fields. For example, see
     `HW_CONFIRMED_LINES` and `confirmed_fields` in `safety/axeos.rs`.
   - **SSH paths** (PR #18): set the shell profile's `hw_confirmed` for that
     firmware.
6. **Update the tests.** The mock e2e tests assert the matrix exactly, so the
   changed cell must now read *live*. Add a unit test showing that neighbouring
   versions and fields stay dry run only.
7. **Update the docs.** Update the README action table and
   [FIRMWARE.md](FIRMWARE.md).

## Never

- Do not set `allow_unconfirmed_paths` / `allow_unconfirmed` in app code. They
  are for tests only.
- Do not widen a confirmation beyond what was observed. One firmware line does
  not confirm another, and frequency does not confirm voltage.
