# Safety model

TerraMine changes the state of real mining hardware, so every write goes through
one pipeline with conservative defaults. The rule is simple: **nothing is sent
unless a capability gate allows it, and nothing is sent live unless the path has
been confirmed on real hardware.**

## 1. Detect, then gate

Discovery builds a device profile (make, model, firmware, version and a
`DeviceClass`). Every path declares the classes it may run on, and a path is
never offered outside them:

- AxeOS HTTP calls are only made to AxeOS devices (Bitaxe, NerdAxe and other
  ESP-Miner forks). They are never sent to an Antminer, Whatsminer, Braiins or
  Avalon device.
- cgminer/bmminer RPC is only used on miners that expose that API, and never on
  AxeOS.
- The Antminer stock CGI is only used on stock Antminer firmware.
- asic-rs native calls are gated on the miner's own `supports_*()` flags.
  A flag that is `false` means the action is **not supported**. It is never
  reported as a success.

The mock end-to-end tests count requests per path to prove these rules.

## 2. Fallback order and "paths tried"

For each action and device, TerraMine builds an ordered chain of paths. It tries
them in order and records why each one was skipped or failed. If no path works,
the result is "can't do this" with the full list. The **Capabilities…** matrix
shows the same list for every miner without sending anything.

## 3. Hardware confirmation (live vs. dry run only)

Each path is either:

- **Live**: confirmed on a real unit. `Capabilities::with` for control actions,
  and `SettingsDriver::confirmed_for` returning true for settings.
- **Dry run only**: implemented and mock-tested, but not yet seen working on real
  hardware. `Capabilities::with_unconfirmed`, or `confirmed_for` returning false.

A live request on a dry-run-only path is refused with `no_path`, and the reason
is recorded. A dry run on the same path is allowed. It plans the change, records
it and sends nothing. See [HARDWARE_CONFIRMATION.md](HARDWARE_CONFIRMATION.md).

## 4. The write pipeline (settings)

1. **Dry run first**: every new action class opens with *Dry run* checked.
2. **Limits**: a checked-in per-model table (`src-tauri/src/control/safety/limits.json`)
   sets frequency, voltage, temperature and fan ranges. Values outside the range
   are refused before any I/O. Frequency raises need an explicit opt-in.
3. **Snapshot**: the current config is read and kept before the write.
4. **Write** through the selected path.
5. **Read-back**: the device must report the requested values.
6. **Health watch**: hashrate, temperatures and reachability are checked for a
   window after the write.
7. **Automatic rollback**: if read-back or health fails, the snapshot is restored
   and the row is marked `rolled_back`.

Batches stop early when too many devices fail.

## 5. Irreversible actions

Actions such as factory reset, firmware upgrade and password change are marked
irreversible. They run one device at a time and require typing the action name.
They also need an explicit allow flag. TerraMine refuses them otherwise.

## 6. Secrets

- Pool passwords, miner web passwords and SSH credentials are held as secrets.
  They are stored in the OS keychain and never written to logs or the audit file.
- The audit viewer and its CSV/JSONL export redact again on output, as defence
  in depth. This covers any field or `key=value` text that looks like a password,
  passphrase, token, secret, private key, cookie, authorization header or API key.
- CSV exports guard against spreadsheet formula injection.

## 7. Audit log

Every write produces an `attempt` record before anything is sent and an
`outcome` record afterwards. Dry runs and refusals produce records too. The
records go to `<app data>/audit/actions.jsonl` as one JSON object per line, with
the run id, device, requested change, snapshot id, previous state, paths tried
and result. **Audit log…** in the toolbar filters by miner, action, result,
phase, mode, date and text, and exports to CSV or JSONL.
