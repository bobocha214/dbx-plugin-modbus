# Modbus TCP

A DBX plugin that reads and writes **Modbus TCP** data from industrial
devices: PLCs, gateways and remote IO couplers.

Built from the official `dbx-plugin create` **Rust + frontend** template.

- [English plugin development guide](https://dbxio.com/en/docs/plugin-development)
- [中文插件开发文档](https://dbxio.com/cn/docs/plugin-development)

## What it contributes

| Contribution | Type | Purpose |
| --- | --- | --- |
| `io.github.bobocha214.modbus.modbus` | connection-provider | Modbus TCP connection form |
| `io.github.bobocha214.modbus.modbus.workbench` | workbench | Register table with polling and guarded writes |
| `io.github.bobocha214.modbus.mcp` | mcp | Read-only tools for the DBX AI assistant |

The workbench is served by `ui/index.html`; the page routes on
`dbxPlugin.contributionId`.

## Interface

The workbench follows the same shape: **configuration on the left, values on
the right.** Nothing that describes *what* to read sits in front of what you are
reading, so the table is never competing with form fields for space.

The styling uses DBX's own design tokens (`--color-background`, `--color-card`,
`--color-muted`, `--color-border`, `--color-primary`, `--radius-md`/`-lg`), so the
plugin follows the surrounding application in light and dark themes instead of
imposing a look of its own. Nothing is hard-coded to a white background.

**Modbus TCP**

- **The connection form asks for four things only** — host, port, slave id and
  request timeout. Byte order and word order are not properties of the cable;
  they belong to how you are reading the data, so they live in the workbench.
- **The sidebar has two tabs. `Devices` manages the endpoints; `Ranges` manages
  what is read from them.**
- **Devices.** Each device is an IP, port, slave id and timeout with its own
  session in the backend, so one workbench can read several PLCs at once. Every
  device shows its state — `connected` with the round-trip it measured,
  `connecting`, `unreachable` with the backend's reason, or `not connected` — and
  a device that fails is retried with a growing, capped delay instead of being
  silently treated as up. A device still referenced by a range cannot be deleted.
  A new workbench seeds its first device from the saved DBX connection, so the IP
  never has to be typed twice.
- **Ranges.** Each entry is one contiguous range — device, area, start address,
  quantity — with its own data type, byte order, word order, display format and
  scan rate. Selecting one loads it: the grid, the form and the decoded readout
  all follow the selection. Ranges are added with `+`, which starts the new one
  where the previous one ended, and deleted from the row itself. The last range
  cannot be deleted.
- **The right side is only addresses and values** — one cell per address, holding
  the address, the value and the untouched raw word, with a fixed-width hex
  gutter so the column lines up.
- **Columns and rows set how much is visible at once.** `Columns` (1–6) is how
  many cells share a row, `Rows` (1–50) how many rows a page holds; together they
  are one page. A range larger than a page pages with `‹ 1 / 2 ›` instead of
  scrolling, so the same number of addresses stays on screen while polling. Both
  are view settings: changing them re-flows the grid and keeps the values already
  read, without another request.
- **The `Log` button opens a read log.** Every read is recorded with its time,
  device, address range, outcome and duration. Repeats of the same outcome
  collapse into one row with a counter, so a one-second poll does not bury
  everything else while the first occurrence of a new failure still gets its own
  line. A failed read records the exact reason — a Modbus exception code, a
  timeout, a refused connection — and the same reason is shown above the grid, so
  it is visible whether or not the log is open.
- **Double-click a value to write it.** Read-only areas (input registers,
  discrete inputs) are not clickable, matching the backend rule.
- **Byte order and word order affect the decoded value only.** The raw word in
  each cell always shows the 16-bit register that came off the wire; changing
  either option re-decodes the value and re-renders the grid without another
  request. Word order is disabled for types narrower than 64 bits, where it would
  have no effect.
- `Tx` (requests sent) and `Err` (failed responses) sit in the header beside the
  Read and Poll actions.
- The divider between the sidebar and the grid is draggable, and adjustable with
  the arrow keys.

The workbench reports connection state in the header and never writes to the
device without a confirmation dialog that names the exact target and value.

## Modbus TCP

Implemented directly in `backend/src/modbus.rs` — Modbus Application Protocol over
MBAP, with no driver crate in the dependency tree.

Supported function codes: `01` read coils, `02` read discrete inputs,
`03` read holding registers, `04` read input registers, `05` write single coil,
`06` write single register, `15` write multiple coils, `16` write multiple registers.

Register decoding supports `uint16`, `int16`, `uint32`, `int32`, `float32`,
`uint64`, `int64` and `float64`, with the four classic byte orders:

| Byte order | Meaning |
| --- | --- |
| `ABCD` | Big endian, the Modbus default |
| `BADC` | Bytes swapped inside every 16-bit word |
| `CDAB` | The two 16-bit halves of every 32-bit group swapped |
| `DCBA` | Fully reversed 32-bit group |

`word_order` (`big` / `little`) reverses the register order first, which matters for
64-bit values that span four registers.

The TCP socket is reused across reads and reconnected once automatically when a device
silently drops an idle connection. Modbus exception responses are reported by name
(for example `Modbus exception 2 (illegal data address)`).

## Connection lifecycle

`connection/test`, `connection/connect` and `connection/disconnect` report their outcome
in the result payload as `{ "success": true|false, "message": "..." }` rather than as a
JSON-RPC error, for both connection types. An unreachable device or
a timeout is something the connection form has to display, not a plugin fault. The
message carries the underlying reason, for example
`Modbus TCP unreachable on 192.168.1.10:502: connection refused`.

Only a missing `connection` payload, an unknown connection type or an unknown method is
returned as a JSON-RPC error. Calls made *after* `connect` (`modbus/read`,
…) reference a session by a bare `connectionId`, so they use a different id resolver than
the lifecycle calls, which carry the full connection snapshot.

### Connection payload layout

DBX serialises a saved connection across three buckets, and the field bindings in
`manifest.json` decide which one a value lands in:

| Bucket | Bindings | Example fields |
| --- | --- | --- |
| top level | `name`, `host`, `port`, `username`, `password`, `database` | `host`, `port` |
| `external_config` | `config` | `unit_id`, `byte_order` |
| `connection_secrets` | `secret` (the default for `password` fields) | `password` |

The provider type is **not** in `database_type` — that key is hard-coded to `"plugin"` for
every plugin connection. The real value arrives as `plugin_connection_type` (the
`database_type` declared by the contribution), with `plugin_connection_provider` carrying
the contribution id. `provider_of` reads them in that order and only falls back to the
legacy `database_type` / `databaseType` / `dbType` spellings, so a host-side rename cannot
silently turn into `Unsupported connection type ""`.

Because secrets sit in their own bucket, field lookups consult the top level, then
`external_config`, then `connection_secrets`. Reading only the top level would silently
drop the password, because secrets are never echoed back as plain config.

## AI tools

The sidecar implements `mcp/tools` and `mcp/call` with one read-only tool:
`modbus_read`. Enable it per plugin under
**Plugin Center → Installed → Built-in AI tools**; installing the plugin alone exposes
nothing.

## Develop

With Node.js 22+ and Rust installed, run the browser development host without DBX:

```bash
dbx-plugin dev --path . --port 5190
```

The CLI compiles the Rust backend. Development credentials are stored as local plaintext
under `.dbx-dev/`; keep that directory out of commits and packages. Use the real DBX host
for final integration testing — the dev host only implements a subset of the Host API.

Run the backend unit tests (Modbus byte-order and register encoding):

```bash
cd backend && cargo test
```

Build an installable candidate:

```bash
dbx-plugin package .
```

The command builds the native backend for the current host, stages `manifest.json`,
`assets/` and `ui/`, then writes an unsigned target-specific `.dbxp` candidate plus a
matching `.artifact.json` into `dist/`.

> The sidecar binary is target-specific, so the plugin ships one package
> per OS and CPU architecture rather than a single `universal` package. A local
> build therefore only ever produces **one** target — the one you are running.
> See [Multi-platform builds](#multi-platform-builds).

## Multi-platform builds

The Rust sidecar is a native binary, so every OS and CPU architecture needs its own
build. `dbx-plugin package` compiles the backend **on the host it runs on**; it does
not cross-compile, and `--target` / `DBX_PLUGIN_TARGET` only label the artifact. A
build on Windows can never emit `linux-x64`, whichever target is passed.

Multi-platform coverage therefore comes from building once per platform, which the two
workflows in `.github/workflows/` do with a runner/target matrix:

| Target | Runner | Notes |
| --- | --- | --- |
| `linux-x64` | `ubuntu-24.04` | |
| `linux-arm64` | `ubuntu-24.04-arm` | ARM runners need a plan that allows them |
| `windows-x64` | `windows-2022` | |
| `darwin-x64` | `macos-15-intel` | Intel macOS runners are being retired upstream |
| `darwin-arm64` | `macos-15` | Apple silicon |

- **`.github/workflows/plugin-build.yml`** builds every target on pushes to `main`, on
  pull requests and on demand, uploading one `.dbxp` plus its `.artifact.json` per
  target as workflow artifacts. It doubles as the portability gate: a platform-specific
  compile error surfaces here instead of at release time.
- **`.github/workflows/plugin-release.yml`** runs the same matrix when a GitHub Release
  is published, and attaches every target plus the merged `release-candidates.json` to
  that release.

If a runner is unavailable to your account, delete just that matrix entry — the
remaining targets still build and the plugin installs on the targets you shipped. To
add Windows on ARM, add `{"runner":"windows-11-arm","target":"windows-arm64"}` to both
matrices.

The backend itself is platform-neutral: `backend/src` uses only `std::net`, `std::sync`,
`std::time` and `serde_json`, with no `#[cfg(...)]` branches and no Windows-only APIs,
and `dbx-plugin.toml` declares `binary = "dbx-plugin-modbus"` without an extension so
the packager appends `.exe` on Windows.

## Acceptance checklist

- Empty configuration, wrong credentials, timeouts, an unreachable device and a sidecar
  restart all surface as a readable error rather than a hang.
- Light and dark DBX themes, `zh-CN` and `en`.
- Reconnecting, closing a workbench, opening the same workbench twice, and an empty
  workbench context.
- No development data or credentials inside the packaged `.dbxp`.

## Release

1. Publish a GitHub Release. The generated workflow builds unsigned candidates for every target.
2. If this repository is registered with `autoUpdate: true`, DBX Store automatically creates or updates a candidate PR. Otherwise, open one candidate PR against **`t8y2/dbx-store:main`** with the release and `release-candidates.json` URL; a submission Issue is not required.
3. After review, DBX Store signs approved candidates with the official repository key and updates the same PR with the installable assets and signed metadata.

Do not submit ordinary plugin source to `t8y2/dbx`; that repository accepts plugin host, SDK, CLI, schema, documentation and official-example changes.
