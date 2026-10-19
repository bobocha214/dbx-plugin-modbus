# everlink

A DBX plugin that reads and writes **Modbus TCP** and **OPC UA** data from industrial
devices: PLCs, gateways, remote IO couplers and OPC UA servers.

Built from the official `dbx-plugin create` **Rust + frontend** template.

- [English plugin development guide](https://dbxio.com/en/docs/plugin-development)
- [中文插件开发文档](https://dbxio.com/cn/docs/plugin-development)

## What it contributes

| Contribution | Type | Purpose |
| --- | --- | --- |
| `io.github.yuzhi.everlink.modbus` | connection-provider | Modbus TCP connection form |
| `io.github.yuzhi.everlink.opcua` | connection-provider | OPC UA connection form |
| `io.github.yuzhi.everlink.modbus.workbench` | workbench | Register table with polling and guarded writes |
| `io.github.yuzhi.everlink.opcua.workbench` | workbench | Address space browser, reads and subscriptions |
| `io.github.yuzhi.everlink.mcp` | mcp | Read-only tools for the DBX AI assistant |

Both workbenches are served by the same `ui/index.html`; the page routes on
`dbxPlugin.contributionId`.

## Interface

Both workbenches follow the same shape: **configuration on the left, values on
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

**OPC UA**

- Left sidebar: the **address space** tree, with a namespace picker and rebrowse.
  The tree prefixes each node with a colour-coded class glyph (`O` object,
  `V` variable, `M` method, `W` view, `T` type) standing in for an icon set.
  Clicking expands a node; double-clicking adds it to the watch list.
- The **namespace picker highlights** one namespace at a time — matching nodes go
  bold and the rest dim, rather than filtering rows out, so the tree shape stays
  stable.
- Right side: the **watch list** — node id, value, data type, timestamp and
  quality. Clicking a value opens the write dialog; a node that is not `Good` is
  flagged.
- The **attributes** panel sits beneath the watch list and describes the node
  selected in the tree.

Both workbenches report connection state in the header and never write to the
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

## OPC UA

Implemented in `backend/src/opcua_client.rs` on top of
[`async-opcua`](https://crates.io/crates/async-opcua) 0.19. Only the `client` feature is
enabled, and that fork's crypto backend is pure Rust (`rsa`, `sha2`, `aes`), so the
sidecar cross-compiles without OpenSSL, CMake, Perl or NASM.

- Security policies: `None`, `Basic128Rsa15`, `Basic256Sha256`
- Message security modes: `None`, `Sign`, `SignAndEncrypt`
- Identity: anonymous or user name / password (password goes to the DBX Secret Store)
- Browse the address space with continuation points, read, write, and subscribe
- Subscriptions stream `opcua/dataChange` events into the workbench

A `security_policy` of `None` forces `MessageSecurityMode::None`, because OPC UA forbids
transport security under that policy. The reverse combination — a real security policy
with security mode `None` — is rejected instead of being silently downgraded.

Client certificates and the trust store live in `DBX_PLUGIN_DATA_DIR/pki`, which DBX
preserves across plugin upgrades. Server certificates are trusted on first use
(`trust_server_certs(true)`), which is the only practical default for field devices
that ship self-signed certificates.

### Node ids

`opcua/read`, `opcua/write` and `opcua/subscribe` take node ids in the usual
`ns=<index>;<i|s|g|b>=<value>` form, for example `ns=2;i=5`. A leading `svr=<index>;`
segment — as printed by many OPC UA tools — is accepted and ignored, because a server
index only means something to a multi-server client.

Browse results are reduced to that same form before they reach the host. The wire
format for references is an `ExpandedNodeId`, whose text form carries `svr=`/`nsu=`
prefixes that `NodeId` cannot parse back; emitting the raw form would make every node
discovered through the address-space tree fail with `BadNodeIdInvalid` on the first read.

### Browsing

`opcua/browse` walks `HierarchicalReferences` by default, so the tree shows children
rather than a node's type. Optional parameters:

| Parameter | Default | Meaning |
| --- | --- | --- |
| `nodeId` | `i=84` (Objects) | Node to browse |
| `direction` | `forward` | `forward`, `inverse` or `both` |
| `referenceTypeId` | hierarchical references | Pass `i=0` for every reference, including `HasTypeDefinition` |
| `includeSubtypes` | `true` | Include subtypes of the chosen reference type |
| `maxReferences` | `500` | Page size, clamped to 2000 |

Continuation points are followed automatically, with a loop guard: `async-opcua` does not
clear `continuation_point` when a page is exhausted, so the walk also stops as soon as a
page adds no new references.

## Connection lifecycle

`connection/test`, `connection/connect` and `connection/disconnect` report their outcome
in the result payload as `{ "success": true|false, "message": "..." }` rather than as a
JSON-RPC error, for both connection types. A rejected password, an unreachable device or
a timeout is something the connection form has to display, not a plugin fault. The
message carries the underlying reason, for example
`OPC UA session on opc.tcp://plc:4840 ended: BadUserAccessDenied`.

Only a missing `connection` payload, an unknown connection type or an unknown method is
returned as a JSON-RPC error. Calls made *after* `connect` (`modbus/read`, `opcua/read`,
…) reference a session by a bare `connectionId`, so they use a different id resolver than
the lifecycle calls, which carry the full connection snapshot.

### Connection payload layout

DBX serialises a saved connection across three buckets, and the field bindings in
`manifest.json` decide which one a value lands in:

| Bucket | Bindings | Example fields |
| --- | --- | --- |
| top level | `name`, `host`, `port`, `username`, `password`, `database` | `host`, `port` |
| `external_config` | `config` | `unit_id`, `byte_order`, `security_policy` |
| `connection_secrets` | `secret` (the default for `password` fields) | `password` |

The provider type is **not** in `database_type` — that key is hard-coded to `"plugin"` for
every plugin connection. The real value arrives as `plugin_connection_type` (the
`database_type` declared by the contribution), with `plugin_connection_provider` carrying
the contribution id. `provider_of` reads them in that order and only falls back to the
legacy `database_type` / `databaseType` / `dbType` spellings, so a host-side rename cannot
silently turn into `Unsupported connection type ""`.

Because secrets sit in their own bucket, field lookups consult the top level, then
`external_config`, then `connection_secrets`. Reading only the top level would silently
downgrade an authenticated OPC UA connection to anonymous access.

Closing an OPC UA session waits for at most five seconds. The client's
`Session::disconnect` round-trips a `CloseSession` request through the session's event
loop and then waits for that loop to observe the disconnection, so the loop has to stay
alive during the close and a loop that has already given up has to be abandoned rather
than awaited.

## AI tools

The sidecar implements `mcp/tools` and `mcp/call` with three read-only tools:
`modbus_read`, `opcua_read` and `opcua_browse`. Enable them per plugin under
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

> The sidecar binary is target-specific, so OPC UA and Modbus plugins ship one package
> per OS and CPU architecture rather than a single `universal` package.

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
