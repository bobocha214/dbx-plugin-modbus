//! Minimal Modbus TCP client.
//!
//! The plugin implements the Modbus Application Protocol itself instead of
//! pulling in a driver crate: the protocol is small, and a hand-written client
//! lets us expose the register/byte-order conventions that field devices
//! actually use (ABCD / BADC / CDAB / DCBA) without a translation layer.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

pub const FC_READ_COILS: u8 = 1;
pub const FC_READ_DISCRETE_INPUTS: u8 = 2;
pub const FC_READ_HOLDING_REGISTERS: u8 = 3;
pub const FC_READ_INPUT_REGISTERS: u8 = 4;

/// Modbus TCP protocol id is always 0.
const MBAP_PROTOCOL_ID: u16 = 0;

/// A single failed exchange. `retryable` is true only for transport-level
/// failures, because re-sending a request that the device answered with an
/// exception code would just repeat the same error.
struct Exchange {
    retryable: bool,
    message: String,
}

impl Exchange {
    fn protocol(message: impl Into<String>) -> Self {
        Self { retryable: false, message: message.into() }
    }

    fn transport(error: std::io::Error, stage: &str) -> Self {
        Self { retryable: true, message: format!("{stage}: {error}") }
    }
}

pub struct ModbusSession {
    host: String,
    port: u16,
    timeout: Duration,
    stream: Mutex<Option<TcpStream>>,
    transaction_id: AtomicU16,
}

impl ModbusSession {
    pub fn new(host: impl Into<String>, port: u16, timeout: Duration) -> Self {
        Self {
            host: host.into(),
            port,
            timeout,
            stream: Mutex::new(None),
            transaction_id: AtomicU16::new(1),
        }
    }

    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    fn connect(&self) -> Result<TcpStream, String> {
        let addresses: Vec<SocketAddr> = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|error| format!("cannot resolve {}: {error}", self.endpoint()))?
            .collect();
        if addresses.is_empty() {
            return Err(format!("cannot resolve {}", self.endpoint()));
        }
        let mut last_error = String::from("no address attempted");
        for address in addresses {
            match TcpStream::connect_timeout(&address, self.timeout) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(self.timeout)).map_err(|error| error.to_string())?;
                    stream.set_write_timeout(Some(self.timeout)).map_err(|error| error.to_string())?;
                    let _ = stream.set_nodelay(true);
                    return Ok(stream);
                }
                Err(error) => last_error = error.to_string(),
            }
        }
        Err(format!("cannot connect to {}: {last_error}", self.endpoint()))
    }

    fn next_transaction_id(&self) -> u16 {
        // Wrapping is fine: the id only has to correlate one in-flight request.
        self.transaction_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Verifies the endpoint is reachable and optionally performs a light read.
    ///
    /// Returns `(reachable, note)`. A Modbus exception on the probe address is
    /// reported as a note rather than a failure, because plenty of devices
    /// simply do not implement holding register 0.
    pub fn probe(&self) -> Result<(bool, String), String> {
        let _stream = self.connect()?;
        match self.read_registers(1, FC_READ_HOLDING_REGISTERS, 0, 1) {
            Ok(registers) => Ok((true, format!("holding register 0 = {}", registers[0]))),
            Err(error) => Ok((
                true,
                format!("TCP reachable; probe read returned: {error}"),
            )),
        }
    }

    pub fn disconnect(&self) {
        if let Ok(mut guard) = self.stream.lock() {
            *guard = None;
        }
    }

    fn exchange(
        stream: &mut TcpStream,
        transaction_id: u16,
        unit_id: u8,
        pdu: &[u8],
    ) -> Result<Vec<u8>, Exchange> {
        // MBAP: transaction id, protocol id, length (unit id + PDU), unit id.
        let mut frame = Vec::with_capacity(7 + pdu.len());
        frame.extend_from_slice(&transaction_id.to_be_bytes());
        frame.extend_from_slice(&MBAP_PROTOCOL_ID.to_be_bytes());
        frame.extend_from_slice(&((pdu.len() + 1) as u16).to_be_bytes());
        frame.push(unit_id);
        frame.extend_from_slice(pdu);

        stream
            .write_all(&frame)
            .map_err(|error| Exchange::transport(error, "write request"))?;

        let mut header = [0u8; 7];
        stream
            .read_exact(&mut header)
            .map_err(|error| Exchange::transport(error, "read response header"))?;

        // Bytes 0..2 are the transaction id, 2..4 the protocol id, 4..6 the
        // length, 6 the unit id.
        let response_id = u16::from_be_bytes([header[0], header[1]]);
        let protocol_id = u16::from_be_bytes([header[2], header[3]]);
        let length = u16::from_be_bytes([header[4], header[5]]) as usize;

        if protocol_id != MBAP_PROTOCOL_ID {
            return Err(Exchange::protocol(format!(
                "unexpected protocol id {protocol_id} in response"
            )));
        }
        if response_id != transaction_id {
            return Err(Exchange::protocol(format!(
                "transaction id mismatch: sent {transaction_id}, received {response_id}"
            )));
        }
        if length < 2 {
            return Err(Exchange::protocol("truncated Modbus response"));
        }

        let mut body = vec![0u8; length - 1];
        stream
            .read_exact(&mut body)
            .map_err(|error| Exchange::transport(error, "read response body"))?;

        let function = body[0];
        if function & 0x80 != 0 {
            let code = body.get(1).copied().unwrap_or(0);
            return Err(Exchange::protocol(format!(
                "Modbus exception {code} ({})",
                exception_name(code)
            )));
        }
        Ok(body[1..].to_vec())
    }

    fn transact(&self, unit_id: u8, pdu: &[u8]) -> Result<Vec<u8>, String> {
        let mut guard = self
            .stream
            .lock()
            .map_err(|_| "Modbus session lock is poisoned".to_string())?;
        if guard.is_none() {
            *guard = Some(self.connect()?);
        }

        let transaction_id = self.next_transaction_id();
        let first = {
            let stream = guard.as_mut().ok_or_else(|| "Modbus stream was lost".to_string())?;
            Self::exchange(stream, transaction_id, unit_id, pdu)
        };

        match first {
            Ok(body) => Ok(body),
            Err(error) if error.retryable => {
                // Field devices drop idle sockets without notice. Throw the dead
                // socket away and retry exactly once on a fresh connection.
                *guard = None;
                let mut stream = self.connect()?;
                let transaction_id = self.next_transaction_id();
                match Self::exchange(&mut stream, transaction_id, unit_id, pdu) {
                    Ok(body) => {
                        *guard = Some(stream);
                        Ok(body)
                    }
                    Err(error) => {
                        *guard = None;
                        Err(error.message)
                    }
                }
            }
            Err(error) => Err(error.message),
        }
    }

    pub fn read_bits(
        &self,
        unit_id: u8,
        function_code: u8,
        address: u16,
        quantity: u16,
    ) -> Result<Vec<bool>, String> {
        if !matches!(function_code, FC_READ_COILS | FC_READ_DISCRETE_INPUTS) {
            return Err(format!("function code {function_code} is not a bit read"));
        }
        if quantity == 0 || quantity > 2000 {
            return Err("quantity for a bit read must be 1..=2000".to_string());
        }
        let mut pdu = Vec::with_capacity(5);
        pdu.push(function_code);
        pdu.extend_from_slice(&address.to_be_bytes());
        pdu.extend_from_slice(&quantity.to_be_bytes());

        let body = self.transact(unit_id, &pdu)?;
        let byte_count = *body.first().ok_or("empty Modbus response")? as usize;
        let data = body.get(1..).ok_or("empty Modbus response")?;
        if data.len() < byte_count {
            return Err(format!(
                "short bit response: declared {byte_count} bytes, received {}",
                data.len()
            ));
        }
        let mut bits = Vec::with_capacity(quantity as usize);
        for index in 0..quantity as usize {
            bits.push(data[index / 8] & (1 << (index % 8)) != 0);
        }
        Ok(bits)
    }

    pub fn read_registers(
        &self,
        unit_id: u8,
        function_code: u8,
        address: u16,
        quantity: u16,
    ) -> Result<Vec<u16>, String> {
        if !matches!(function_code, FC_READ_HOLDING_REGISTERS | FC_READ_INPUT_REGISTERS) {
            return Err(format!("function code {function_code} is not a register read"));
        }
        if quantity == 0 || quantity > 125 {
            return Err("quantity for a register read must be 1..=125".to_string());
        }
        let mut pdu = Vec::with_capacity(5);
        pdu.push(function_code);
        pdu.extend_from_slice(&address.to_be_bytes());
        pdu.extend_from_slice(&quantity.to_be_bytes());

        let body = self.transact(unit_id, &pdu)?;
        let byte_count = *body.first().ok_or("empty Modbus response")? as usize;
        let data = body.get(1..).ok_or("empty Modbus response")?;
        if byte_count != quantity as usize * 2 {
            return Err(format!(
                "register count mismatch: requested {quantity}, device returned {} bytes",
                byte_count
            ));
        }
        if data.len() < byte_count {
            return Err(format!(
                "short register response: declared {byte_count} bytes, received {}",
                data.len()
            ));
        }
        Ok(data[..byte_count]
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect())
    }

    pub fn write_single_coil(&self, unit_id: u8, address: u16, value: bool) -> Result<u16, String> {
        let mut pdu = Vec::with_capacity(5);
        pdu.push(5);
        pdu.extend_from_slice(&address.to_be_bytes());
        pdu.extend_from_slice(&if value { 0xFF00u16 } else { 0x0000u16 }.to_be_bytes());
        let body = self.transact(unit_id, &pdu)?;
        echoed_address(&body, address)
    }

    pub fn write_multiple_coils(
        &self,
        unit_id: u8,
        address: u16,
        values: &[bool],
    ) -> Result<u16, String> {
        if values.is_empty() || values.len() > 1968 {
            return Err("a multiple coil write takes 1..=1968 values".to_string());
        }
        let byte_count = values.len().div_ceil(8);
        let mut packed = vec![0u8; byte_count];
        for (index, value) in values.iter().enumerate() {
            if *value {
                packed[index / 8] |= 1 << (index % 8);
            }
        }
        let mut pdu = Vec::with_capacity(6 + packed.len());
        pdu.push(15);
        pdu.extend_from_slice(&address.to_be_bytes());
        pdu.extend_from_slice(&(values.len() as u16).to_be_bytes());
        pdu.push(byte_count as u8);
        pdu.extend_from_slice(&packed);

        let body = self.transact(unit_id, &pdu)?;
        echoed_address(&body, address)
    }

    pub fn write_single_register(
        &self,
        unit_id: u8,
        address: u16,
        value: u16,
    ) -> Result<u16, String> {
        let mut pdu = Vec::with_capacity(5);
        pdu.push(6);
        pdu.extend_from_slice(&address.to_be_bytes());
        pdu.extend_from_slice(&value.to_be_bytes());
        let body = self.transact(unit_id, &pdu)?;
        echoed_address(&body, address)
    }

    pub fn write_multiple_registers(
        &self,
        unit_id: u8,
        address: u16,
        values: &[u16],
    ) -> Result<u16, String> {
        if values.is_empty() || values.len() > 123 {
            return Err("a multiple register write takes 1..=123 values".to_string());
        }
        let mut pdu = Vec::with_capacity(6 + values.len() * 2);
        pdu.push(16);
        pdu.extend_from_slice(&address.to_be_bytes());
        pdu.extend_from_slice(&(values.len() as u16).to_be_bytes());
        pdu.push((values.len() * 2) as u8);
        for value in values {
            pdu.extend_from_slice(&value.to_be_bytes());
        }

        let body = self.transact(unit_id, &pdu)?;
        echoed_address(&body, address)
    }
}

fn echoed_address(body: &[u8], expected: u16) -> Result<u16, String> {
    if body.len() < 2 {
        return Err("short write response".to_string());
    }
    let address = u16::from_be_bytes([body[0], body[1]]);
    if address != expected {
        return Err(format!(
            "write response echoed address {address}, expected {expected}"
        ));
    }
    Ok(address)
}

fn exception_name(code: u8) -> &'static str {
    match code {
        1 => "illegal function",
        2 => "illegal data address",
        3 => "illegal data value",
        4 => "server device failure",
        5 => "acknowledge",
        6 => "server device busy",
        8 => "memory parity error",
        10 => "gateway path unavailable",
        11 => "gateway target device failed to respond",
        _ => "unknown exception",
    }
}

/// Maps a manifest area key to the two Modbus read/write function codes.
pub fn area_function_codes(area: &str) -> Result<(u8, u8), String> {
    match area {
        "coil" | "coils" => Ok((FC_READ_COILS, 5)),
        "discrete" | "discrete_input" | "discrete_inputs" => Ok((FC_READ_DISCRETE_INPUTS, 0)),
        "holding" | "holding_register" | "holding_registers" => Ok((FC_READ_HOLDING_REGISTERS, 6)),
        "input" | "input_register" | "input_registers" => Ok((FC_READ_INPUT_REGISTERS, 0)),
        other => Err(format!("unknown Modbus area \"{other}\"")),
    }
}

pub fn byte_size(data_type: &str) -> Result<usize, String> {
    Ok(match data_type {
        "uint16" | "int16" => 2,
        "uint32" | "int32" | "float32" => 4,
        "uint64" | "int64" | "float64" => 8,
        other => return Err(format!("unknown Modbus data type \"{other}\"")),
    })
}

pub fn register_count(data_type: &str) -> Result<usize, String> {
    Ok(byte_size(data_type)? / 2)
}

/// Applies one of the four classic register byte orders.
///
/// Every transformation is its own inverse, so the exact same call is used when
/// decoding a device response and when encoding a write request.
fn apply_byte_order(bytes: &mut [u8], byte_order: &str) {
    match byte_order {
        // Big endian: the wire order already matches the typed order.
        "ABCD" => {}
        // Byte swap inside every 16-bit word.
        "BADC" => {
            let mut index = 0;
            while index + 1 < bytes.len() {
                bytes.swap(index, index + 1);
                index += 2;
            }
        }
        // Swap the two 16-bit halves of every 32-bit group.
        "CDAB" => {
            let mut index = 0;
            while index + 3 < bytes.len() {
                bytes.swap(index, index + 2);
                bytes.swap(index + 1, index + 3);
                index += 4;
            }
        }
        // Fully reversed 32-bit group.
        "DCBA" => {
            let mut index = 0;
            while index + 3 < bytes.len() {
                bytes.swap(index, index + 3);
                bytes.swap(index + 1, index + 2);
                index += 4;
            }
        }
        // Unknown values fall back to big endian rather than failing a read.
        _ => {}
    }
}

fn registers_to_bytes(registers: &[u16], byte_order: &str, word_order: &str) -> Vec<u8> {
    let mut words: Vec<u16> = registers.to_vec();
    if word_order == "little" {
        words.reverse();
    }
    let mut bytes = Vec::with_capacity(words.len() * 2);
    for word in words {
        bytes.extend_from_slice(&word.to_be_bytes());
    }
    apply_byte_order(&mut bytes, byte_order);
    bytes
}

fn bytes_to_registers(bytes: &[u8], byte_order: &str, word_order: &str) -> Vec<u16> {
    let mut bytes = bytes.to_vec();
    apply_byte_order(&mut bytes, byte_order);
    let mut words: Vec<u16> = bytes
        .chunks(2)
        .map(|chunk| ((chunk[0] as u16) << 8) | chunk.get(1).copied().unwrap_or(0) as u16)
        .collect();
    if word_order == "little" {
        words.reverse();
    }
    words
}

/// Decodes registers into a JSON scalar according to the classic conventions.
pub fn decode_registers(
    registers: &[u16],
    data_type: &str,
    byte_order: &str,
    word_order: &str,
) -> Result<Value, String> {
    let needed = register_count(data_type)?;
    if registers.len() < needed {
        return Err(format!(
            "decoding {data_type} needs {needed} register(s), received {}",
            registers.len()
        ));
    }
    let bytes = registers_to_bytes(&registers[..needed], byte_order, word_order);
    Ok(match data_type {
        "uint16" => json!(u16::from_be_bytes([bytes[0], bytes[1]])),
        "int16" => json!(i16::from_be_bytes([bytes[0], bytes[1]])),
        "uint32" => json!(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        "int32" => json!(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        "float32" => json!(f32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        "uint64" => json!(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]
        ])),
        "int64" => json!(i64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]
        ])),
        "float64" => json!(f64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]
        ])),
        other => return Err(format!("unknown Modbus data type \"{other}\"")),
    })
}

/// Encodes a JSON scalar into the register payload for a write request.
pub fn encode_value(
    value: &Value,
    data_type: &str,
    byte_order: &str,
    word_order: &str,
) -> Result<Vec<u16>, String> {
    let bytes: Vec<u8> = match data_type {
        "uint16" => (as_u64(value, data_type)? as u16).to_be_bytes().to_vec(),
        "int16" => (as_i64(value, data_type)? as i16).to_be_bytes().to_vec(),
        "uint32" => (as_u64(value, data_type)? as u32).to_be_bytes().to_vec(),
        "int32" => (as_i64(value, data_type)? as i32).to_be_bytes().to_vec(),
        "float32" => (as_f64(value, data_type)? as f32).to_be_bytes().to_vec(),
        "uint64" => as_u64(value, data_type)?.to_be_bytes().to_vec(),
        "int64" => as_i64(value, data_type)?.to_be_bytes().to_vec(),
        "float64" => as_f64(value, data_type)?.to_be_bytes().to_vec(),
        other => return Err(format!("unknown Modbus data type \"{other}\"")),
    };
    Ok(bytes_to_registers(&bytes, byte_order, word_order))
}

fn as_u64(value: &Value, data_type: &str) -> Result<u64, String> {
    value
        .as_u64()
        .or_else(|| value.as_f64().filter(|number| *number >= 0.0).map(|number| number as u64))
        .ok_or_else(|| format!("value for {data_type} must be a non-negative integer"))
}

fn as_i64(value: &Value, data_type: &str) -> Result<i64, String> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
        .ok_or_else(|| format!("value for {data_type} must be an integer"))
}

fn as_f64(value: &Value, data_type: &str) -> Result<f64, String> {
    value
        .as_f64()
        .ok_or_else(|| format!("value for {data_type} must be a number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_endian_round_trip() {
        let registers = [0x1234u16, 0x5678];
        let decoded = decode_registers(&registers, "float32", "ABCD", "big").unwrap();
        let bytes = [0x12u8, 0x34, 0x56, 0x78];
        assert_eq!(decoded, json!(f32::from_be_bytes(bytes)));
        let encoded = encode_value(&decoded, "float32", "ABCD", "big").unwrap();
        assert_eq!(encoded, registers);
    }

    #[test]
    fn word_swap_matches_classic_convention() {
        // CDAB swaps the two 16-bit halves of a 32-bit value.
        let decoded = decode_registers(&[0x5678, 0x1234], "uint32", "CDAB", "big").unwrap();
        assert_eq!(decoded, json!(0x12345678u32));
        assert_eq!(
            encode_value(&json!(0x12345678u32), "uint32", "CDAB", "big").unwrap(),
            vec![0x5678, 0x1234]
        );
    }

    #[test]
    fn byte_swap_matches_classic_convention() {
        let decoded = decode_registers(&[0x3412, 0x7856], "uint32", "BADC", "big").unwrap();
        assert_eq!(decoded, json!(0x12345678u32));
    }

    #[test]
    fn full_reverse_matches_classic_convention() {
        let decoded = decode_registers(&[0x7856, 0x3412], "uint32", "DCBA", "big").unwrap();
        assert_eq!(decoded, json!(0x12345678u32));
    }

    #[test]
    fn little_word_order_reverses_registers() {
        let registers = [0x1234u16, 0x5678];
        assert_eq!(
            registers_to_bytes(&registers, "ABCD", "little"),
            vec![0x56, 0x78, 0x12, 0x34]
        );
    }

    #[test]
    fn signed_decode_is_signed() {
        assert_eq!(
            decode_registers(&[0xFFFF], "int16", "ABCD", "big").unwrap(),
            json!(-1)
        );
    }

    #[test]
    fn area_lookup_rejects_unknown_areas() {
        assert_eq!(area_function_codes("holding").unwrap().0, FC_READ_HOLDING_REGISTERS);
        assert!(area_function_codes("nope").is_err());
    }
}
