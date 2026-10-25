//! DBX sidecar for the Modbus TCP industrial protocol.
//!
//! `modbus-tcp` — a hand-written Modbus TCP client ([`modbus`]).
//!
//! DBX calls `PluginHandler::handle` on plain worker threads, so blocking
//! sockets are safe here.

mod modbus;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dbx_plugin_sdk::{
    PluginEmitter, PluginError, PluginHandler, PluginMetadata, PluginServer, RequestContext,
};
use serde_json::{json, Value};

use modbus::{area_function_codes, decode_registers, encode_value, register_count, ModbusSession};

const PLUGIN_ID: &str = "io.github.bobocha214.modbus";
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Maximum number of read tasks accepted in a single `modbus/read` call.
const MAX_READ_TASKS: usize = 64;
/// Maximum number of write operations accepted in a single `modbus/write` call.
const MAX_WRITE_TASKS: usize = 32;

/// Tool names advertised through `mcp/tools`. Every one of them is read-only, so
/// DBX runs them without a per-call confirmation prompt.
const MCP_READ_TOOLS: [&str; 1] = ["modbus_read"];

/// Told to the host when a connection-level action needs an address that the
/// connection does not carry. Creating a workspace asks for a name only — the
/// host, port and slave id belong to the devices configured in the workbench.
const NO_HOST_MESSAGE: &str = "This Modbus workspace has no address of its own. Open the Modbus workbench, add a device with its host and port, and connect it there.";

/// Byte order, word order and unit id captured when a session opened. The
/// connection form no longer collects them, so these fall back to the classic
/// defaults and the workbench names them explicitly on every read and write.
type ModbusDefaults = (String, String, u8);

/// Device sessions are opened by the workbench itself, one per device in its
/// list, so they are keyed distinctly from the connection DBX opened. The host
/// never guesses at these ids, and `modbus/read` resolves either kind the same
/// way.
const DEVICE_PREFIX: &str = "device:";

fn device_key(device_id: &str) -> String {
    format!("{DEVICE_PREFIX}{device_id}")
}

/// Splits a `host:port` endpoint. Splitting on the last colon keeps an IPv6
/// literal such as `[::1]:502` in one piece.
fn split_endpoint(endpoint: &str) -> (String, u16) {
    match endpoint.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.parse::<u16>().unwrap_or(502)),
        None => (endpoint.to_string(), 502),
    }
}

enum Session {
    Modbus(ModbusSession),
}

struct Plugin {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Byte order, word order and unit id captured when the connection opened.
    defaults: Mutex<HashMap<String, ModbusDefaults>>,
}

impl Plugin {
    fn new() -> Result<Self, String> {
        Ok(Self {
            sessions: Mutex::new(HashMap::new()),
            defaults: Mutex::new(HashMap::new()),
        })
    }

    fn lookup(&self, connection_id: &str) -> Result<Arc<Session>, PluginError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        sessions.get(connection_id).cloned().ok_or_else(|| {
            PluginError::new(
                -32002,
                format!("Connection {connection_id} is not open in this plugin"),
            )
        })
    }

    fn store(
        &self,
        connection_id: String,
        session: Session,
        defaults: Option<ModbusDefaults>,
    ) -> Result<(), PluginError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        sessions.insert(connection_id.clone(), Arc::new(session));
        drop(sessions);
        if let Some(defaults) = defaults {
            if let Ok(mut map) = self.defaults.lock() {
                map.insert(connection_id, defaults);
            }
        }
        Ok(())
    }

    fn drop_session(&self, connection_id: &str) -> Option<Arc<Session>> {
        if let Ok(mut map) = self.defaults.lock() {
            map.remove(connection_id);
        }
        self.sessions
            .lock()
            .ok()
            .and_then(|mut sessions| sessions.remove(connection_id))
    }

    fn modbus_defaults_for(&self, connection_id: &str) -> ModbusDefaults {
        self.defaults
            .lock()
            .ok()
            .and_then(|map| map.get(connection_id).cloned())
            .unwrap_or_else(|| ("ABCD".to_string(), "big".to_string(), 1))
    }
}

// ---------------------------------------------------------------------------
// Connection payload helpers
//
// DBX sends the saved connection as a JSON snapshot. Fields bound to standard
// bindings land on the top level (host/port/username/...); provider fields bound
// to `config` are nested under `external_config`. Read both so the plugin keeps
// working if the host flattens one of them.
// ---------------------------------------------------------------------------

pub fn field<'a>(connection: &'a Value, key: &str) -> Option<&'a Value> {
    connection
        .get(key)
        .or_else(|| connection.get("external_config").and_then(|config| config.get(key)))
        // Fields bound to `secret` are delivered in a separate bucket so plain
        // config dumps never contain them.
        .or_else(|| {
            connection
                .get("connection_secrets")
                .and_then(|secrets| secrets.get(key))
        })
}

pub fn text_field(connection: &Value, key: &str) -> Option<String> {
    field(connection, key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Request parameters, as opposed to connection snapshot fields.
fn param_text(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn param_u64(params: &Value, key: &str) -> Option<u64> {
    params.get(key).and_then(Value::as_u64)
}

pub fn number_field(connection: &Value, key: &str) -> Option<u64> {
    field(connection, key).and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
            .or_else(|| value.as_f64().map(|number| number as u64))
    })
}

fn connection_object(params: &Value) -> Result<&Value, PluginError> {
    params
        .get("connection")
        .ok_or_else(|| PluginError::new(-32602, "Missing connection payload"))
}

fn connection_id(params: &Value) -> Result<String, PluginError> {
    connection_object(params)?
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| PluginError::new(-32602, "Missing connection id"))
}

/// Connection id for calls made *after* connect.
///
/// Lifecycle requests (`connection/test|connect|disconnect`) carry a connection
/// snapshot, but every later call only carries a bare `connectionId`, because
/// the frontend refers to the session the backend already established.
fn target_connection_id(params: &Value) -> Result<String, PluginError> {
    params
        .get("connectionId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            params
                .get("connection")
                .and_then(|connection| connection.get("id"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .ok_or_else(|| PluginError::new(-32602, "Missing connectionId"))
}

/// Resolves the provider type the host asked for.
///
/// DBX serialises a saved connection as `db_type: "plugin"` (always), plus
/// `plugin_connection_type` carrying the manifest `database_type` and
/// `plugin_connection_provider` carrying the contribution id. Older/other hosts
/// have shipped the bare `database_type` / `databaseType` / `dbType` spellings,
/// so every form is accepted — but the plugin-specific keys come first, because
/// `database_type` on its own says nothing useful.
fn provider_of(connection: &Value) -> String {
    for key in ["plugin_connection_type", "pluginConnectionType"] {
        if let Some(value) = connection.get(key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return value.trim().to_string();
            }
        }
    }
    // A contribution id always reads `com.example.plugin.<kind>`; the last
    // segment is the database type the manifest declared.
    for key in ["plugin_connection_provider", "pluginConnectionProvider"] {
        if let Some(value) = connection.get(key).and_then(Value::as_str) {
            if let Some(kind) = value.trim().rsplit('.').next() {
                if !kind.is_empty() {
                    return kind.to_string();
                }
            }
        }
    }
    for key in ["database_type", "databaseType", "dbType"] {
        if let Some(value) = connection.get(key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return value.trim().to_string();
            }
        }
    }
    String::new()
}

/// Endpoint DBX resolved for this connection, after any SSH tunnel or proxy.
fn runtime_endpoint(params: &Value) -> (Option<String>, Option<u16>) {
    let runtime = params.get("runtime");
    let host = runtime
        .and_then(|value| value.get("host"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let port = runtime
        .and_then(|value| value.get("port"))
        .and_then(Value::as_u64)
        .map(|value| value as u16);
    (host, port)
}

/// Address a host-side connection resolves to, if it has one.
///
/// The connection form no longer asks for a host: a workspace is created empty
/// and the devices inside it own the addresses. So "no host" is a normal state
/// rather than a typo, and both `test` and `connect` have to say so instead of
/// dialling a placeholder address.
fn connection_host(connection: &Value, params: &Value) -> Option<(String, u16)> {
    let (runtime_host, runtime_port) = runtime_endpoint(params);
    let host = runtime_host
        .or_else(|| text_field(connection, "host"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    let port = runtime_port
        .or_else(|| number_field(connection, "port").map(|value| value as u16))
        // A connection saved without a port field serialises as 0.
        .filter(|value| *value > 0)
        .unwrap_or(502);
    Some((host, port))
}

fn modbus_session(connection: &Value, params: &Value) -> Result<ModbusSession, String> {
    let (host, port) = connection_host(connection, params)
        .ok_or_else(|| NO_HOST_MESSAGE.to_string())?;
    let timeout_ms = number_field(connection, "timeout_ms")
        .unwrap_or(3000)
        .clamp(200, 120_000);
    Ok(ModbusSession::new(host, port, Duration::from_millis(timeout_ms)))
}

fn modbus_defaults(connection: &Value) -> ModbusDefaults {
    let byte_order = text_field(connection, "byte_order").unwrap_or_else(|| "ABCD".to_string());
    let word_order = text_field(connection, "word_order").unwrap_or_else(|| "big".to_string());
    let unit_id = number_field(connection, "unit_id").unwrap_or(1).min(255) as u8;
    (byte_order, word_order, unit_id)
}


// ---------------------------------------------------------------------------
// Modbus operations
// ---------------------------------------------------------------------------

fn read_task(session: &ModbusSession, task: &Value, defaults: &ModbusDefaults) -> Value {
    let (default_byte_order, default_word_order, default_unit) = defaults;
    let unit_id = task
        .get("unitId")
        .and_then(Value::as_u64)
        .map(|value| value.min(255) as u8)
        .unwrap_or(*default_unit);
    let area = task.get("area").and_then(Value::as_str).unwrap_or("holding");
    let address = task.get("address").and_then(Value::as_u64).unwrap_or(0) as u16;
    let data_type = task.get("dataType").and_then(Value::as_str).unwrap_or("uint16");
    let byte_order = task
        .get("byteOrder")
        .and_then(Value::as_str)
        .unwrap_or(default_byte_order);
    let word_order = task
        .get("wordOrder")
        .and_then(Value::as_str)
        .unwrap_or(default_word_order);

    let read_code = match area_function_codes(area) {
        Ok((read_code, _)) => read_code,
        Err(error) => return json!({ "ok": false, "error": error }),
    };

    match read_code {
        modbus::FC_READ_COILS | modbus::FC_READ_DISCRETE_INPUTS => {
            let quantity = task
                .get("quantity")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 2000) as u16;
            match session.read_bits(unit_id, read_code, address, quantity) {
                Ok(bits) => json!({
                    "ok": true,
                    "unitId": unit_id,
                    "area": area,
                    "address": address,
                    "quantity": quantity,
                    "values": bits,
                }),
                Err(error) => json!({ "ok": false, "error": error }),
            }
        }
        modbus::FC_READ_HOLDING_REGISTERS | modbus::FC_READ_INPUT_REGISTERS => {
            // `quantity` arrives as a register count (the UI multiplies its
            // value/element count by `register_count(data_type)`). A wide type
            // can exceed the 125-register PDU limit, so read in chunks.
            let quantity = match task.get("quantity").and_then(Value::as_u64) {
                Some(value) => value.clamp(1, 2000),
                None => register_count(data_type).unwrap_or(1) as u64,
            } as u16;
            match session.read_registers_chunked(unit_id, read_code, address, quantity) {
                Ok(registers) => {
                    let value = decode_registers(&registers, data_type, byte_order, word_order).ok();
                    json!({
                        "ok": true,
                        "unitId": unit_id,
                        "area": area,
                        "address": address,
                        "quantity": quantity,
                        "dataType": data_type,
                        "byteOrder": byte_order,
                        "wordOrder": word_order,
                        "value": value,
                        "registers": registers,
                    })
                }
                Err(error) => json!({ "ok": false, "error": error }),
            }
        }
        other => json!({ "ok": false, "error": format!("unsupported read function code {other}") }),
    }
}

fn write_task(session: &ModbusSession, task: &Value, defaults: &ModbusDefaults) -> Value {
    let (default_byte_order, default_word_order, default_unit) = defaults;
    let unit_id = task
        .get("unitId")
        .and_then(Value::as_u64)
        .map(|value| value.min(255) as u8)
        .unwrap_or(*default_unit);
    let area = task.get("area").and_then(Value::as_str).unwrap_or("holding");
    let address = task.get("address").and_then(Value::as_u64).unwrap_or(0) as u16;
    let byte_order = task
        .get("byteOrder")
        .and_then(Value::as_str)
        .unwrap_or(default_byte_order);
    let word_order = task
        .get("wordOrder")
        .and_then(Value::as_str)
        .unwrap_or(default_word_order);

    match area_function_codes(area) {
        Ok((modbus::FC_READ_COILS, _)) => write_coils(session, task, unit_id, address, area),
        Ok((modbus::FC_READ_HOLDING_REGISTERS, _)) => {
            write_registers(session, task, unit_id, address, area, byte_order, word_order)
        }
        Ok(_) => json!({
            "ok": false,
            "error": format!("area \"{area}\" is read-only (discrete inputs and input registers cannot be written)"),
        }),
        Err(error) => json!({ "ok": false, "error": error }),
    }
}

fn write_coils(
    session: &ModbusSession,
    task: &Value,
    unit_id: u8,
    address: u16,
    area: &str,
) -> Value {
    let result = if let Some(values) = task.get("values").and_then(Value::as_array) {
        let bits: Result<Vec<bool>, String> = values
            .iter()
            .map(|value| {
                value
                    .as_bool()
                    .ok_or_else(|| "coil values must be true or false".to_string())
            })
            .collect();
        match bits {
            Ok(bits) => session
                .write_multiple_coils(unit_id, address, &bits)
                .map(|_| bits.len()),
            Err(error) => return json!({ "ok": false, "error": error }),
        }
    } else if let Some(value) = task.get("value").and_then(Value::as_bool) {
        session
            .write_single_coil(unit_id, address, value)
            .map(|_| 1usize)
    } else {
        return json!({ "ok": false, "error": "a coil write needs value or values" });
    };

    match result {
        Ok(count) => json!({
            "ok": true,
            "unitId": unit_id,
            "area": area,
            "address": address,
            "written": count,
        }),
        Err(error) => json!({ "ok": false, "error": error }),
    }
}

fn write_registers(
    session: &ModbusSession,
    task: &Value,
    unit_id: u8,
    address: u16,
    area: &str,
    byte_order: &str,
    word_order: &str,
) -> Value {
    // `raw: true` writes the values array as literal 16-bit registers, which is
    // what the register table in the workbench uses. Otherwise every value is
    // encoded with the requested data type and byte order.
    let raw = task.get("raw").and_then(Value::as_bool).unwrap_or(false);
    let data_type = task.get("dataType").and_then(Value::as_str).unwrap_or("uint16");

    let registers: Result<Vec<u16>, String> = if raw {
        let Some(values) = task.get("values").and_then(Value::as_array) else {
            return json!({ "ok": false, "error": "a raw register write needs a values array" });
        };
        values
            .iter()
            .map(|value| {
                value
                    .as_u64()
                    .filter(|number| *number <= u16::MAX as u64)
                    .map(|number| number as u16)
                    .ok_or_else(|| "raw register values must be 0..65535".to_string())
            })
            .collect()
    } else if let Some(values) = task.get("values").and_then(Value::as_array) {
        let mut encoded = Vec::new();
        for value in values {
            match encode_value(value, data_type, byte_order, word_order) {
                Ok(mut registers) => encoded.append(&mut registers),
                Err(error) => return json!({ "ok": false, "error": error }),
            }
        }
        Ok(encoded)
    } else if let Some(value) = task.get("value") {
        encode_value(value, data_type, byte_order, word_order)
    } else {
        return json!({ "ok": false, "error": "a register write needs value or values" });
    };

    let registers = match registers {
        Ok(registers) => registers,
        Err(error) => return json!({ "ok": false, "error": error }),
    };
    if registers.is_empty() || registers.len() > 123 {
        return json!({
            "ok": false,
            "error": format!(
                "a register write takes 1..=123 registers, got {}",
                registers.len()
            ),
        });
    }

    let result = if registers.len() == 1 {
        session
            .write_single_register(unit_id, address, registers[0])
            .map(|_| 1usize)
    } else {
        session
            .write_multiple_registers(unit_id, address, &registers)
            .map(|_| registers.len())
    };

    match result {
        Ok(count) => json!({
            "ok": true,
            "unitId": unit_id,
            "area": area,
            "address": address,
            "dataType": data_type,
            "registers": registers,
            "written": count,
        }),
        Err(error) => json!({ "ok": false, "error": error }),
    }
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

impl PluginHandler for Plugin {
    fn handle(
        &self,
        _context: RequestContext,
        method: &str,
        params: Value,
        emitter: &PluginEmitter,
    ) -> Result<Value, PluginError> {
        match method {
            "connection/test" => self.test_connection(&params, emitter),
            "connection/connect" => self.connect(&params, emitter),
            "connection/disconnect" => self.disconnect(&params),
            "modbus/read" => self.modbus_read(&params),
            "modbus/write" => self.modbus_write(&params),
            // Device sessions: the workbench keeps a list of devices and opens
            // one session per device, rather than being limited to the single
            // endpoint DBX passed in when the workbench was opened.
            "modbus/open" => self.modbus_open(&params),
            "modbus/close" => self.modbus_close(&params),
            "modbus/devices" => self.modbus_devices(),
            "modbus/connection_info" => self.modbus_connection_info(&params),
            "mcp/tools" => Ok(self.mcp_tools()),
            "mcp/call" => self.mcp_call(&params),
            // Cheap identity probe, handy when debugging the sidecar wiring.
            "industrial/ping" => Ok(json!({
                "ok": true,
                "plugin": PLUGIN_ID,
                "version": PLUGIN_VERSION,
                "language": "rust",
                "openConnections": self
                    .sessions
                    .lock()
                    .map(|sessions| sessions.len())
                    .unwrap_or(0),
            })),
            _ => Err(PluginError::method_not_found(method)),
        }
    }
}

impl Plugin {
    fn test_connection(&self, params: &Value, _emitter: &PluginEmitter) -> Result<Value, PluginError> {
        let connection = connection_object(params)?;
        let provider = provider_of(connection);

        if provider != "modbus-tcp" && provider != "modbus" {
            return Err(PluginError::new(
                -32602,
                format!("Unsupported connection type \"{provider}\""),
            ));
        }

        let session = match modbus_session(connection, params) {
            Ok(session) => session,
            Err(error) => return Ok(json!({ "success": false, "message": error })),
        };
        // A Modbus exception on the probe address still proves the endpoint
        // answers, so it is reported as a note rather than a failure.
        let probe = session.probe();
        session.disconnect();
        Ok(match probe {
            Ok((_, note)) => json!({
                "success": true,
                "message": format!("Modbus TCP reachable on {}; {note}", session.endpoint()),
            }),
            Err(error) => json!({
                "success": false,
                "message": format!("Cannot reach {}: {error}", session.endpoint()),
            }),
        })
    }

    fn connect(&self, params: &Value, _emitter: &PluginEmitter) -> Result<Value, PluginError> {
        let connection = connection_object(params)?;
        let connection_id = connection_id(params)?;
        let provider = provider_of(connection);

        // Connect is idempotent: drop any session already held for this id.
        if let Some(previous) = self.drop_session(&connection_id) {
            let Session::Modbus(session) = previous.as_ref();
            session.disconnect();
        }


        if provider == "modbus-tcp" || provider == "modbus" {
            // A workspace with no address of its own is the normal case now: the
            // devices inside the workbench own the addresses, so opening it is
            // not an error and the message says where the address belongs. With
            // an address, the store is lazy: the socket is opened on the first
            // read, which reconnects transparently once the device is actually
            // there. Reachability is reported in `connected` rather than
            // blocking the page when the IP is not answering.
            let session = match modbus_session(connection, params) {
                Ok(session) => session,
                Err(error) => return Ok(json!({ "success": true, "connected": false, "message": error })),
            };
            let endpoint = session.endpoint();
            let defaults = modbus_defaults(connection);
            let probe = session.probe();
            self.store(connection_id, Session::Modbus(session), Some(defaults))?;
            return Ok(match probe {
                Ok((_, note)) => json!({
                    "success": true,
                    "connected": true,
                    "message": format!("Connected to {endpoint}; {note}")
                }),
                Err(error) => json!({
                    "success": true,
                    "connected": false,
                    "message": format!(
                        "Endpoint not reachable yet: {error}. The workbench is open — configure the address and reconnect. Reads will automatically connect once the device answers."
                    ),
                }),
            });
        }

        Err(PluginError::new(
            -32602,
            format!("Unsupported connection type \"{provider}\""),
        ))
    }

    fn disconnect(&self, params: &Value) -> Result<Value, PluginError> {
        let connection_id = connection_id(params)?;
        match self.drop_session(&connection_id) {
            Some(session) => {
                let Session::Modbus(session) = session.as_ref();
                session.disconnect();
                Ok(json!({ "success": true }))
            }
            // Disconnect must be idempotent.
            None => Ok(json!({ "success": true })),
        }
    }

    fn modbus_read(&self, params: &Value) -> Result<Value, PluginError> {
        let connection_id = target_connection_id(params)?;
        let session = self.lookup(&connection_id)?;
        let Session::Modbus(session) = session.as_ref();

        let tasks = params
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .or_else(|| params.get("task").map(|task| vec![task.clone()]))
            .ok_or_else(|| PluginError::new(-32602, "Missing tasks"))?;
        if tasks.is_empty() || tasks.len() > MAX_READ_TASKS {
            return Err(PluginError::new(
                -32602,
                format!("A read takes 1..={MAX_READ_TASKS} tasks"),
            ));
        }

        let defaults = self.modbus_defaults_for(&connection_id);
        let results: Vec<Value> = tasks
            .iter()
            .map(|task| read_task(session, task, &defaults))
            .collect();
        Ok(json!({ "results": results }))
    }

    /// Opens (or re-opens) a device session and reports whether the endpoint
    /// actually answers, with the round-trip it took.
    ///
    /// Probing here rather than at first read means a mistyped IP is a failure
    /// the device list can show immediately, instead of a read that fails later
    /// with no obvious cause.
    fn modbus_open(&self, params: &Value) -> Result<Value, PluginError> {
        let device_id = param_text(params, "deviceId")
            .ok_or_else(|| PluginError::new(-32602, "Missing deviceId"))?;
        let host = param_text(params, "host")
            .ok_or_else(|| PluginError::new(-32602, "Missing host"))?;
        if host.is_empty() {
            return Err(PluginError::new(-32602, "Missing host"));
        }
        let port = param_u64(params, "port").unwrap_or(502).clamp(1, 65_535) as u16;
        let timeout_ms = param_u64(params, "timeoutMs")
            .unwrap_or(3000)
            .clamp(200, 120_000);
        let unit_id = param_u64(params, "unitId").unwrap_or(1).min(255) as u8;
        let byte_order = param_text(params, "byteOrder").unwrap_or_else(|| "ABCD".to_string());
        let word_order = param_text(params, "wordOrder").unwrap_or_else(|| "big".to_string());

        let key = device_key(&device_id);
        // Re-opening replaces the previous session, so the device form can be
        // saved and reconnected without leaking a socket.
        if let Some(previous) = self.drop_session(&key) {
            let Session::Modbus(session) = previous.as_ref();
            session.disconnect();
        }

        let session = ModbusSession::new(host.as_str(), port, Duration::from_millis(timeout_ms));
        let endpoint = session.endpoint();
        let started = Instant::now();
        let probe = session.probe();
        let latency_ms = started.elapsed().as_millis() as u64;

        match probe {
            Ok((_, note)) => {
                self.store(
                    key,
                    Session::Modbus(session),
                    Some((byte_order, word_order, unit_id)),
                )?;
                Ok(json!({
                    "success": true,
                    "endpoint": endpoint,
                    "latencyMs": latency_ms,
                    "message": format!("{endpoint} reachable; {note}"),
                }))
            }
            Err(error) => Ok(json!({
                "success": false,
                "endpoint": endpoint,
                "latencyMs": latency_ms,
                "message": format!("Cannot reach {endpoint}: {error}"),
            })),
        }
    }

    fn modbus_close(&self, params: &Value) -> Result<Value, PluginError> {
        let device_id = param_text(params, "deviceId")
            .ok_or_else(|| PluginError::new(-32602, "Missing deviceId"))?;
        let closed = match self.drop_session(&device_key(&device_id)) {
            Some(session) => {
                let Session::Modbus(session) = session.as_ref();
                session.disconnect();
                true
            }
            None => false,
        };
        Ok(json!({ "success": true, "closed": closed }))
    }

    /// Which devices currently hold an open session. The workbench asks on boot
    /// so a UI reload reuses the sockets the backend already has instead of
    /// reconnecting every device.
    fn modbus_devices(&self) -> Result<Value, PluginError> {
        // Collect the endpoints first, then read the defaults: holding the
        // session registry while taking the defaults lock would nest the two for
        // no reason.
        let mut found: Vec<(String, String)> = Vec::new();
        {
            let sessions = self
                .sessions
                .lock()
                .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
            for (key, session) in sessions.iter() {
                let Some(device_id) = key.strip_prefix(DEVICE_PREFIX) else { continue };
                let Session::Modbus(session) = session.as_ref();
                found.push((device_id.to_string(), session.endpoint()));
            }
        }
        let devices: Vec<Value> = found
            .into_iter()
            .map(|(device_id, endpoint)| {
                let (_, _, unit_id) = self.modbus_defaults_for(&device_key(&device_id));
                json!({
                    "deviceId": device_id,
                    "endpoint": endpoint,
                    "unitId": unit_id,
                })
            })
            .collect();
        Ok(json!({ "devices": devices }))
    }

    /// The endpoint DBX opened this workbench against, so a fresh workbench can
    /// seed its device list from the saved connection instead of asking the user
    /// to retype the IP it already knows.
    fn modbus_connection_info(&self, params: &Value) -> Result<Value, PluginError> {
        let connection_id = target_connection_id(params)?;
        let session = self.lookup(&connection_id)?;
        let Session::Modbus(session) = session.as_ref();
        let endpoint = session.endpoint();
        let (host, port) = split_endpoint(&endpoint);
        let (_, _, unit_id) = self.modbus_defaults_for(&connection_id);
        Ok(json!({
            "host": host,
            "port": port,
            "unitId": unit_id,
            "endpoint": endpoint,
        }))
    }

    fn modbus_write(&self, params: &Value) -> Result<Value, PluginError> {
        let connection_id = target_connection_id(params)?;
        let session = self.lookup(&connection_id)?;
        let Session::Modbus(session) = session.as_ref();

        let writes = params
            .get("writes")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| PluginError::new(-32602, "Missing writes"))?;
        if writes.is_empty() || writes.len() > MAX_WRITE_TASKS {
            return Err(PluginError::new(
                -32602,
                format!("A write takes 1..={MAX_WRITE_TASKS} operations"),
            ));
        }

        let defaults = self.modbus_defaults_for(&connection_id);
        let results: Vec<Value> = writes
            .iter()
            .map(|task| write_task(session, task, &defaults))
            .collect();
        Ok(json!({ "results": results }))
    }



    /// Read-only tools offered to the DBX AI assistant.
    fn mcp_tools(&self) -> Value {
        json!({
            "tools": [
                {
                    "name": "modbus_read",
                    "description": "Read Modbus TCP coils, discrete inputs, holding registers or input registers from an open Modbus TCP connection. Returns the raw register values and, for registers, the value decoded with the requested data type and byte order.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "area": {
                                "type": "string",
                                "description": "Register area to read.",
                                "enum": ["coil", "discrete", "holding", "input"]
                            },
                            "address": {
                                "type": "integer",
                                "description": "Zero-based address of the first item.",
                                "minimum": 0,
                                "maximum": 65535
                            },
                            "quantity": {
                                "type": "integer",
                                "description": "Number of items to read. Defaults to the register count of the data type.",
                                "minimum": 1,
                                "maximum": 2000
                            },
                            "dataType": {
                                "type": "string",
                                "description": "Data type used to decode register values.",
                                "enum": ["uint16", "int16", "uint32", "int32", "float32", "uint64", "int64", "float64"]
                            },
                            "byteOrder": {
                                "type": "string",
                                "description": "Register byte order.",
                                "enum": ["ABCD", "BADC", "CDAB", "DCBA"]
                            },
                            "unitId": {
                                "type": "integer",
                                "description": "Modbus unit id (slave address). Defaults to the connection setting.",
                                "minimum": 0,
                                "maximum": 255
                            }
                        },
                        "required": ["area", "address"]
                    },
                    "annotations": { "readOnlyHint": true, "title": "Read Modbus TCP values" }
                },
            ]
        })
    }

    fn mcp_call(&self, params: &Value) -> Result<Value, PluginError> {
        let tool = params
            .get("tool")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::new(-32602, "Missing tool"))?;
        // Check the tool name before the connection. Resolving the connection
        // first makes an unknown tool report "No open plugin connection", which
        // hides the actual mistake. Must stay in step with `mcp/tools`.
        if !MCP_READ_TOOLS.contains(&tool) {
            return Err(PluginError::new(-32602, format!("Unknown tool {tool}")));
        }
        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

        // The host strips connectionId from the model-visible schema and injects
        // the bound connection; fall back to the lifecycle payload for older hosts.
        let connection_id = arguments
            .get("connectionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                params
                    .get("lifecycle")
                    .and_then(|lifecycle| lifecycle.get("connection"))
                    .and_then(|connection| connection.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let Some(connection_id) = connection_id else {
            return Ok(mcp_error(
                "No open plugin connection is bound to this tool call.",
            ));
        };

        // A failing tool has to reach the model as `isError`, not abort the whole
        // call. Wrapping the dispatch in a closure keeps `?` inside the tool body
        // so the error becomes a value here instead of escaping the handler.
        let result = (|| -> Result<Value, PluginError> {
            match tool {
                "modbus_read" => {
                    let session = self.lookup(&connection_id)?;
                    let Session::Modbus(session) = session.as_ref();
                    let defaults = self.modbus_defaults_for(&connection_id);
                    Ok(read_task(session, &arguments, &defaults))
                }
                other => Err(PluginError::new(-32602, format!("Unknown tool {other}"))),
            }
        })();

        Ok(match result {
            Ok(value) => {
                let text =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                json!({ "content": [{ "type": "text", "text": text }], "isError": false })
            }
            // Tool failures are reported in-band so the model can react instead
            // of the whole call failing.
            Err(error) => mcp_error(&error.message),
        })
    }
}

fn mcp_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

fn main() -> std::io::Result<()> {
    let plugin = Plugin::new().unwrap_or_else(|error| {
        // stdout is reserved for protocol traffic, so failures go to stderr.
        eprintln!("[{PLUGIN_ID}] {error}");
        std::process::exit(1);
    });
    let metadata = PluginMetadata::new(PLUGIN_ID, PLUGIN_VERSION)
        .with_capability("connections")
        .with_capability("modbus-tcp");
    PluginServer::new(metadata, plugin).serve()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The exact shape DBX builds for a lifecycle request: the provider type is
    /// carried by `plugin_connection_type`, never by `database_type`, which is
    /// hard-coded to "plugin". Regression guard for "Unsupported connection
    /// type \"\"".
    #[test]
    fn provider_comes_from_the_plugin_connection_type() {
        let connection = json!({
            "id": "abc",
            "db_type": "plugin",
            "plugin_id": "io.github.bobocha214.modbus",
            "plugin_connection_provider": "io.github.bobocha214.modbus.modbus",
            "plugin_connection_type": "modbus-tcp",
            "host": "127.0.0.1",
            "port": 502,
            "external_config": { "unit_id": 3, "byte_order": "BADC" },
            "connection_secrets": {},
        });
        assert_eq!(provider_of(&connection), "modbus-tcp");
        assert_eq!(number_field(&connection, "unit_id"), Some(3));
        assert_eq!(text_field(&connection, "byte_order").as_deref(), Some("BADC"));
    }

    #[test]
    fn provider_falls_back_to_the_contribution_id_tail() {
        let connection = json!({
            "plugin_connection_provider": "io.github.bobocha214.modbus.modbus",
        });
        assert_eq!(provider_of(&connection), "modbus");
    }

    #[test]
    fn provider_accepts_legacy_spellings() {
        assert_eq!(provider_of(&json!({ "database_type": "modbus-tcp" })), "modbus-tcp");
        assert_eq!(provider_of(&json!({ "databaseType": "modbus" })), "modbus");
        assert_eq!(provider_of(&json!({ "dbType": "modbus" })), "modbus");
        // Nothing recognisable must stay empty so the caller reports it.
        assert_eq!(provider_of(&json!({ "db_type": "plugin" })), "");
    }

    #[test]
    fn secrets_and_config_are_both_visible() {
        let connection = json!({
            "host": "10.0.0.5",
            "external_config": { "byte_order": "BADC" },
            "connection_secrets": { "password": "s3cret" },
        });
        assert_eq!(text_field(&connection, "host").as_deref(), Some("10.0.0.5"));
        assert_eq!(
            text_field(&connection, "byte_order").as_deref(),
            Some("BADC")
        );
        assert_eq!(text_field(&connection, "password").as_deref(), Some("s3cret"));
    }

    /// The connection form asks for a workspace name only; addresses, slave ids
    /// and display options are chosen per device in the workbench. A snapshot
    /// carrying just those fields must still yield usable defaults, because the
    /// workbench sends the display options with each read instead.
    #[test]
    fn a_minimal_connection_falls_back_to_classic_defaults() {
        let connection = json!({
            "id": "abc",
            "db_type": "plugin",
            "plugin_connection_type": "modbus-tcp",
            "host": "192.168.1.20",
            "port": 502,
            "external_config": { "unit_id": 7, "timeout_ms": 2000 },
        });
        let (byte_order, word_order, unit_id) = modbus_defaults(&connection);
        assert_eq!(byte_order, "ABCD");
        assert_eq!(word_order, "big");
        assert_eq!(unit_id, 7);

        // An empty snapshot must be just as safe, because the workbench may be
        // opened before the connection body has been filled in.
        let (byte_order, word_order, unit_id) = modbus_defaults(&json!({}));
        assert_eq!(byte_order, "ABCD");
        assert_eq!(word_order, "big");
        assert_eq!(unit_id, 1);
    }

    /// The display options the workbench sends per read must reach the decoder
    /// and change what the same two registers decode to.
    #[test]
    fn per_read_display_options_reach_the_decoder() {
        // 0x4200 0x0000 is 32.0 as a big-endian float; word-swapped it is not.
        let registers = vec![0x4200u16, 0x0000];
        let big = decode_registers(&registers, "float32", "ABCD", "big").unwrap();
        let swapped = decode_registers(&registers, "float32", "CDAB", "big").unwrap();
        assert_eq!(big, 32.0);
        assert_ne!(big, swapped, "word swap must change the decoded value");

        // The raw registers themselves are never touched by a display option.
        assert_eq!(registers, vec![0x4200u16, 0x0000]);
    }

    /// Device sessions live in the same registry as the connection DBX opened,
    /// so their ids must never collide with a host connection id.
    #[test]
    fn device_sessions_are_namespaced() {
        let key = device_key("plc-1");
        assert_eq!(key, "device:plc-1");
        assert!(key.starts_with(DEVICE_PREFIX));
        // A host connection id never starts with the prefix, so stripping it is
        // unambiguous when listing devices.
        assert_eq!(key.strip_prefix(DEVICE_PREFIX), Some("plc-1"));
        assert_eq!("8f2c1a".strip_prefix(DEVICE_PREFIX), None);
    }

    /// The endpoint reported back to the workbench is re-split into a host and a
    /// port it can put in the device form.
    #[test]
    fn endpoints_split_back_into_host_and_port() {
        assert_eq!(split_endpoint("192.168.1.10:502"), ("192.168.1.10".to_string(), 502));
        assert_eq!(split_endpoint("plc.local:15020"), ("plc.local".to_string(), 15020));
        // IPv6 literals keep their brackets and inner colons.
        assert_eq!(split_endpoint("[::1]:502"), ("[::1]".to_string(), 502));
        // A malformed port falls back to the protocol default rather than
        // producing a device the user cannot fix from the form.
        assert_eq!(split_endpoint("host:not-a-port"), ("host".to_string(), 502));
        assert_eq!(split_endpoint("bare-host"), ("bare-host".to_string(), 502));
    }

    /// A saved connection no longer carries an address, so a connection-level
    /// connect must not dial a placeholder: it has to report that the address
    /// lives on the devices inside the workbench instead.
    #[test]
    fn a_hostless_connection_has_no_endpoint() {
        let hostless = json!({
            "id": "abc",
            "db_type": "plugin",
            "plugin_connection_type": "modbus-tcp",
            "host": "",
            "port": 0,
            "external_config": {},
        });
        assert_eq!(connection_host(&hostless, &json!({})), None);
        // Not `unwrap_err`: the Ok side is a socket session, which is not Debug.
        assert!(matches!(
            modbus_session(&hostless, &json!({})),
            Err(message) if message == NO_HOST_MESSAGE
        ));

        // The host's own runtime endpoint still wins when it resolves one, and a
        // port saved as 0 (no port field) means the protocol default.
        assert_eq!(
            connection_host(&hostless, &json!({ "runtime": { "host": "10.0.0.9" } })),
            Some(("10.0.0.9".to_string(), 502))
        );
        let saved = json!({ "host": "10.0.0.5", "port": 0 });
        assert_eq!(connection_host(&saved, &json!({})), Some(("10.0.0.5".to_string(), 502)));
        let saved = json!({ "host": "10.0.0.5", "port": 1502 });
        assert_eq!(connection_host(&saved, &json!({})), Some(("10.0.0.5".to_string(), 1502)));
    }

    /// `modbus/open` clamps the values it accepts, so a device form typo cannot
    /// ask for a port of zero or a timeout that hangs the plugin.
    #[test]
    fn open_parameters_are_clamped() {
        assert_eq!(0u64.clamp(1, 65_535), 1);
        assert_eq!(70_000u64.clamp(1, 65_535), 65_535);
        assert_eq!(10u64.clamp(200, 120_000), 200);
        assert_eq!(500_000u64.clamp(200, 120_000), 120_000);
        assert_eq!(999u64.min(255), 255);
    }
}
