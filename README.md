# Terra Mine

Desktop app for **LAN ASIC miner discovery** using [Tauri](https://tauri.app/) 2 and [asic-rs](https://crates.io/crates/asic-rs) (Apache-2.0). **Licensed under GNU GPLv3** — see [LICENSE](LICENSE). If you distribute this program or a derivative, comply with the GPL (source offer, notices, etc.).

## Develop

```bash
npm install
npm run tauri dev
```

## Build

```bash
npm run tauri build
```

## Miner actions

TerraMine can act on miners it discovers, always behind a capability gate and
the safety engine (dry run, limits, snapshot, read-back, health watch,
rollback). **Which actions a given miner supports is never hard-coded in the
UI**: open **Capabilities…** in the toolbar for the live matrix (per miner or
per firmware, with every path tried), and **Audit log…** for every attempt and
outcome. "Live" means the path is confirmed on real hardware; "dry run only"
means TerraMine will plan and log the action but refuses to send it.

| Action | Paths (in order) | Status today |
|---|---|---|
| Start / Stop | asic-rs `resume`/`pause` · AxeOS HTTP (≥ 2.12 or probed) · S9 stock CGI sleep mode (miner-mode firmware only) | live where the gate allows it |
| Reboot | asic-rs `restart` · AxeOS HTTP · S9 stock CGI · cgminer RPC `restart` | asic-rs / AxeOS / S9 CGI: live · cgminer RPC: dry run only |
| Fault light | asic-rs `set_fault_light` · AxeOS identify (≥ 2.12) · S9 stock CGI | asic-rs: live where supported · AxeOS 2.15: live · S9 CGI: dry run only |
| Frequency, fan (manual/auto) | AxeOS HTTP | AxeOS 2.15.x: **hardware-confirmed** · other lines: dry run only |
| Core voltage, fan target temp | AxeOS HTTP | dry run only |
| Power limit, tuning (percent/mode/hashrate/preset), scaling, pools, timezone | asic-rs native (`supports_*` gated) | dry run only (no asic-rs settings backend is hardware-confirmed yet) |
| Hashboards, logs, change password, factory reset | asic-rs native | in review (PR #17) — irreversible ones need typed confirmation |
| SSH fallbacks, pool divert | SSH (known shells) · pool divert | in review (PR #18) — dry run only until hardware-confirmed |

The table summarizes the code at the time of writing; the in-app matrix is the
source of truth.

- [docs/SAFETY.md](docs/SAFETY.md) — the safety model
- [docs/FIRMWARE.md](docs/FIRMWARE.md) — per-firmware notes, including the Antminer S9 API allow-list findings
- [docs/HARDWARE_CONFIRMATION.md](docs/HARDWARE_CONFIRMATION.md) — how a path goes from dry run only to live

## Third-party

Rust and npm dependencies include permissive and Apache-2.0 crates; see [NOTICE](NOTICE) for a short summary.
