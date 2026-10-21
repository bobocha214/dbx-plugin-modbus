## Plugin submission

- Plugin ID: `io.github.yuzhi.modbus`
- Version: `0.1.0`
- Publisher ID: `yuzhi`

## Source repository and tag

https://github.com/bobocha214/dbx-plugin-modbus

Built from the immutable tag `v0.1.0`. The candidates are the unsigned
`.dbxp` assets attached to that release.

## Capabilities and user workflow

The plugin contributes a single connection provider (`Modbus TCP`) plus the
workbench and one read-only AI tool:

- **Connection provider** — the user adds a Modbus TCP connection (host, port,
  unit id, request timeout), then tests it from the connection form. Failures
  surface the underlying reason (`connect failed`, timeout, exception).
- **Workbench** — configuration on the left, values on the right. Each
  configuration defines address ranges over the four Modbus areas (coils,
  discrete inputs, holding registers, input registers), a data type and a byte
  order; the grid shows decoded values, can poll on an interval, and keeps a
  bounded in-memory read log. Writes require a confirmation dialog naming the
  exact address and value, and read-only areas (`input`) reject writes.
- **AI tool** — one read-only tool, `modbus_read`. No write tool is exposed to
  the assistant; `ai_tools: true` is the only MCP contribution.

## Permissions, data access, and network access

Manifest permissions: `host.storage` only.

- `host.storage` — persists the user's register configurations (addresses,
  data type, byte order, poll interval, grid layout). Live register values are
  deliberately not persisted.
- **Network** — outbound Modbus TCP to the host and port the user typed in the
  connection form (default port 502). Nothing else is contacted: no telemetry,
  no update check, no third-party APIs.
- **Credentials** — none. The connection form has no username or password
  field, and nothing is written to the DBX Secret Store.
- **Filesystem** — the plugin reads no user files and writes none. The workbench
  runs inside the host sandbox and uses `host.storage` instead of
  `localStorage`.

## Native sidecar behavior

Yes — the plugin ships a Rust sidecar (`bin/<target>/dbx-plugin-modbus`), one
binary per platform, built from `backend/src`.

- Transport: `stdio-jsonl` over stdin/stdout. No listening sockets, no ports.
- Filesystem: no reads or writes; no data directory is used.
- Subprocesses: none. No external commands are executed.
- Network: only the outbound Modbus TCP sockets described above, opened on user
  action and closed when the connection is closed.
- Dependencies: `std::net` and `serde_json` only — no TLS stack, no async
  runtime, no platform-specific crate.

## License

MIT (see `LICENSE`).

## Homepage or support URL

https://github.com/bobocha214/dbx-plugin-modbus

## Confirmation

- [x] The candidate packages are unsigned and match the disclosed source tag.
- [x] I have not included private keys, tokens, credentials, or proprietary data.
- [x] The candidates are listed in `candidates/<plugin-id>.json`; `plugins/` and `catalog/` were not edited manually.
