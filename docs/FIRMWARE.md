# Per-firmware notes

These notes explain *why* the capability matrix looks the way it does for each
family. The in-app **Capabilities…** matrix is always the source of truth. These
notes may lag behind the code.

## AxeOS (Bitaxe, NerdAxe, other ESP-Miner forks)

- Paths: AxeOS HTTP (`/api/system/...`) only. asic-rs has no fan config or
  frequency target for these devices.
- **Start / stop**: `POST /api/system/pause` and `/resume`. Used on AxeOS ≥ 2.12,
  or on older versions when a probe shows the endpoint exists.
- **Reboot**: `POST /api/system/restart`. This works on 2.10 as well.
- **Fault light**: AxeOS has an *identify* endpoint from 2.12, and it is
  hardware-confirmed on the 2.15 line. Identify expires after about 30 s, and a
  second call toggles it. There is no "off" command, so *Fault light off* is not
  supported.
- **Frequency and fan (manual/auto)**: hardware-confirmed on AxeOS 2.15.x
  (`HW_CONFIRMED_LINES` in `safety/axeos.rs`). A Bitaxe Gamma on 2.15.3 showed:
  - frequency applies live, with no restart (`actualFrequency` confirms it);
  - manual and auto fan read back correctly.
  Other AxeOS lines are dry run only.
- **Core voltage, fan target temperature**: implemented, but dry run only.

## Antminer S9 (stock Bitmain firmware)

- Paths: the stock web CGI, and cgminer RPC on port 4028.
- **Fault light**: CGI blink. Dry run only.
- **Reboot via cgminer RPC `restart`**: dry run only. The default API allow-list
  usually blocks it. See the findings below.
- **Reboot via CGI** (`reboot.cgi`, digest auth): live.
- **Start / stop**: only on firmware whose stock config has a miner mode
  (sleep/normal). Without it, the matrix shows them as not supported.

### S9 API allow-list findings

cgminer/bmminer's RPC is guarded by `api-allow` in `/config/bmminer.conf`:

- A rule with the `W:` prefix grants privileged (write) commands. Rules without
  the prefix grant read-only commands.
- **cgminer applies the first rule that matches the client.** A broader read-only
  rule that comes earlier shadows a later `W:` rule. So any `W:` entry for a
  management host must come **before** the read-only rules.
- Stock read commands include `summary`, `stats`, `pools`, `devs`, `version` and
  `privileged`. `privileged` tells a client whether it has write access without
  changing anything. TerraMine's only S9 RPC write is `restart`.
- The config file can be edited over the stock Dropbear SSH. bmminer re-reads it
  on `/etc/init.d/bmminer.sh restart`.
- **Test, done once on one S9:**
  1. Back up the config.
  2. Add a `W:` rule for the management host as the first entry.
  3. Restart bmminer and confirm `privileged` reports write access.
  4. Prove the RPC `restart`.
  5. Restore the original config and verify it is restored.

  This is not yet a hardware confirmation of TerraMine's own cgminer-rpc path, so
  that path stays dry run only. TerraMine never edits the allow-list itself.
- Upstream: read support for the allow-list (reporting whether the miner allows
  writes) was proposed to asic-rs as an issue and a PR.

## asic-rs native families (Antminer on Braiins/LuxOS/VNish, Whatsminer, Avalon, ePIC, …)

- Every capability comes from the miner object's `supports_*()` flags (22 in
  asic-rs 0.8.6). The matrix shows them raw under *asic-rs flags*.
- Start / stop / reboot / fault light use the native calls and run live where the
  flag is true. This is the baseline control behaviour from PR #3.
- Settings (power limit, tuning, scaling, pools, timezone) run through the asic-rs
  settings backend. No asic-rs settings backend is hardware-confirmed yet, so all
  of them are **dry run only**. A false flag is reported as not supported.
- Whatsminer v1 reports no fault-light support, and the matrix says so (with the
  flag) rather than falling back to a path for another class.

## In review

- PR #17: hashboards, log reading, password change and factory reset, all gated by
  the native flags.
- PR #18: SSH fallbacks for known shells and pool divert. Host keys are pinned on
  first use and credentials are kept in the keychain. These paths are dry run only
  until hardware-confirmed.

Once they merge, their actions appear in the audit viewer automatically, because
they use the same audit record.
