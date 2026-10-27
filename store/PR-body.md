## Plugin update

- Plugin ID: `io.github.yuzhi.modbus` (existing catalog entry, first published at `0.1.0` by merged PR #323)
- Version: `0.2.1`
- Publisher ID: `yuzhi` (unchanged — this is a version update, not a new listing or an identity transfer)

`plugins/io.github.yuzhi.modbus.json` and `catalog/` were not edited by hand; the
only added file is `candidates/io.github.yuzhi.modbus.json`.

## Response to the review on #369

| Point | What changed |
| --- | --- |
| Duplicate listing / identity transfer | The earlier submission in this PR used a new id `io.github.bobocha214.modbus` and a new publisher. That was wrong: it would have created a second entry and left 0.1.0 users without an upgrade path. This PR is updated in place and goes back to `io.github.yuzhi.modbus` + `yuzhi`, so 0.1.0 installs upgrade instead of getting a second entry. No separate new-id PR remains open. |
| "Writes have a second confirmation" was untrue | The claim is gone from the manifest descriptions, both READMEs and the store metadata. The behaviour was also tightened: **only `Enter` writes**. `Esc` *and* leaving the field both cancel, and a dropped edit reports "Not written — press Enter" instead of vanishing. A double-click that types nothing still writes nothing. |
| No LICENSE inside the package | `dbx-plugin.toml` now stages it: `[package] include = ["assets", "ui", "LICENSE"]`. The `.dbxp` carries `LICENSE` at the package root. |
| Description should name the connection target | The plugin description now says it connects to the host and port the user enters (502 by default). |

## Version

`0.2.1` is a fresh tag. `0.2.0` was published as a GitHub release while the
plugin still carried the `bobocha214` identity, so its artifacts cannot be
reused: the store binds candidates to exact bytes, and those bytes declare
`id: io.github.bobocha214.modbus` / `publisher: bobocha214`.

## Source repository and tag

https://github.com/bobocha214/dbx-plugin-modbus — tag `v0.2.1`

Candidates are the unsigned `.dbxp` assets attached to that release, built by
`t8y2/dbx/.github/workflows/plugin-release-reusable.yml@main` on one native
runner per target (five targets). SHA-256 and size are pinned to the bytes the
release workflow uploaded; `release-candidates.json` on the release carries the
same values.

## What 0.2.1 delivers

- **Connection form asks for a workspace name only.** Host, port, slave id and
  timeout belong to the devices added inside the workbench, so the host dialog no
  longer asks for values the user has not looked up. A workbench opened with no
  connection still works (`standalone` in the header).
- **Devices and ranges are managed in the workbench.** Each device is an
  endpoint with its own session and status; each range is one contiguous block
  over coils, discrete inputs, holding registers or input registers with its own
  data type, byte order, word order, format and scan interval. A new range reads
  ten addresses.
- **Editing a definition takes effect immediately.** Changing device, area,
  start, quantity or data type issues a fresh read itself; a wider data type
  fetches the extra registers instead of leaving half the grid empty. Editing an
  endpoint re-dials the session.
- **Writes are explicit.** Double-click a holding register or coil, type, press
  `Enter`. Read-only areas reject writes in the backend as well.

## Permissions, data access, and network access

Manifest permissions: `host.storage` only.

- `host.storage` — persists the user's devices and range configurations. Live
  register values are deliberately not persisted.
- **Network** — outbound Modbus TCP to the `host:port` the user typed into a
  device inside the workbench (port 502 by default). Nothing else is contacted:
  no telemetry, no update check, no third-party APIs.
- **Credentials** — none. No username or password field, nothing written to the
  DBX Secret Store.
- **Filesystem** — the plugin reads no user files and writes none; the workbench
  uses `host.storage` instead of `localStorage`.

## Native sidecar behavior

Yes — a Rust sidecar (`bin/<target>/dbx-plugin-modbus`), one binary per platform,
built from `backend/src`.

- Transport: `stdio-jsonl` over stdin/stdout. No listening sockets, no ports.
- Filesystem: no reads or writes; no data directory.
- Subprocesses: none.
- Network: only the outbound Modbus TCP sockets described above.
- Dependencies: `std::net`, `std::sync`, `std::time` and `serde_json` — no TLS
  stack, no async runtime, no platform-specific crate.

## License

MIT (see `LICENSE`, shipped inside the package).

## Homepage or support URL

https://github.com/bobocha214/dbx-plugin-modbus

## Confirmation

- [x] The candidate packages are unsigned and match the disclosed source tag.
- [x] I have not included private keys, tokens, credentials, or proprietary data.
- [x] The candidates are listed in `candidates/io.github.yuzhi.modbus.json`;
      `plugins/` and `catalog/` were not edited manually.
- [x] `verified` is left to the maintainers.
