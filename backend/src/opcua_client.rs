//! OPC UA client built on `async-opcua`.
//!
//! The crate is published as `async-opcua` but its library name is `opcua`. It is
//! compiled with the pure-Rust `rsa`/`sha2` backend, so no OpenSSL is needed to
//! cross-compile the sidecar.
//!
//! Every blocking `PluginHandler::handle` call drives this module through a
//! shared tokio runtime: DBX dispatches requests on plain worker threads, so
//! `Runtime::block_on` here never re-enters the runtime from one of its own
//! threads.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbx_plugin_sdk::PluginEmitter;
use opcua::client::{ClientBuilder, DataChangeCallback, IdentityToken, MonitoredItem, Session};
use opcua::crypto::SecurityPolicy;
use opcua::types::{
    Array, AttributeId, BrowseDescription, BrowseDirection, DataValue, EndpointDescription,
    ExpandedNodeId, MessageSecurityMode, MonitoredItemCreateRequest, MonitoringParameters, NodeId,
    NumericRange, QualifiedName, ReadValueId, ReferenceTypeId, StatusCode, TimestampsToReturn,
    UserTokenPolicy, UserTokenType, Variant, VariantScalarTypeId, WriteValue,
};
use serde_json::{json, Value};
use tokio::runtime::Runtime;

/// Resolved connection parameters for one OPC UA session.
#[derive(Debug, Clone)]
pub struct OpcuaConfig {
    pub endpoint_url: String,
    pub security_policy: String,
    pub security_mode: MessageSecurityMode,
    pub username: Option<String>,
    pub password: Option<String>,
    pub timeout: Duration,
    pub pki_dir: PathBuf,
}

impl OpcuaConfig {
    /// Builds the endpoint URL from the connection form.
    ///
    /// `host` / `port` may come from DBX's standard bindings (and may already
    /// have been rewritten by an SSH tunnel or proxy), so they are preferred
    /// over anything typed into the endpoint field.
    pub fn from_connection(
        connection: &Value,
        runtime_host: Option<&str>,
        runtime_port: Option<u16>,
        pki_dir: PathBuf,
    ) -> Result<Self, String> {
        let host = runtime_host
            .map(str::to_string)
            .or_else(|| string_field(connection, "host"))
            .unwrap_or_else(|| "localhost".to_string());
        let port = runtime_port
            .or_else(|| number_field(connection, "port").map(|value| value as u16))
            .unwrap_or(4840);
        let path = string_field(connection, "endpoint_path").unwrap_or_else(|| "/".to_string());
        let path = if path.starts_with('/') { path } else { format!("/{path}") };

        let security_policy = string_field(connection, "security_policy")
            .unwrap_or_else(|| "None".to_string());
        let requested_mode =
            string_field(connection, "security_mode").unwrap_or_else(|| "None".to_string());
        // OPC UA forbids transport security under the None policy. The form hides
        // the mode field in that case, so coerce rather than reject the saved
        // value; the opposite combination is a silent downgrade and is rejected.
        let security_mode = match (security_policy.as_str(), requested_mode.as_str()) {
            ("None", _) => MessageSecurityMode::None,
            (policy, "None") => {
                return Err(format!(
                    "security policy {policy} requires the message security mode \
                     Sign or SignAndEncrypt"
                ))
            }
            (_, mode) => parse_security_mode(mode)?,
        };

        let timeout_ms = number_field(connection, "timeout_ms")
            .unwrap_or(10_000)
            .clamp(500, 120_000);

        Ok(Self {
            endpoint_url: format!("opc.tcp://{host}:{port}{path}"),
            security_policy,
            security_mode,
            username: string_field(connection, "username").filter(|value| !value.is_empty()),
            password: string_field(connection, "password"),
            timeout: Duration::from_millis(timeout_ms),
            pki_dir,
        })
    }
}

/// Reads a provider field from whichever bucket the host used.
///
/// DBX splits a saved connection across the top level (`host`, `port`,
/// `username`, ...), `external_config` (fields bound to `config`) and
/// `connection_secrets` (fields bound to `secret`, i.e. passwords). Reading only
/// the top level silently downgrades security, so all three are consulted.
fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .or_else(|| value.get("external_config").and_then(|config| config.get(key)))
        .or_else(|| {
            value
                .get("connection_secrets")
                .and_then(|secrets| secrets.get(key))
        })
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Numeric sibling of [`string_field`]; hosts are free to send numbers as JSON
/// numbers or as strings from a text input.
fn number_field(value: &Value, key: &str) -> Option<u64> {
    value
        .get(key)
        .or_else(|| value.get("external_config").and_then(|config| config.get(key)))
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
                .or_else(|| value.as_f64().map(|number| number as u64))
        })
}

fn parse_security_mode(value: &str) -> Result<MessageSecurityMode, String> {
    match value {
        "None" => Ok(MessageSecurityMode::None),
        "Sign" => Ok(MessageSecurityMode::Sign),
        "SignAndEncrypt" => Ok(MessageSecurityMode::SignAndEncrypt),
        other => Err(format!(
            "unknown message security mode \"{other}\"; use None, Sign or SignAndEncrypt"
        )),
    }
}

fn resolve_security_policy(value: &str) -> Result<SecurityPolicy, String> {
    let policy = SecurityPolicy::from_str(value).map_err(|_| {
        format!("unknown security policy \"{value}\"")
    })?;
    if !policy.is_supported() {
        return Err(format!("security policy \"{value}\" is not supported"));
    }
    Ok(policy)
}

/// How long a graceful session close is allowed to take before the event loop is
/// torn down anyway. A wedged or already-finished event loop never answers the
/// CloseSession request, and the sidecar must not hang on that.
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Closes a session, waiting at most [`DISCONNECT_TIMEOUT`].
///
/// `Session::disconnect` round-trips a CloseSession request through the event
/// loop and then waits for the loop to observe the disconnection. Neither step
/// completes once the loop has stopped, so the wait must be bounded.
fn close_session(runtime: &Runtime, session: &Session) {
    let _ = runtime
        .block_on(async { tokio::time::timeout(DISCONNECT_TIMEOUT, session.disconnect()).await });
}

/// A live OPC UA session plus the event loop that keeps it connected.
pub struct OpcuaSession {
    session: Arc<Session>,
    event_loop: Mutex<Option<tokio::task::JoinHandle<StatusCode>>>,
    subscriptions: Mutex<Vec<u32>>,
    endpoint_url: String,
}

impl OpcuaSession {
    pub fn endpoint_url(&self) -> &str {
        &self.endpoint_url
    }

    /// Opens a secure channel and activates a session.
    pub fn connect(runtime: &Runtime, config: &OpcuaConfig) -> Result<Self, String> {
        let policy = resolve_security_policy(&config.security_policy)?;

        let identity = match (&config.username, &config.password) {
            (Some(user), password) => {
                IdentityToken::new_user_name(user.clone(), password.clone().unwrap_or_default())
            }
            (None, _) => IdentityToken::Anonymous,
        };

        let user_token_policy = match &config.username {
            Some(_) => UserTokenPolicy {
                policy_id: opcua::types::UAString::from("username"),
                token_type: UserTokenType::UserName,
                issued_token_type: opcua::types::UAString::null(),
                issuer_endpoint_url: opcua::types::UAString::null(),
                security_policy_uri: opcua::types::UAString::null(),
            },
            None => UserTokenPolicy::anonymous(),
        };

        let mut client = ClientBuilder::new()
            .application_name("DBX Industrial Protocols")
            .application_uri("urn:dbx:plugin:industrial-protocols")
            .product_uri("urn:dbx:plugin:industrial-protocols")
            .create_sample_keypair(true)
            // Field devices ship self-signed certificates that the operator
            // cannot pre-install, so trust-on-first-use is the usable default.
            .trust_server_certs(true)
            .session_retry_limit(2)
            .session_retry_initial(Duration::from_secs(1))
            .session_retry_max(Duration::from_secs(10))
            .request_timeout(config.timeout)
            .pki_dir(config.pki_dir.clone())
            .client()
            .map_err(|errors| format!("invalid OPC UA client configuration: {}", errors.join("; ")))?;

        let endpoint: EndpointDescription = (
            config.endpoint_url.as_str(),
            policy.to_uri(),
            config.security_mode,
            user_token_policy,
        )
            .into();

        let (session, mut handle) = runtime
            .block_on(async {
                // Skip discovery first: some PLCs answer GetEndpoints only after
                // a full handshake, and a direct connect keeps the surface small.
                let (session, event_loop) =
                    match client.connect_to_endpoint_directly(endpoint.clone(), identity.clone()) {
                        Ok(pair) => pair,
                        Err(direct_error) => {
                            let mut retry_client = ClientBuilder::new()
                                .application_name("DBX Industrial Protocols")
                                .application_uri("urn:dbx:plugin:industrial-protocols")
                                .product_uri("urn:dbx:plugin:industrial-protocols")
                                .create_sample_keypair(true)
                                .trust_server_certs(true)
                                .session_retry_limit(2)
                                .request_timeout(config.timeout)
                                .pki_dir(config.pki_dir.clone())
                                .client()
                                .map_err(|errors| errors.join("; "))?;
                            retry_client
                                .connect_to_matching_endpoint(endpoint.clone(), identity.clone())
                                .await
                                .map_err(|error| {
                                    format!(
                                        "direct connect failed ({direct_error}); \
                                         endpoint discovery also failed ({error})"
                                    )
                                })?
                        }
                    };
                // `EventLoop::spawn` is a thin wrapper over `tokio::task::spawn`,
                // so it has to run with the reactor on the stack. Hoisting it out
                // of `block_on` panics with "there is no reactor running".
                let handle = event_loop.spawn();
                Ok::<_, String>((session, handle))
            })?;

        // `wait_for_connection` documents that it never returns once the event
        // loop has ended, and a rejected identity token ends the loop before the
        // session ever becomes active. Racing the loop against the wait is what
        // turns "did not become active" into the real reason, such as
        // BadUserAccessDenied, instead of a bare timeout.
        let outcome = runtime.block_on(async {
            tokio::select! {
                ended = &mut handle => Err(match ended {
                    Ok(status) => format!("OPC UA session on {} ended: {status}", config.endpoint_url),
                    Err(error) => format!("OPC UA session task stopped: {error}"),
                }),
                active = tokio::time::timeout(config.timeout, session.wait_for_connection()) => {
                    match active {
                        Ok(true) => Ok(()),
                        Ok(false) => Err(format!(
                            "OPC UA session on {} closed before it became active",
                            config.endpoint_url
                        )),
                        Err(_) => Err(format!(
                            "OPC UA session on {} did not become active within {:?}",
                            config.endpoint_url, config.timeout
                        )),
                    }
                }
            }
        });
        if let Err(error) = outcome {
            // The loop is already gone, so a graceful close would wait for a
            // reply that never comes.
            close_session(runtime, &session);
            handle.abort();
            return Err(error);
        }

        Ok(Self {
            session,
            event_loop: Mutex::new(Some(handle)),
            subscriptions: Mutex::new(Vec::new()),
            endpoint_url: config.endpoint_url.clone(),
        })
    }

    /// Reads the list of endpoints the server advertises.
    pub fn read(&self, runtime: &Runtime, node_ids: &[String]) -> Result<Value, String> {
        if node_ids.is_empty() {
            return Err("at least one node id is required".to_string());
        }
        let nodes: Vec<NodeId> = node_ids
            .iter()
            .map(|value| parse_node_id(value))
            .collect::<Result<_, _>>()?;
        let reads: Vec<ReadValueId> = nodes.into_iter().map(ReadValueId::from).collect();

        let values = runtime
            .block_on(self.session.read(&reads, TimestampsToReturn::Both, 0.0))
            .map_err(|error| format!("OPC UA read failed: {error}"))?;

        let results: Vec<Value> = node_ids
            .iter()
            .zip(values.iter())
            .map(|(node_id, data_value)| data_value_to_json(node_id, data_value))
            .collect();
        Ok(json!({ "results": results }))
    }

    pub fn write(&self, runtime: &Runtime, writes: &[Value]) -> Result<Value, String> {
        if writes.is_empty() {
            return Err("at least one write is required".to_string());
        }
        let mut payload = Vec::with_capacity(writes.len());
        for entry in writes {
            let node_id = parse_node_id(
                entry
                    .get("nodeId")
                    .and_then(Value::as_str)
                    .ok_or("a write entry needs a nodeId")?,
            )?;
            let data_type = entry
                .get("dataType")
                .and_then(Value::as_str)
                .unwrap_or("Auto");
            let value = entry.get("value").ok_or("a write entry needs a value")?;
            let variant = json_to_variant(value, data_type)?;
            payload.push(WriteValue {
                node_id,
                attribute_id: AttributeId::Value as u32,
                index_range: NumericRange::None,
                value: DataValue::new_now(variant),
            });
        }

        let statuses = runtime
            .block_on(self.session.write(&payload))
            .map_err(|error| format!("OPC UA write failed: {error}"))?;

        let results: Vec<Value> = writes
            .iter()
            .zip(statuses.iter())
            .map(|(entry, status)| {
                json!({
                    "nodeId": entry.get("nodeId").cloned().unwrap_or(Value::Null),
                    "ok": status.is_good(),
                    "statusCode": status.bits(),
                    "status": status.to_string(),
                })
            })
            .collect();
        Ok(json!({ "results": results }))
    }

    /// Browses one node.
    ///
    /// `reference_type_id` defaults to `HierarchicalReferences` because an
    /// unfiltered browse also returns `HasTypeDefinition`, which puts the node's
    /// type (for example `FolderType`) into what should be a list of children.
    pub fn browse(
        &self,
        runtime: &Runtime,
        node_id: &str,
        direction: &str,
        reference_type_id: Option<&str>,
        include_subtypes: bool,
        max_references: u32,
    ) -> Result<Value, String> {
        let root = parse_node_id(node_id)?;
        let browse_direction = match direction {
            "forward" | "Forward" => BrowseDirection::Forward,
            "inverse" | "Inverse" => BrowseDirection::Inverse,
            "both" | "Both" => BrowseDirection::Both,
            other => return Err(format!("unknown browse direction \"{other}\"")),
        };
        let reference_type = match reference_type_id {
            Some(value) if !value.trim().is_empty() => parse_node_id(value)?,
            _ => ReferenceTypeId::HierarchicalReferences.into(),
        };
        let description = BrowseDescription {
            node_id: root.clone(),
            browse_direction,
            reference_type_id: reference_type,
            include_subtypes,
            node_class_mask: 0,
            result_mask: 0,
        };

        let results = runtime
            .block_on(self.session.browse(
                &[description],
                max_references.clamp(1, 2000),
                None,
            ))
            .map_err(|error| format!("OPC UA browse failed: {error}"))?;

        let mut references = Vec::new();
        let mut continuation_points = Vec::new();
        for result in results {
            // A non-null continuation point means the server truncated this page.
            if !result.continuation_point.is_null() {
                continuation_points.push(result.continuation_point.clone());
            }
            for reference in result.references.clone().unwrap_or_default() {
                references.push(reference_to_json(&reference));
            }
        }
        // Follow continuation points so browsing a large namespace in one call
        // still returns complete results.
        let mut guard = 0;
        while !continuation_points.is_empty() && guard < 16 {
            guard += 1;
            let page = runtime
                .block_on(self.session.browse_next(false, &continuation_points))
                .map_err(|error| format!("OPC UA browse continuation failed: {error}"))?;
            continuation_points.clear();
            for result in page {
                if !result.continuation_point.is_null() {
                    continuation_points.push(result.continuation_point.clone());
                }
                for reference in result.references.clone().unwrap_or_default() {
                    references.push(reference_to_json(&reference));
                }
            }
        }

        Ok(json!({
            "nodeId": node_id,
            "references": references,
        }))
    }

    /// Creates a subscription and streams changes back to the workbench UI.
    pub fn subscribe(
        &self,
        runtime: &Runtime,
        connection_id: &str,
        node_ids: &[String],
        interval_ms: u64,
        emitter: &PluginEmitter,
    ) -> Result<Value, String> {
        if node_ids.is_empty() {
            return Err("at least one node id is required".to_string());
        }
        let nodes: Vec<NodeId> = node_ids
            .iter()
            .map(|value| parse_node_id(value))
            .collect::<Result<_, _>>()?;

        let emitter = emitter.clone();
        let connection = connection_id.to_string();
        let callback = DataChangeCallback::new(move |data_value: DataValue, item: &MonitoredItem| {
            let node_id = item.item_to_monitor().node_id.to_string();
            let payload = data_value_to_json(&node_id, &data_value);
            let event = json!({
                "connectionId": connection,
                "nodeId": node_id,
                "value": payload.get("value").cloned().unwrap_or(Value::Null),
                "dataType": payload.get("dataType").cloned().unwrap_or(Value::Null),
                "statusCode": payload.get("statusCode").cloned().unwrap_or(Value::Null),
                "status": payload.get("status").cloned().unwrap_or(Value::Null),
                "sourceTimestamp": payload.get("sourceTimestamp").cloned().unwrap_or(Value::Null),
            });
            // A failed event write means DBX went away; drop it silently rather
            // than poisoning the subscription callback.
            let _ = emitter.event("opcua/dataChange", event);
        });

        let publishing_interval = Duration::from_millis(interval_ms.clamp(100, 60_000));
        let subscription_id = runtime
            .block_on(self.session.create_subscription(
                publishing_interval,
                30,
                10,
                0,
                0,
                true,
                callback,
            ))
            .map_err(|error| format!("OPC UA subscription failed: {error}"))?;

        let requests: Vec<MonitoredItemCreateRequest> = nodes
            .into_iter()
            .map(|node_id| {
                let mut parameters = MonitoringParameters::default();
                parameters.sampling_interval = publishing_interval.as_millis().min(u32::MAX as u128) as f64;
                parameters.queue_size = 10;
                MonitoredItemCreateRequest::new(
                    ReadValueId::from(node_id),
                    opcua::types::MonitoringMode::Reporting,
                    parameters,
                )
            })
            .collect();

        runtime
            .block_on(self.session.create_monitored_items(
                subscription_id,
                TimestampsToReturn::Both,
                requests,
            ))
            .map_err(|error| format!("OPC UA monitored item creation failed: {error}"))?;

        self.subscriptions
            .lock()
            .map_err(|_| "OPC UA subscription lock is poisoned".to_string())?
            .push(subscription_id);

        Ok(json!({
            "subscriptionId": subscription_id,
            "intervalMs": publishing_interval.as_millis() as u64,
            "nodeIds": node_ids,
        }))
    }

    pub fn unsubscribe(&self, runtime: &Runtime, subscription_id: u32) -> Result<Value, String> {
        if subscription_id != 0 {
            runtime
                .block_on(self.session.delete_subscription(subscription_id))
                .map_err(|error| format!("OPC UA unsubscribe failed: {error}"))?;
        }
        if let Ok(mut guard) = self.subscriptions.lock() {
            guard.retain(|id| *id != subscription_id);
        }
        Ok(json!({ "subscriptionId": subscription_id, "removed": true }))
    }

    /// Closes the session and stops the event loop.
    pub fn disconnect(&self, runtime: &Runtime) {
        // Order matters: `Session::disconnect` round-trips a CloseSession request
        // through the event loop, so aborting the loop first would leave that
        // request unanswered and hang the sidecar on a dead session.
        close_session(runtime, &self.session);
        self.stop_event_loop();
    }

    /// Aborts the event loop task, if it is still running.
    fn stop_event_loop(&self) {
        if let Ok(mut guard) = self.event_loop.lock() {
            if let Some(handle) = guard.take() {
                // Dropping a `JoinHandle` only detaches the task, so a session
                // that is dropped without an explicit disconnect would otherwise
                // keep reconnecting in the background for the process lifetime.
                handle.abort();
            }
        }
    }
}

impl Drop for OpcuaSession {
    fn drop(&mut self) {
        self.stop_event_loop();
    }
}

/// Turns a browse result node into an id the host and the UI can round-trip.
///
/// Browse results carry [`ExpandedNodeId`]s, whose `Display` writes a
/// `svr=<index>;` prefix (and `nsu=<uri>;` when the server reports a namespace
/// URI instead of an index). `NodeId::from_str` accepts neither form, so every
/// id produced here must be reduced to the inner [`NodeId`]; otherwise reading a
/// node discovered through the address-space tree fails with `BadNodeIdInvalid`.
fn expanded_node_id(node_id: &ExpandedNodeId) -> String {
    node_id.node_id.to_string()
}

fn parse_node_id(value: &str) -> Result<NodeId, String> {
    let trimmed = value.trim();
    // Other OPC UA tooling renders ids as `svr=0;ns=2;i=1`. A server index only
    // means something to a multi-server client, so accept and drop the prefix.
    let normalized = match trimmed.strip_prefix("svr=") {
        Some(rest) => rest
            .split_once(';')
            .map(|(_, tail)| tail)
            .ok_or_else(|| format!("invalid OPC UA node id \"{value}\""))?,
        None => trimmed,
    };
    NodeId::from_str(normalized)
        .map_err(|status| format!("invalid OPC UA node id \"{value}\" ({status})"))
}

fn reference_to_json(reference: &opcua::types::ReferenceDescription) -> Value {
    json!({
        "nodeId": expanded_node_id(&reference.node_id),
        "browseName": reference.browse_name.to_string(),
        "displayName": reference.display_name.text.to_string(),
        "nodeClass": format!("{:?}", reference.node_class),
        "typeDefinition": expanded_node_id(&reference.type_definition),
        "referenceType": reference.reference_type_id.to_string(),
        "isForward": reference.is_forward,
    })
}

fn data_value_to_json(node_id: &str, data_value: &DataValue) -> Value {
    json!({
        "nodeId": node_id,
        "value": data_value
            .value
            .as_ref()
            .map(variant_to_json)
            .unwrap_or(Value::Null),
        "dataType": data_value
            .value
            .as_ref()
            .map(variant_type_name)
            .unwrap_or("Null"),
        "status": data_value
            .status
            .as_ref()
            .map(StatusCode::to_string)
            .unwrap_or_default(),
        "statusCode": data_value.status.as_ref().map(StatusCode::bits).unwrap_or(0),
        "good": data_value.status.as_ref().map(StatusCode::is_good).unwrap_or(false),
        "sourceTimestamp": data_value
            .source_timestamp
            .as_ref()
            .map(|value| value.to_rfc3339()),
        "serverTimestamp": data_value
            .server_timestamp
            .as_ref()
            .map(|value| value.to_rfc3339()),
    })
}

/// Human-readable OPC UA built-in type name for a value.
pub fn variant_type_name(variant: &Variant) -> &'static str {
    match variant {
        Variant::Empty => "Null",
        Variant::Boolean(_) => "Boolean",
        Variant::SByte(_) => "SByte",
        Variant::Byte(_) => "Byte",
        Variant::Int16(_) => "Int16",
        Variant::UInt16(_) => "UInt16",
        Variant::Int32(_) => "Int32",
        Variant::UInt32(_) => "UInt32",
        Variant::Int64(_) => "Int64",
        Variant::UInt64(_) => "UInt64",
        Variant::Float(_) => "Float",
        Variant::Double(_) => "Double",
        Variant::String(_) => "String",
        Variant::DateTime(_) => "DateTime",
        Variant::Guid(_) => "Guid",
        Variant::StatusCode(_) => "StatusCode",
        Variant::ByteString(_) => "ByteString",
        Variant::XmlElement(_) => "XmlElement",
        Variant::QualifiedName(_) => "QualifiedName",
        Variant::LocalizedText(_) => "LocalizedText",
        Variant::NodeId(_) => "NodeId",
        Variant::ExpandedNodeId(_) => "ExpandedNodeId",
        Variant::ExtensionObject(_) => "ExtensionObject",
        Variant::Variant(_) => "Variant",
        Variant::DataValue(_) => "DataValue",
        Variant::DiagnosticInfo(_) => "DiagnosticInfo",
        Variant::Array(_) => "Array",
    }
}

/// Converts an OPC UA value into plain JSON so the sandboxed UI can render it.
pub fn variant_to_json(variant: &Variant) -> Value {
    match variant {
        Variant::Empty => Value::Null,
        Variant::Boolean(value) => json!(value),
        Variant::SByte(value) => json!(value),
        Variant::Byte(value) => json!(value),
        Variant::Int16(value) => json!(value),
        Variant::UInt16(value) => json!(value),
        Variant::Int32(value) => json!(value),
        Variant::UInt32(value) => json!(value),
        Variant::Int64(value) => json!(value),
        Variant::UInt64(value) => json!(value),
        Variant::Float(value) => json!(value),
        Variant::Double(value) => json!(value),
        Variant::String(value) => json!(value.as_ref()),
        Variant::DateTime(value) => json!(value.to_rfc3339()),
        Variant::Guid(value) => json!(value.to_string()),
        Variant::StatusCode(value) => json!({
            "code": value.bits(),
            "status": value.to_string(),
            "good": value.is_good(),
        }),
        Variant::ByteString(value) => json!({ "byteLength": value.as_ref().len() }),
        Variant::XmlElement(value) => json!(value.to_string()),
        Variant::QualifiedName(value) => json!(value.to_string()),
        Variant::LocalizedText(value) => json!(value.text.to_string()),
        Variant::NodeId(value) => json!(value.to_string()),
        Variant::ExpandedNodeId(value) => json!(value.to_string()),
        Variant::ExtensionObject(_) => json!("[ExtensionObject]"),
        Variant::Variant(value) => variant_to_json(value),
        Variant::DataValue(value) => data_value_to_json("", value),
        Variant::DiagnosticInfo(_) => json!("[DiagnosticInfo]"),
        Variant::Array(array) => {
            Value::Array(array.values.iter().map(variant_to_json).collect())
        }
    }
}

/// Converts a JSON value from the workbench into an OPC UA [`Variant`].
///
/// `data_type` is an OPC UA built-in type name (as reported by a read) or
/// `Auto` to infer it from the JSON value.
pub fn json_to_variant(value: &Value, data_type: &str) -> Result<Variant, String> {
    if data_type == "Auto" {
        return Ok(infer_variant(value)?);
    }
    if let Value::Array(items) = value {
        let scalar = match data_type.strip_prefix("Array of ") {
            Some(inner) => inner.trim(),
            None => data_type,
        };
        let value_type = scalar_type_id(scalar)?;
        let variants = items
            .iter()
            .map(|item| scalar_variant(item, scalar))
            .collect::<Result<Vec<_>, _>>()?;
        return Array::new(value_type, variants)
            .map(|array| Variant::Array(Box::new(array)))
            .map_err(|error| format!("cannot build an OPC UA array: {error}"));
    }
    scalar_variant(value, data_type)
}

fn scalar_variant(value: &Value, data_type: &str) -> Result<Variant, String> {
    Ok(match data_type {
        "Boolean" | "Bool" => Variant::Boolean(
            value
                .as_bool()
                .ok_or_else(|| "value for Boolean must be true or false".to_string())?,
        ),
        "SByte" => Variant::SByte(number(value, data_type)? as i8),
        "Byte" => Variant::Byte(number(value, data_type)? as u8),
        "Int16" => Variant::Int16(number(value, data_type)? as i16),
        "UInt16" => Variant::UInt16(number(value, data_type)? as u16),
        "Int32" | "Integer" | "Int" => Variant::Int32(number(value, data_type)? as i32),
        "UInt32" | "UInteger" | "UInt" => Variant::UInt32(number(value, data_type)? as u32),
        "Int64" => Variant::Int64(number(value, data_type)? as i64),
        "UInt64" => Variant::UInt64(number(value, data_type)? as u64),
        "Float" => Variant::Float(number(value, data_type)? as f32),
        "Double" | "Number" => Variant::Double(number(value, data_type)?),
        "String" => Variant::String(opcua::types::UAString::from(
            value
                .as_str()
                .ok_or_else(|| "value for String must be text".to_string())?,
        )),
        "DateTime" => {
            let text = value
                .as_str()
                .ok_or_else(|| "value for DateTime must be an RFC 3339 string".to_string())?;
            let parsed = opcua::types::DateTime::from_str(text)
                .map_err(|error| format!("invalid DateTime \"{text}\": {error}"))?;
            Variant::DateTime(Box::new(parsed))
        }
        "NodeId" => {
            let text = value
                .as_str()
                .ok_or_else(|| "value for NodeId must be a node id string".to_string())?;
            Variant::NodeId(Box::new(parse_node_id(text)?))
        }
        "LocalizedText" => Variant::LocalizedText(Box::new(opcua::types::LocalizedText {
            locale: opcua::types::UAString::null(),
            text: opcua::types::UAString::from(
                value
                    .as_str()
                    .ok_or_else(|| "value for LocalizedText must be text".to_string())?,
            ),
        })),
        "QualifiedName" => Variant::QualifiedName(Box::new(QualifiedName {
            namespace_index: 0,
            name: opcua::types::UAString::from(
                value
                    .as_str()
                    .ok_or_else(|| "value for QualifiedName must be text".to_string())?,
            ),
        })),
        other => return Err(format!("cannot write values of type \"{other}\" from the workbench")),
    })
}

fn infer_variant(value: &Value) -> Result<Variant, String> {
    Ok(match value {
        Value::Null => Variant::Empty,
        Value::Bool(value) => Variant::Boolean(*value),
        Value::String(value) => Variant::String(opcua::types::UAString::from(value)),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Variant::Int64(value)
            } else {
                Variant::Double(
                    number
                        .as_f64()
                        .ok_or_else(|| "unsupported number".to_string())?,
                )
            }
        }
        Value::Array(items) => {
            let variants = items.iter().map(infer_variant).collect::<Result<Vec<_>, _>>()?;
            let value_type = variants
                .first()
                .map(scalar_type_id_from_variant)
                .transpose()?
                .unwrap_or(VariantScalarTypeId::Double);
            Array::new(value_type, variants)
                .map(|array| Variant::Array(Box::new(array)))
                .map_err(|error| format!("cannot build an OPC UA array: {error}"))?
        }
        Value::Object(_) => {
            return Err("cannot infer an OPC UA type for a JSON object".to_string())
        }
    })
}

fn scalar_type_id(name: &str) -> Result<VariantScalarTypeId, String> {
    Ok(match name {
        "Boolean" | "Bool" => VariantScalarTypeId::Boolean,
        "SByte" => VariantScalarTypeId::SByte,
        "Byte" => VariantScalarTypeId::Byte,
        "Int16" => VariantScalarTypeId::Int16,
        "UInt16" => VariantScalarTypeId::UInt16,
        "Int32" | "Integer" | "Int" => VariantScalarTypeId::Int32,
        "UInt32" | "UInteger" | "UInt" => VariantScalarTypeId::UInt32,
        "Int64" => VariantScalarTypeId::Int64,
        "UInt64" => VariantScalarTypeId::UInt64,
        "Float" => VariantScalarTypeId::Float,
        "Double" | "Number" => VariantScalarTypeId::Double,
        "String" => VariantScalarTypeId::String,
        "DateTime" => VariantScalarTypeId::DateTime,
        "Guid" => VariantScalarTypeId::Guid,
        "NodeId" => VariantScalarTypeId::NodeId,
        other => return Err(format!("unsupported OPC UA array element type \"{other}\"")),
    })
}

fn scalar_type_id_from_variant(variant: &Variant) -> Result<VariantScalarTypeId, String> {
    Ok(match variant {
        Variant::Boolean(_) => VariantScalarTypeId::Boolean,
        Variant::SByte(_) => VariantScalarTypeId::SByte,
        Variant::Byte(_) => VariantScalarTypeId::Byte,
        Variant::Int16(_) => VariantScalarTypeId::Int16,
        Variant::UInt16(_) => VariantScalarTypeId::UInt16,
        Variant::Int32(_) => VariantScalarTypeId::Int32,
        Variant::UInt32(_) => VariantScalarTypeId::UInt32,
        Variant::Int64(_) => VariantScalarTypeId::Int64,
        Variant::UInt64(_) => VariantScalarTypeId::UInt64,
        Variant::Float(_) => VariantScalarTypeId::Float,
        Variant::Double(_) => VariantScalarTypeId::Double,
        Variant::String(_) => VariantScalarTypeId::String,
        Variant::DateTime(_) => VariantScalarTypeId::DateTime,
        Variant::NodeId(_) => VariantScalarTypeId::NodeId,
        other => {
            return Err(format!(
                "cannot build an OPC UA array of {}",
                variant_type_name(other)
            ))
        }
    })
}

fn number(value: &Value, data_type: &str) -> Result<f64, String> {
    value
        .as_f64()
        .ok_or_else(|| format!("value for {data_type} must be a number"))
}
