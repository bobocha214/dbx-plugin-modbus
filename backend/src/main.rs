//! DBX sidecar for industrial field protocols.
//!
//! Two connection providers share one native backend:
//!
//! * `modbus-tcp` — a hand-written Modbus TCP client ([`modbus`]).
//! * `opcua` — an OPC UA client on top of `async-opcua` ([`opcua_client`]).
//!
//! DBX calls `PluginHandler::handle` on plain worker threads, so blocking
//! sockets are safe here; the only async surface is OPC UA, which is driven
//! through a shared runtime created once at start-up.

mod modbus;
mod opcua_client;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbx_plugin_sdk::{
    PluginEmitter, PluginError, PluginHandler, PluginMetadata, PluginServer, RequestContext,
};
use serde_json::{json, Value};

use modbus::{area_function_codes, decode_registers, encode_value, register_count, ModbusSession};
use opcua_client::{OpcuaConfig, OpcuaSession};

const PLUGIN_ID: &str = "com.dbx.industrial-protocols";
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Maximum number of read tasks accepted in a single `modbus/read` call.
const MAX_READ_TASKS: usize = 64;
/// Maximum number of write operations accepted in a single `modbus/write` call.
const MAX_WRITE_TASKS: usize = 32;

/// Tool names advertised through `mcp/tools`. Every one of them is read-only, so
/// DBX runs them without a per-call confirmation prompt.
const MCP_READ_TOOLS: [&str; 3] = ["modbus_read", "opcua_read", "opcua_browse"];

/// Per-connection Modbus defaults taken from the connection form.
type ModbusDefaults = (String, String, u8);

enum Session {
    Modbus(ModbusSession),
    Opcua(OpcuaSession),
}

struct Plugin {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Byte order, word order and unit id captured when the connection opened.
    defaults: Mutex<HashMap<String, ModbusDefaults>>,
    runtime: tokio::runtime::Runtime,
    data_dir: PathBuf,
}

impl Plugin {
    fn new() -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("dbx-industrial-opcua")
            .build()
            .map_err(|error| format!("cannot start the OPC UA runtime: {error}"))?;
        Ok(Self {
            sessions: Mutex::new(HashMap::new()),
            defaults: Mutex::new(HashMap::new()),
            runtime,
            data_dir: plugin_data_dir(),
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

fn plugin_data_dir() -> PathBuf {
    // DBX exports this for every sidecar run: plugin-data/<plugin-id>.
    match std::env::var("DBX_PLUGIN_DATA_DIR") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => std::env::temp_dir().join(PLUGIN_ID),
    }
}

fn ensure_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))
}

// ---------------------------------------------------------------------------
// Connection payload helpers
//
// DBX sends the saved connection as a JSON snapshot. Fields bound to standard
// bindings land on the top level (host/port/username/...); provider fields bound
// to `config` are nested under `external_config`. Read both so the plugin keeps
// working if the host flattens one of them.
// ---------------------------------------------------------------------------

fn field<'a>(connection: &'a Value, key: &str) -> Option<&'a Value> {
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

fn text_field(connection: &Value, key: &str) -> Option<String> {
    field(connection, key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn number_field(connection: &Value, key: &str) -> Option<u64> {
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

fn modbus_session(connection: &Value, params: &Value) -> Result<ModbusSession, String> {
    let (runtime_host, runtime_port) = runtime_endpoint(params);
    let host = runtime_host
        .or_else(|| text_field(connection, "host"))
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = runtime_port
        .or_else(|| number_field(connection, "port").map(|value| value as u16))
        .unwrap_or(502);
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

fn opcua_config(
    connection: &Value,
    params: &Value,
    data_dir: &Path,
) -> Result<OpcuaConfig, String> {
    let (runtime_host, runtime_port) = runtime_endpoint(params);
    // Client certificates and the trust store live in the plugin data
    // directory, which DBX keeps across upgrades.
    let pki_dir = data_dir.join("pki");
    ensure_dir(&pki_dir)?;
    OpcuaConfig::from_connection(connection, runtime_host.as_deref(), runtime_port, pki_dir)
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
            let quantity = match task.get("quantity").and_then(Value::as_u64) {
                Some(value) => value.clamp(1, 125) as u16,
                None => register_count(data_type).unwrap_or(1).clamp(1, 125) as u16,
            };
            match session.read_registers(unit_id, read_code, address, quantity) {
                Ok(registers) => {
                    match decode_registers(&registers, data_type, byte_order, word_order) {
                        Ok(value) => json!({
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
                        }),
                        Err(error) => json!({ "ok": false, "error": error }),
                    }
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
            "connection/test" => self.test_connection(&params),
            "connection/connect" => self.connect(&params),
            "connection/disconnect" => self.disconnect(&params),
            "modbus/read" => self.modbus_read(&params),
            "modbus/write" => self.modbus_write(&params),
            "opcua/browse" => self.opcua_browse(&params),
            "opcua/read" => self.opcua_read(&params),
            "opcua/write" => self.opcua_write(&params),
            "opcua/subscribe" => self.opcua_subscribe(&params, emitter),
            "opcua/unsubscribe" => self.opcua_unsubscribe(&params),
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
    fn test_connection(&self, params: &Value) -> Result<Value, PluginError> {
        let connection = connection_object(params)?;
        let provider = provider_of(connection);

        if provider == "opcua" {
            // Connection lifecycle calls report failures in-band so the host can
            // show them in the connection form; a JSON-RPC error would surface as
            // a plugin fault instead of "these credentials were rejected".
            return Ok(
                match opcua_config(connection, params, &self.data_dir)
                    .and_then(|config| OpcuaSession::connect(&self.runtime, &config))
                {
                    Ok(session) => {
                        let endpoint = session.endpoint_url().to_string();
                        session.disconnect(&self.runtime);
                        json!({
                            "success": true,
                            "message": format!("OPC UA session activated on {endpoint}"),
                        })
                    }
                    Err(error) => json!({ "success": false, "message": error }),
                },
            );
        }
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

    fn connect(&self, params: &Value) -> Result<Value, PluginError> {
        let connection = connection_object(params)?;
        let connection_id = connection_id(params)?;
        let provider = provider_of(connection);

        // Connect is idempotent: drop any session already held for this id.
        if let Some(previous) = self.drop_session(&connection_id) {
            match previous.as_ref() {
                Session::Modbus(session) => session.disconnect(),
                Session::Opcua(session) => session.disconnect(&self.runtime),
            }
        }

        if provider == "opcua" {
            // Same in-band contract as `connection/test`: an unreachable endpoint
            // or a rejected identity token is a result the connection form shows,
            // not a plugin fault.
            let outcome = opcua_config(connection, params, &self.data_dir)
                .and_then(|config| OpcuaSession::connect(&self.runtime, &config));
            return match outcome {
                Ok(session) => {
                    let endpoint = session.endpoint_url().to_string();
                    self.store(connection_id, Session::Opcua(session), None)?;
                    Ok(json!({
                        "success": true,
                        "message": format!("Connected to {endpoint}"),
                    }))
                }
                Err(error) => Ok(json!({ "success": false, "message": error })),
            };
        }

        if provider == "modbus-tcp" || provider == "modbus" {
            let session = match modbus_session(connection, params) {
                Ok(session) => session,
                Err(error) => return Ok(json!({ "success": false, "message": error })),
            };
            let endpoint = session.endpoint();
            let defaults = modbus_defaults(connection);
            // Probe once so a bad address surfaces during connect rather than
            // as a surprise on the first poll.
            let probe = session.probe();
            self.store(connection_id, Session::Modbus(session), Some(defaults))?;
            return Ok(match probe {
                Ok((_, note)) => {
                    json!({ "success": true, "message": format!("Connected to {endpoint}; {note}") })
                }
                Err(error) => json!({
                    "success": false,
                    "message": format!("Cannot reach {endpoint}: {error}"),
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
                match session.as_ref() {
                    Session::Modbus(session) => session.disconnect(),
                    Session::Opcua(session) => session.disconnect(&self.runtime),
                }
                Ok(json!({ "success": true }))
            }
            // Disconnect must be idempotent.
            None => Ok(json!({ "success": true })),
        }
    }

    fn modbus_read(&self, params: &Value) -> Result<Value, PluginError> {
        let connection_id = target_connection_id(params)?;
        let session = self.lookup(&connection_id)?;
        let Session::Modbus(session) = session.as_ref() else {
            return Err(PluginError::new(
                -32602,
                format!("Connection {connection_id} is not a Modbus TCP connection"),
            ));
        };

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

    fn modbus_write(&self, params: &Value) -> Result<Value, PluginError> {
        let connection_id = target_connection_id(params)?;
        let session = self.lookup(&connection_id)?;
        let Session::Modbus(session) = session.as_ref() else {
            return Err(PluginError::new(
                -32602,
                format!("Connection {connection_id} is not a Modbus TCP connection"),
            ));
        };

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

    fn opcua_session(&self, params: &Value) -> Result<(String, Arc<Session>), PluginError> {
        let connection_id = target_connection_id(params)?;
        let session = self.lookup(&connection_id)?;
        if !matches!(session.as_ref(), Session::Opcua(_)) {
            return Err(PluginError::new(
                -32602,
                format!("Connection {connection_id} is not an OPC UA connection"),
            ));
        }
        Ok((connection_id, session))
    }

    fn opcua_browse(&self, params: &Value) -> Result<Value, PluginError> {
        let (_, session) = self.opcua_session(params)?;
        let Session::Opcua(session) = session.as_ref() else {
            unreachable!("opcua_session already checked the variant")
        };
        let node_id = params.get("nodeId").and_then(Value::as_str).unwrap_or("i=84");
        let direction = params
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or("forward");
        let max_references = params
            .get("maxReferences")
            .and_then(Value::as_u64)
            .unwrap_or(500) as u32;
        // Omit `referenceTypeId` for a hierarchy-only browse; pass `i=0` to see
        // every reference, including HasTypeDefinition and HasSubtype.
        let reference_type_id = params.get("referenceTypeId").and_then(Value::as_str);
        let include_subtypes = params
            .get("includeSubtypes")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        session
            .browse(
                &self.runtime,
                node_id,
                direction,
                reference_type_id,
                include_subtypes,
                max_references,
            )
            .map_err(to_error)
    }

    fn opcua_read(&self, params: &Value) -> Result<Value, PluginError> {
        let (_, session) = self.opcua_session(params)?;
        let Session::Opcua(session) = session.as_ref() else {
            unreachable!("opcua_session already checked the variant")
        };
        let node_ids = string_list(params, "nodeIds")?;
        session.read(&self.runtime, &node_ids).map_err(to_error)
    }

    fn opcua_write(&self, params: &Value) -> Result<Value, PluginError> {
        let (_, session) = self.opcua_session(params)?;
        let Session::Opcua(session) = session.as_ref() else {
            unreachable!("opcua_session already checked the variant")
        };
        let writes = params
            .get("writes")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| PluginError::new(-32602, "Missing writes"))?;
        if writes.is_empty() || writes.len() > 200 {
            return Err(PluginError::new(-32602, "A write takes 1..=200 entries"));
        }
        session.write(&self.runtime, &writes).map_err(to_error)
    }

    fn opcua_subscribe(
        &self,
        params: &Value,
        emitter: &PluginEmitter,
    ) -> Result<Value, PluginError> {
        let (connection_id, session) = self.opcua_session(params)?;
        let Session::Opcua(session) = session.as_ref() else {
            unreachable!("opcua_session already checked the variant")
        };
        let node_ids = string_list(params, "nodeIds")?;
        let interval_ms = params
            .get("intervalMs")
            .and_then(Value::as_u64)
            .unwrap_or(1000);
        session
            .subscribe(&self.runtime, &connection_id, &node_ids, interval_ms, emitter)
            .map_err(to_error)
    }

    fn opcua_unsubscribe(&self, params: &Value) -> Result<Value, PluginError> {
        let (_, session) = self.opcua_session(params)?;
        let Session::Opcua(session) = session.as_ref() else {
            unreachable!("opcua_session already checked the variant")
        };
        let subscription_id = params
            .get("subscriptionId")
            .and_then(Value::as_u64)
            .ok_or_else(|| PluginError::new(-32602, "Missing subscriptionId"))?
            as u32;
        session
            .unsubscribe(&self.runtime, subscription_id)
            .map_err(to_error)
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
                {
                    "name": "opcua_read",
                    "description": "Read one or more OPC UA nodes from an open OPC UA connection and return their values, data types and status codes.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "nodeIds": {
                                "type": "array",
                                "description": "OPC UA node ids such as ns=3;s=Temperature.",
                                "items": { "type": "string" },
                                "maxItems": 200
                            }
                        },
                        "required": ["nodeIds"]
                    },
                    "annotations": { "readOnlyHint": true, "title": "Read OPC UA nodes" }
                },
                {
                    "name": "opcua_browse",
                    "description": "Browse the children of an OPC UA node to discover variables and objects. Defaults to the Objects folder.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "nodeId": {
                                "type": "string",
                                "description": "Node id to browse. Defaults to i=85, the Objects folder."
                            },
                            "maxReferences": {
                                "type": "integer",
                                "description": "Maximum number of references to return.",
                                "minimum": 1,
                                "maximum": 2000
                            }
                        },
                        "required": []
                    },
                    "annotations": { "readOnlyHint": true, "title": "Browse the OPC UA address space" }
                }
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
        let scope = json!({ "connectionId": connection_id });

        // A failing tool has to reach the model as `isError`, not abort the whole
        // call. Wrapping the dispatch in a closure keeps `?` inside the tool body
        // so the error becomes a value here instead of escaping the handler.
        let result = (|| -> Result<Value, PluginError> {
            match tool {
                "modbus_read" => {
                    let session = self.lookup(&connection_id)?;
                    let Session::Modbus(session) = session.as_ref() else {
                        return Err(PluginError::new(
                            -32602,
                            "The bound connection is not a Modbus TCP connection.",
                        ));
                    };
                    let defaults = self.modbus_defaults_for(&connection_id);
                    Ok(read_task(session, &arguments, &defaults))
                }
                "opcua_read" => {
                    let (_, session) = self.opcua_session(&scope)?;
                    let Session::Opcua(session) = session.as_ref() else {
                        unreachable!("opcua_session already checked the variant")
                    };
                    let node_ids = string_list(&arguments, "nodeIds")?;
                    session.read(&self.runtime, &node_ids).map_err(to_error)
                }
                "opcua_browse" => {
                    let (_, session) = self.opcua_session(&scope)?;
                    let Session::Opcua(session) = session.as_ref() else {
                        unreachable!("opcua_session already checked the variant")
                    };
                    let node_id = arguments
                        .get("nodeId")
                        .and_then(Value::as_str)
                        .unwrap_or("i=85");
                    let max_references = arguments
                        .get("maxReferences")
                        .and_then(Value::as_u64)
                        .unwrap_or(500) as u32;
                    session
                        .browse(&self.runtime, node_id, "forward", None, true, max_references)
                        .map_err(to_error)
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

fn string_list(params: &Value, key: &str) -> Result<Vec<String>, PluginError> {
    let values = params
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::new(-32602, format!("Missing {key}")))?;
    if values.is_empty() || values.len() > 500 {
        return Err(PluginError::new(
            -32602,
            format!("{key} takes 1..=500 entries"),
        ));
    }
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| PluginError::new(-32602, format!("{key} must contain strings")))
        })
        .collect()
}

fn to_error(message: String) -> PluginError {
    PluginError::new(-32000, message)
}

fn main() -> std::io::Result<()> {
    let plugin = Plugin::new().unwrap_or_else(|error| {
        // stdout is reserved for protocol traffic, so failures go to stderr.
        eprintln!("[{PLUGIN_ID}] {error}");
        std::process::exit(1);
    });
    let metadata = PluginMetadata::new(PLUGIN_ID, PLUGIN_VERSION)
        .with_capability("connections")
        .with_capability("modbus-tcp")
        .with_capability("opcua");
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
            "plugin_id": "com.dbx.industrial-protocols",
            "plugin_connection_provider": "com.dbx.industrial-protocols.modbus",
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
            "plugin_connection_provider": "com.dbx.industrial-protocols.opcua",
        });
        assert_eq!(provider_of(&connection), "opcua");
    }

    #[test]
    fn provider_accepts_legacy_spellings() {
        assert_eq!(provider_of(&json!({ "database_type": "opcua" })), "opcua");
        assert_eq!(provider_of(&json!({ "databaseType": "modbus-tcp" })), "modbus-tcp");
        assert_eq!(provider_of(&json!({ "dbType": "modbus" })), "modbus");
        // Nothing recognisable must stay empty so the caller reports it.
        assert_eq!(provider_of(&json!({ "db_type": "plugin" })), "");
    }

    #[test]
    fn secrets_and_config_are_both_visible() {
        let connection = json!({
            "host": "10.0.0.5",
            "external_config": { "security_policy": "Basic256Sha256" },
            "connection_secrets": { "password": "s3cret" },
        });
        assert_eq!(text_field(&connection, "host").as_deref(), Some("10.0.0.5"));
        assert_eq!(
            text_field(&connection, "security_policy").as_deref(),
            Some("Basic256Sha256")
        );
        assert_eq!(text_field(&connection, "password").as_deref(), Some("s3cret"));
    }
}
