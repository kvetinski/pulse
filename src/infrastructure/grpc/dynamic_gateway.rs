use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use prost::Message;
use prost::bytes::{Buf, BufMut};
use serde_json::{Map, Number, Value};
use tonic::client::Grpc;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::{Channel, Endpoint};
use tracing::info;

use crate::domain::error::PulseError;
use crate::domain::ports::{DynamicGrpcGateway, DynamicGrpcRequest, DynamicGrpcResponse};

#[derive(Clone)]
pub struct DescriptorBackedGrpcGateway {
    channel: Channel,
    methods: Arc<HashMap<(String, String), MethodMeta>>,
    messages: Arc<HashMap<String, MessageSchema>>,
}

#[derive(Clone, Debug)]
struct MethodMeta {
    route_path: String,
    client_streaming: bool,
    server_streaming: bool,
    input_type: String,
    output_type: String,
}

#[derive(Clone, Debug)]
struct MessageSchema {
    fields_by_name: HashMap<String, u32>,
    fields_by_number: HashMap<u32, FieldSchema>,
}

type MethodRegistry = HashMap<(String, String), MethodMeta>;
type MessageRegistry = HashMap<String, MessageSchema>;

#[derive(Clone, Debug)]
struct FieldSchema {
    name: String,
    number: u32,
    repeated: bool,
    packable: bool,
    kind: FieldKind,
}

#[derive(Clone, Debug)]
enum FieldKind {
    Double,
    Float,
    Int64,
    UInt64,
    Int32,
    Fixed64,
    Fixed32,
    Bool,
    String,
    Message(String),
    Bytes,
    UInt32,
    Enum,
    SFixed32,
    SFixed64,
    SInt32,
    SInt64,
}

impl DescriptorBackedGrpcGateway {
    pub async fn connect(endpoint: &str, descriptor_set_path: &str) -> Result<Self, PulseError> {
        info!(
            endpoint = %endpoint,
            descriptor_set_path = %descriptor_set_path,
            "connecting dynamic gRPC gateway"
        );

        let channel = Endpoint::from_shared(endpoint.to_string())
            .map_err(|e| PulseError::Client(format!("invalid endpoint: {e}")))?
            .connect()
            .await
            .map_err(|e| PulseError::Client(format!("connect failed: {e}")))?;

        let (methods, messages) = load_schema(descriptor_set_path)?;

        info!(
            endpoint = %endpoint,
            method_count = methods.len(),
            message_count = messages.len(),
            "connected dynamic gRPC gateway"
        );
        Ok(Self {
            channel,
            methods: Arc::new(methods),
            messages: Arc::new(messages),
        })
    }

    fn method_meta(&self, service: &str, method: &str) -> Result<&MethodMeta, PulseError> {
        let service = normalize_name(service);
        let method = method.to_string();
        self.methods
            .get(&(service.clone(), method.clone()))
            .ok_or_else(|| {
                PulseError::Client(format!(
                    "method not found in descriptor set: {service}/{method}"
                ))
            })
    }
}

#[async_trait]
impl DynamicGrpcGateway for DescriptorBackedGrpcGateway {
    async fn unary(&self, input: DynamicGrpcRequest) -> Result<DynamicGrpcResponse, PulseError> {
        let meta = self.method_meta(&input.service, &input.method)?;

        if meta.client_streaming || meta.server_streaming {
            return Err(PulseError::Client(format!(
                "method {}/{} is streaming; only unary is supported",
                input.service, input.method
            )));
        }

        let path: PathAndQuery = meta
            .route_path
            .parse()
            .map_err(|e| PulseError::Client(format!("invalid grpc route path: {e}")))?;

        let mut grpc = Grpc::new(self.channel.clone());
        grpc.ready().await.map_err(|e| {
            PulseError::Client(format!(
                "dynamic grpc client not ready for {}/{}: {e}",
                input.service, input.method
            ))
        })?;
        let response = grpc
            .unary(tonic::Request::new(input.payload), path, RawBytesCodec)
            .await
            .map_err(|status| PulseError::GrpcStatus {
                code: status.code().to_string(),
                message: format!(
                    "dynamic grpc call {}/{} failed: {status}",
                    input.service, input.method
                ),
            })?;

        Ok(DynamicGrpcResponse {
            payload: response.into_inner(),
        })
    }

    fn encode_request_fields(
        &self,
        service: &str,
        method: &str,
        fields: &Value,
    ) -> Result<Vec<u8>, PulseError> {
        let meta = self.method_meta(service, method)?;
        encode_message(&meta.input_type, fields, &self.messages)
    }

    fn decode_response_fields(
        &self,
        service: &str,
        method: &str,
        payload: &[u8],
    ) -> Result<Value, PulseError> {
        let meta = self.method_meta(service, method)?;
        decode_message(&meta.output_type, payload, &self.messages)
    }
}

fn load_schema(path: &str) -> Result<(MethodRegistry, MessageRegistry), PulseError> {
    let bytes = fs::read(path)
        .map_err(|e| PulseError::Client(format!("failed to read descriptor set '{path}': {e}")))?;
    let descriptor_set = prost_types::FileDescriptorSet::decode(bytes.as_slice()).map_err(|e| {
        PulseError::Client(format!(
            "failed to decode descriptor set '{path}' as FileDescriptorSet: {e}"
        ))
    })?;

    let mut messages: MessageRegistry = HashMap::new();
    let mut methods: MethodRegistry = HashMap::new();

    for file in descriptor_set.file {
        let package = file.package.unwrap_or_default();
        collect_messages(&package, None, &file.message_type, &mut messages)?;

        for service in file.service {
            let Some(service_name) = service.name else {
                continue;
            };
            if service_name.trim().is_empty() {
                continue;
            }

            let full_service_name = if package.is_empty() {
                service_name
            } else {
                format!("{package}.{service_name}")
            };

            for method in service.method {
                let Some(method_name) = method.name else {
                    continue;
                };
                if method_name.trim().is_empty() {
                    continue;
                }

                let input_type = normalize_name(method.input_type.as_deref().unwrap_or_default());
                let output_type = normalize_name(method.output_type.as_deref().unwrap_or_default());

                if !messages.contains_key(&input_type) {
                    return Err(PulseError::Client(format!(
                        "input type '{input_type}' for {full_service_name}/{method_name} not found in descriptor set"
                    )));
                }
                if !messages.contains_key(&output_type) {
                    return Err(PulseError::Client(format!(
                        "output type '{output_type}' for {full_service_name}/{method_name} not found in descriptor set"
                    )));
                }

                let key = (full_service_name.clone(), method_name.clone());
                let meta = MethodMeta {
                    route_path: format!("/{}/{}", full_service_name, method_name),
                    client_streaming: method.client_streaming.unwrap_or(false),
                    server_streaming: method.server_streaming.unwrap_or(false),
                    input_type,
                    output_type,
                };

                if methods.insert(key.clone(), meta).is_some() {
                    return Err(PulseError::Client(format!(
                        "duplicate method in descriptor set: {}/{}",
                        key.0, key.1
                    )));
                }
            }
        }
    }

    if methods.is_empty() {
        return Err(PulseError::Client(format!(
            "descriptor set '{path}' does not contain any service methods"
        )));
    }

    Ok((methods, messages))
}

fn collect_messages(
    package: &str,
    parent: Option<&str>,
    descriptors: &[prost_types::DescriptorProto],
    out: &mut HashMap<String, MessageSchema>,
) -> Result<(), PulseError> {
    for desc in descriptors {
        let Some(name) = desc.name.as_deref() else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }

        let full_name = match parent {
            Some(parent_name) => format!("{parent_name}.{name}"),
            None if package.is_empty() => name.to_string(),
            None => format!("{package}.{name}"),
        };

        let mut fields_by_name = HashMap::new();
        let mut fields_by_number = HashMap::new();
        for field in &desc.field {
            let field_name = field.name.clone().unwrap_or_default();
            if field_name.trim().is_empty() {
                continue;
            }
            let number = field.number.unwrap_or_default();
            if number <= 0 {
                continue;
            }
            let number = number as u32;

            let repeated = matches!(
                prost_types::field_descriptor_proto::Label::try_from(
                    field.label.unwrap_or_default()
                ),
                Ok(prost_types::field_descriptor_proto::Label::Repeated)
            );
            let field_type = prost_types::field_descriptor_proto::Type::try_from(
                field.r#type.unwrap_or_default(),
            )
            .map_err(|_| {
                PulseError::Client(format!(
                    "unsupported field type for {full_name}.{field_name}"
                ))
            })?;
            let kind = map_field_kind(field_type, field.type_name.as_deref())?;
            let packable = is_packable(&kind);

            let schema = FieldSchema {
                name: field_name.clone(),
                number,
                repeated,
                packable,
                kind,
            };

            fields_by_name.insert(field_name.clone(), number);
            if let Some(json_name) = &field.json_name
                && !json_name.trim().is_empty()
            {
                fields_by_name.insert(json_name.clone(), number);
            }
            fields_by_number.insert(number, schema);
        }

        if out
            .insert(
                full_name.clone(),
                MessageSchema {
                    fields_by_name,
                    fields_by_number,
                },
            )
            .is_some()
        {
            return Err(PulseError::Client(format!(
                "duplicate message in descriptor set: {full_name}"
            )));
        }

        collect_messages(package, Some(&full_name), &desc.nested_type, out)?;
    }
    Ok(())
}

fn map_field_kind(
    field_type: prost_types::field_descriptor_proto::Type,
    type_name: Option<&str>,
) -> Result<FieldKind, PulseError> {
    use prost_types::field_descriptor_proto::Type;
    let out = match field_type {
        Type::Double => FieldKind::Double,
        Type::Float => FieldKind::Float,
        Type::Int64 => FieldKind::Int64,
        Type::Uint64 => FieldKind::UInt64,
        Type::Int32 => FieldKind::Int32,
        Type::Fixed64 => FieldKind::Fixed64,
        Type::Fixed32 => FieldKind::Fixed32,
        Type::Bool => FieldKind::Bool,
        Type::String => FieldKind::String,
        Type::Group => {
            return Err(PulseError::Client(
                "protobuf groups are not supported".to_string(),
            ));
        }
        Type::Message => FieldKind::Message(normalize_name(type_name.unwrap_or_default())),
        Type::Bytes => FieldKind::Bytes,
        Type::Uint32 => FieldKind::UInt32,
        Type::Enum => FieldKind::Enum,
        Type::Sfixed32 => FieldKind::SFixed32,
        Type::Sfixed64 => FieldKind::SFixed64,
        Type::Sint32 => FieldKind::SInt32,
        Type::Sint64 => FieldKind::SInt64,
    };
    Ok(out)
}

fn normalize_name(value: &str) -> String {
    value.trim_start_matches('.').to_string()
}

fn encode_message(
    message_name: &str,
    value: &Value,
    messages: &HashMap<String, MessageSchema>,
) -> Result<Vec<u8>, PulseError> {
    let schema = messages.get(message_name).ok_or_else(|| {
        PulseError::Client(format!(
            "message '{message_name}' not found in descriptor set"
        ))
    })?;
    let Value::Object(map) = value else {
        return Err(PulseError::Client(format!(
            "request_fields for '{message_name}' must be a JSON object"
        )));
    };

    let mut field_pairs: Vec<(u32, &FieldSchema, &Value)> = Vec::new();
    for (key, field_value) in map {
        let Some(field_number) = schema.fields_by_name.get(key) else {
            return Err(PulseError::Client(format!(
                "field '{key}' is not defined in message '{message_name}'"
            )));
        };
        let field_schema = schema.fields_by_number.get(field_number).ok_or_else(|| {
            PulseError::Client(format!(
                "field number '{}' missing from schema for '{message_name}'",
                field_number
            ))
        })?;
        field_pairs.push((*field_number, field_schema, field_value));
    }
    field_pairs.sort_by_key(|(number, _, _)| *number);

    let mut out = Vec::new();
    for (_, field_schema, field_value) in field_pairs {
        encode_field(field_schema, field_value, messages, &mut out)?;
    }
    Ok(out)
}

fn encode_field(
    field: &FieldSchema,
    value: &Value,
    messages: &HashMap<String, MessageSchema>,
    out: &mut Vec<u8>,
) -> Result<(), PulseError> {
    if field.repeated {
        let Value::Array(items) = value else {
            return Err(PulseError::Client(format!(
                "field '{}' is repeated and expects a JSON array",
                field.name
            )));
        };
        if field.packable {
            encode_packed_repeated_field(field, items, out)
        } else {
            for item in items {
                encode_single_field_value(field, item, messages, out)?;
            }
            Ok(())
        }
    } else {
        encode_single_field_value(field, value, messages, out)
    }
}

fn encode_packed_repeated_field(
    field: &FieldSchema,
    items: &[Value],
    out: &mut Vec<u8>,
) -> Result<(), PulseError> {
    let mut packed = Vec::new();
    for item in items {
        encode_packable_value(&field.kind, item, &mut packed)?;
    }
    write_key(out, field.number, 2);
    write_varint(out, packed.len() as u64);
    out.extend_from_slice(&packed);
    Ok(())
}

fn encode_packable_value(
    kind: &FieldKind,
    value: &Value,
    out: &mut Vec<u8>,
) -> Result<(), PulseError> {
    match kind {
        FieldKind::Double => out.extend_from_slice(&to_f64(value)?.to_le_bytes()),
        FieldKind::Float => out.extend_from_slice(&to_f32(value)?.to_le_bytes()),
        FieldKind::Int64 => write_varint(out, to_i64(value)? as u64),
        FieldKind::UInt64 => write_varint(out, to_u64(value)?),
        FieldKind::Int32 => write_varint(out, to_i32(value)? as u32 as u64),
        FieldKind::Fixed64 => out.extend_from_slice(&to_u64(value)?.to_le_bytes()),
        FieldKind::Fixed32 => out.extend_from_slice(&to_u32(value)?.to_le_bytes()),
        FieldKind::Bool => write_varint(out, if to_bool(value)? { 1 } else { 0 }),
        FieldKind::UInt32 => write_varint(out, to_u32(value)? as u64),
        FieldKind::Enum => write_varint(out, to_i32(value)? as u32 as u64),
        FieldKind::SFixed32 => out.extend_from_slice(&to_i32(value)?.to_le_bytes()),
        FieldKind::SFixed64 => out.extend_from_slice(&to_i64(value)?.to_le_bytes()),
        FieldKind::SInt32 => write_varint(out, zigzag_i32(to_i32(value)?)),
        FieldKind::SInt64 => write_varint(out, zigzag_i64(to_i64(value)?)),
        FieldKind::String | FieldKind::Bytes | FieldKind::Message(_) => {
            return Err(PulseError::Client(
                "non-packable field encoded as packed".to_string(),
            ));
        }
    }
    Ok(())
}

fn encode_single_field_value(
    field: &FieldSchema,
    value: &Value,
    messages: &HashMap<String, MessageSchema>,
    out: &mut Vec<u8>,
) -> Result<(), PulseError> {
    match &field.kind {
        FieldKind::Double => {
            write_key(out, field.number, 1);
            out.extend_from_slice(&to_f64(value)?.to_le_bytes());
        }
        FieldKind::Float => {
            write_key(out, field.number, 5);
            out.extend_from_slice(&to_f32(value)?.to_le_bytes());
        }
        FieldKind::Int64 => {
            write_key(out, field.number, 0);
            write_varint(out, to_i64(value)? as u64);
        }
        FieldKind::UInt64 => {
            write_key(out, field.number, 0);
            write_varint(out, to_u64(value)?);
        }
        FieldKind::Int32 => {
            write_key(out, field.number, 0);
            write_varint(out, to_i32(value)? as u32 as u64);
        }
        FieldKind::Fixed64 => {
            write_key(out, field.number, 1);
            out.extend_from_slice(&to_u64(value)?.to_le_bytes());
        }
        FieldKind::Fixed32 => {
            write_key(out, field.number, 5);
            out.extend_from_slice(&to_u32(value)?.to_le_bytes());
        }
        FieldKind::Bool => {
            write_key(out, field.number, 0);
            write_varint(out, if to_bool(value)? { 1 } else { 0 });
        }
        FieldKind::String => {
            write_key(out, field.number, 2);
            let text = to_string_value(value)?;
            write_varint(out, text.len() as u64);
            out.extend_from_slice(text.as_bytes());
        }
        FieldKind::Message(message_name) => {
            let nested = encode_message(message_name, value, messages)?;
            write_key(out, field.number, 2);
            write_varint(out, nested.len() as u64);
            out.extend_from_slice(&nested);
        }
        FieldKind::Bytes => {
            write_key(out, field.number, 2);
            let bytes = to_bytes(value)?;
            write_varint(out, bytes.len() as u64);
            out.extend_from_slice(&bytes);
        }
        FieldKind::UInt32 => {
            write_key(out, field.number, 0);
            write_varint(out, to_u32(value)? as u64);
        }
        FieldKind::Enum => {
            write_key(out, field.number, 0);
            write_varint(out, to_i32(value)? as u32 as u64);
        }
        FieldKind::SFixed32 => {
            write_key(out, field.number, 5);
            out.extend_from_slice(&to_i32(value)?.to_le_bytes());
        }
        FieldKind::SFixed64 => {
            write_key(out, field.number, 1);
            out.extend_from_slice(&to_i64(value)?.to_le_bytes());
        }
        FieldKind::SInt32 => {
            write_key(out, field.number, 0);
            write_varint(out, zigzag_i32(to_i32(value)?));
        }
        FieldKind::SInt64 => {
            write_key(out, field.number, 0);
            write_varint(out, zigzag_i64(to_i64(value)?));
        }
    }
    Ok(())
}

fn decode_message(
    message_name: &str,
    bytes: &[u8],
    messages: &HashMap<String, MessageSchema>,
) -> Result<Value, PulseError> {
    let schema = messages.get(message_name).ok_or_else(|| {
        PulseError::Client(format!(
            "message '{message_name}' not found in descriptor set"
        ))
    })?;

    let mut out = Map::new();
    let mut idx = 0_usize;
    while idx < bytes.len() {
        let key = read_varint(bytes, &mut idx)?;
        let field_number = (key >> 3) as u32;
        let wire_type = (key & 0x7) as u8;

        let Some(field) = schema.fields_by_number.get(&field_number) else {
            skip_wire_value(wire_type, bytes, &mut idx)?;
            continue;
        };

        if field.repeated && field.packable && wire_type == 2 {
            let packed_len = read_varint(bytes, &mut idx)? as usize;
            let end = idx
                .checked_add(packed_len)
                .ok_or_else(|| PulseError::Client("packed field length overflow".to_string()))?;
            if end > bytes.len() {
                return Err(PulseError::Client(
                    "packed field exceeds payload bounds".to_string(),
                ));
            }
            let mut packed_idx = idx;
            let mut arr = Vec::new();
            while packed_idx < end {
                arr.push(decode_packable_value(&field.kind, bytes, &mut packed_idx)?);
            }
            idx = end;
            append_decoded_field(&mut out, &field.name, Value::Array(arr), true);
            continue;
        }

        let value = decode_single_field_value(field, wire_type, bytes, &mut idx, messages)?;
        append_decoded_field(&mut out, &field.name, value, field.repeated);
    }

    Ok(Value::Object(out))
}

fn decode_single_field_value(
    field: &FieldSchema,
    wire_type: u8,
    bytes: &[u8],
    idx: &mut usize,
    messages: &HashMap<String, MessageSchema>,
) -> Result<Value, PulseError> {
    match &field.kind {
        FieldKind::Double => {
            ensure_wire_type(wire_type, 1, field)?;
            let v = read_fixed64(bytes, idx)?;
            let n = Number::from_f64(f64::from_le_bytes(v.to_le_bytes()))
                .ok_or_else(|| PulseError::Client("invalid double value".to_string()))?;
            Ok(Value::Number(n))
        }
        FieldKind::Float => {
            ensure_wire_type(wire_type, 5, field)?;
            let v = read_fixed32(bytes, idx)?;
            let n = Number::from_f64(f32::from_le_bytes(v.to_le_bytes()) as f64)
                .ok_or_else(|| PulseError::Client("invalid float value".to_string()))?;
            Ok(Value::Number(n))
        }
        FieldKind::Int64 => {
            ensure_wire_type(wire_type, 0, field)?;
            Ok(Value::Number(Number::from(read_varint(bytes, idx)? as i64)))
        }
        FieldKind::UInt64 => {
            ensure_wire_type(wire_type, 0, field)?;
            Ok(Value::Number(Number::from(read_varint(bytes, idx)?)))
        }
        FieldKind::Int32 => {
            ensure_wire_type(wire_type, 0, field)?;
            Ok(Value::Number(Number::from(read_varint(bytes, idx)? as i32)))
        }
        FieldKind::Fixed64 => {
            ensure_wire_type(wire_type, 1, field)?;
            Ok(Value::Number(Number::from(read_fixed64(bytes, idx)?)))
        }
        FieldKind::Fixed32 => {
            ensure_wire_type(wire_type, 5, field)?;
            Ok(Value::Number(Number::from(read_fixed32(bytes, idx)?)))
        }
        FieldKind::Bool => {
            ensure_wire_type(wire_type, 0, field)?;
            Ok(Value::Bool(read_varint(bytes, idx)? != 0))
        }
        FieldKind::String => {
            ensure_wire_type(wire_type, 2, field)?;
            let data = read_length_delimited(bytes, idx)?;
            let text = String::from_utf8(data.to_vec()).map_err(|e| {
                PulseError::Client(format!("invalid utf8 in field '{}': {e}", field.name))
            })?;
            Ok(Value::String(text))
        }
        FieldKind::Message(message_name) => {
            ensure_wire_type(wire_type, 2, field)?;
            let data = read_length_delimited(bytes, idx)?;
            decode_message(message_name, data, messages)
        }
        FieldKind::Bytes => {
            ensure_wire_type(wire_type, 2, field)?;
            let data = read_length_delimited(bytes, idx)?;
            Ok(Value::String(
                base64::engine::general_purpose::STANDARD.encode(data),
            ))
        }
        FieldKind::UInt32 => {
            ensure_wire_type(wire_type, 0, field)?;
            Ok(Value::Number(Number::from(read_varint(bytes, idx)? as u32)))
        }
        FieldKind::Enum => {
            ensure_wire_type(wire_type, 0, field)?;
            Ok(Value::Number(Number::from(read_varint(bytes, idx)? as i32)))
        }
        FieldKind::SFixed32 => {
            ensure_wire_type(wire_type, 5, field)?;
            Ok(Value::Number(
                Number::from(read_fixed32(bytes, idx)? as i32),
            ))
        }
        FieldKind::SFixed64 => {
            ensure_wire_type(wire_type, 1, field)?;
            Ok(Value::Number(
                Number::from(read_fixed64(bytes, idx)? as i64),
            ))
        }
        FieldKind::SInt32 => {
            ensure_wire_type(wire_type, 0, field)?;
            Ok(Value::Number(Number::from(unzigzag_i32(read_varint(
                bytes, idx,
            )?)?)))
        }
        FieldKind::SInt64 => {
            ensure_wire_type(wire_type, 0, field)?;
            Ok(Value::Number(Number::from(unzigzag_i64(read_varint(
                bytes, idx,
            )?))))
        }
    }
}

fn decode_packable_value(
    kind: &FieldKind,
    bytes: &[u8],
    idx: &mut usize,
) -> Result<Value, PulseError> {
    match kind {
        FieldKind::Double => {
            let v = f64::from_le_bytes(read_fixed64(bytes, idx)?.to_le_bytes());
            let n = Number::from_f64(v)
                .ok_or_else(|| PulseError::Client("invalid double value".to_string()))?;
            Ok(Value::Number(n))
        }
        FieldKind::Float => {
            let v = f32::from_le_bytes(read_fixed32(bytes, idx)?.to_le_bytes()) as f64;
            let n = Number::from_f64(v)
                .ok_or_else(|| PulseError::Client("invalid float value".to_string()))?;
            Ok(Value::Number(n))
        }
        FieldKind::Int64 => Ok(Value::Number(Number::from(read_varint(bytes, idx)? as i64))),
        FieldKind::UInt64 => Ok(Value::Number(Number::from(read_varint(bytes, idx)?))),
        FieldKind::Int32 => Ok(Value::Number(Number::from(read_varint(bytes, idx)? as i32))),
        FieldKind::Fixed64 => Ok(Value::Number(Number::from(read_fixed64(bytes, idx)?))),
        FieldKind::Fixed32 => Ok(Value::Number(Number::from(read_fixed32(bytes, idx)?))),
        FieldKind::Bool => Ok(Value::Bool(read_varint(bytes, idx)? != 0)),
        FieldKind::UInt32 => Ok(Value::Number(Number::from(read_varint(bytes, idx)? as u32))),
        FieldKind::Enum => Ok(Value::Number(Number::from(read_varint(bytes, idx)? as i32))),
        FieldKind::SFixed32 => Ok(Value::Number(
            Number::from(read_fixed32(bytes, idx)? as i32),
        )),
        FieldKind::SFixed64 => Ok(Value::Number(
            Number::from(read_fixed64(bytes, idx)? as i64),
        )),
        FieldKind::SInt32 => Ok(Value::Number(Number::from(unzigzag_i32(read_varint(
            bytes, idx,
        )?)?))),
        FieldKind::SInt64 => Ok(Value::Number(Number::from(unzigzag_i64(read_varint(
            bytes, idx,
        )?)))),
        FieldKind::String | FieldKind::Bytes | FieldKind::Message(_) => Err(PulseError::Client(
            "non-packable field decoded as packed".to_string(),
        )),
    }
}

fn append_decoded_field(target: &mut Map<String, Value>, name: &str, value: Value, repeated: bool) {
    if repeated {
        match target.get_mut(name) {
            Some(Value::Array(existing)) => existing.push(value),
            Some(existing) => {
                let old = existing.clone();
                *existing = Value::Array(vec![old, value]);
            }
            None => {
                target.insert(name.to_string(), Value::Array(vec![value]));
            }
        }
    } else {
        target.insert(name.to_string(), value);
    }
}

fn ensure_wire_type(wire_type: u8, expected: u8, field: &FieldSchema) -> Result<(), PulseError> {
    if wire_type == expected {
        return Ok(());
    }
    Err(PulseError::Client(format!(
        "unexpected wire type {} for field '{}' (expected {})",
        wire_type, field.name, expected
    )))
}

fn skip_wire_value(wire_type: u8, bytes: &[u8], idx: &mut usize) -> Result<(), PulseError> {
    match wire_type {
        0 => {
            let _ = read_varint(bytes, idx)?;
        }
        1 => {
            advance(bytes, idx, 8)?;
        }
        2 => {
            let len = read_varint(bytes, idx)? as usize;
            advance(bytes, idx, len)?;
        }
        5 => {
            advance(bytes, idx, 4)?;
        }
        _ => {
            return Err(PulseError::Client(format!(
                "unsupported wire type while skipping unknown field: {wire_type}"
            )));
        }
    }
    Ok(())
}

fn advance(bytes: &[u8], idx: &mut usize, n: usize) -> Result<(), PulseError> {
    let end = idx
        .checked_add(n)
        .ok_or_else(|| PulseError::Client("buffer index overflow".to_string()))?;
    if end > bytes.len() {
        return Err(PulseError::Client(
            "buffer underflow while decoding".to_string(),
        ));
    }
    *idx = end;
    Ok(())
}

fn read_varint(bytes: &[u8], idx: &mut usize) -> Result<u64, PulseError> {
    let mut shift = 0_u32;
    let mut out = 0_u64;
    loop {
        if *idx >= bytes.len() {
            return Err(PulseError::Client(
                "unexpected end of payload while reading varint".to_string(),
            ));
        }
        let b = bytes[*idx];
        *idx += 1;
        out |= ((b & 0x7F) as u64) << shift;
        if (b & 0x80) == 0 {
            return Ok(out);
        }
        shift += 7;
        if shift > 63 {
            return Err(PulseError::Client("varint too long".to_string()));
        }
    }
}

fn read_fixed32(bytes: &[u8], idx: &mut usize) -> Result<u32, PulseError> {
    let end = idx
        .checked_add(4)
        .ok_or_else(|| PulseError::Client("buffer index overflow".to_string()))?;
    if end > bytes.len() {
        return Err(PulseError::Client(
            "buffer underflow reading fixed32".to_string(),
        ));
    }
    let mut arr = [0_u8; 4];
    arr.copy_from_slice(&bytes[*idx..end]);
    *idx = end;
    Ok(u32::from_le_bytes(arr))
}

fn read_fixed64(bytes: &[u8], idx: &mut usize) -> Result<u64, PulseError> {
    let end = idx
        .checked_add(8)
        .ok_or_else(|| PulseError::Client("buffer index overflow".to_string()))?;
    if end > bytes.len() {
        return Err(PulseError::Client(
            "buffer underflow reading fixed64".to_string(),
        ));
    }
    let mut arr = [0_u8; 8];
    arr.copy_from_slice(&bytes[*idx..end]);
    *idx = end;
    Ok(u64::from_le_bytes(arr))
}

fn read_length_delimited<'a>(bytes: &'a [u8], idx: &mut usize) -> Result<&'a [u8], PulseError> {
    let len = read_varint(bytes, idx)? as usize;
    let end = idx
        .checked_add(len)
        .ok_or_else(|| PulseError::Client("length-delimited overflow".to_string()))?;
    if end > bytes.len() {
        return Err(PulseError::Client(
            "length-delimited field exceeds payload".to_string(),
        ));
    }
    let out = &bytes[*idx..end];
    *idx = end;
    Ok(out)
}

fn write_key(out: &mut Vec<u8>, field_number: u32, wire_type: u8) {
    write_varint(out, ((field_number as u64) << 3) | (wire_type as u64));
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        if value < 0x80 {
            out.push(value as u8);
            return;
        }
        out.push(((value as u8) & 0x7F) | 0x80);
        value >>= 7;
    }
}

fn zigzag_i32(value: i32) -> u64 {
    ((value << 1) ^ (value >> 31)) as u32 as u64
}

fn zigzag_i64(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag_i32(value: u64) -> Result<i32, PulseError> {
    let v = value as u32;
    let out = ((v >> 1) as i32) ^ (-((v & 1) as i32));
    Ok(out)
}

fn unzigzag_i64(value: u64) -> i64 {
    ((value >> 1) as i64) ^ (-((value & 1) as i64))
}

fn to_string_value(value: &Value) -> Result<String, PulseError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null => Err(PulseError::Client("null is not valid here".to_string())),
        Value::Array(_) | Value::Object(_) => Err(PulseError::Client(
            "object/array is not valid for scalar field".to_string(),
        )),
    }
}

fn to_bool(value: &Value) -> Result<bool, PulseError> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Ok(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Ok(false),
        _ => Err(PulseError::Client(format!(
            "expected boolean value, got {value}"
        ))),
    }
}

fn to_i32(value: &Value) -> Result<i32, PulseError> {
    match value {
        Value::Number(n) => n
            .as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .ok_or_else(|| PulseError::Client(format!("expected i32 value, got {value}"))),
        Value::String(s) => s
            .parse::<i32>()
            .map_err(|_| PulseError::Client(format!("expected i32 string, got '{s}'"))),
        _ => Err(PulseError::Client(format!(
            "expected i32 value, got {value}"
        ))),
    }
}

fn to_i64(value: &Value) -> Result<i64, PulseError> {
    match value {
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| PulseError::Client(format!("expected i64 value, got {value}"))),
        Value::String(s) => s
            .parse::<i64>()
            .map_err(|_| PulseError::Client(format!("expected i64 string, got '{s}'"))),
        _ => Err(PulseError::Client(format!(
            "expected i64 value, got {value}"
        ))),
    }
}

fn to_u32(value: &Value) -> Result<u32, PulseError> {
    match value {
        Value::Number(n) => n
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| PulseError::Client(format!("expected u32 value, got {value}"))),
        Value::String(s) => s
            .parse::<u32>()
            .map_err(|_| PulseError::Client(format!("expected u32 string, got '{s}'"))),
        _ => Err(PulseError::Client(format!(
            "expected u32 value, got {value}"
        ))),
    }
}

fn to_u64(value: &Value) -> Result<u64, PulseError> {
    match value {
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| PulseError::Client(format!("expected u64 value, got {value}"))),
        Value::String(s) => s
            .parse::<u64>()
            .map_err(|_| PulseError::Client(format!("expected u64 string, got '{s}'"))),
        _ => Err(PulseError::Client(format!(
            "expected u64 value, got {value}"
        ))),
    }
}

fn to_f32(value: &Value) -> Result<f32, PulseError> {
    match value {
        Value::Number(n) => n
            .as_f64()
            .map(|v| v as f32)
            .ok_or_else(|| PulseError::Client(format!("expected float value, got {value}"))),
        Value::String(s) => s
            .parse::<f32>()
            .map_err(|_| PulseError::Client(format!("expected float string, got '{s}'"))),
        _ => Err(PulseError::Client(format!(
            "expected float value, got {value}"
        ))),
    }
}

fn to_f64(value: &Value) -> Result<f64, PulseError> {
    match value {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| PulseError::Client(format!("expected double value, got {value}"))),
        Value::String(s) => s
            .parse::<f64>()
            .map_err(|_| PulseError::Client(format!("expected double string, got '{s}'"))),
        _ => Err(PulseError::Client(format!(
            "expected double value, got {value}"
        ))),
    }
}

fn to_bytes(value: &Value) -> Result<Vec<u8>, PulseError> {
    match value {
        Value::String(s) => {
            if let Some(encoded) = s.strip_prefix("base64:") {
                return base64::engine::general_purpose::STANDARD
                    .decode(encoded.as_bytes())
                    .map_err(|e| PulseError::Client(format!("invalid base64 bytes value: {e}")));
            }
            Ok(s.as_bytes().to_vec())
        }
        _ => Err(PulseError::Client(format!(
            "expected bytes field as string, got {value}"
        ))),
    }
}

fn is_packable(kind: &FieldKind) -> bool {
    matches!(
        kind,
        FieldKind::Double
            | FieldKind::Float
            | FieldKind::Int64
            | FieldKind::UInt64
            | FieldKind::Int32
            | FieldKind::Fixed64
            | FieldKind::Fixed32
            | FieldKind::Bool
            | FieldKind::UInt32
            | FieldKind::Enum
            | FieldKind::SFixed32
            | FieldKind::SFixed64
            | FieldKind::SInt32
            | FieldKind::SInt64
    )
}

#[derive(Clone, Debug, Default)]
struct RawBytesCodec;

#[derive(Clone, Debug, Default)]
struct RawBytesEncoder;

#[derive(Clone, Debug, Default)]
struct RawBytesDecoder;

impl Codec for RawBytesCodec {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;
    type Encoder = RawBytesEncoder;
    type Decoder = RawBytesDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        RawBytesEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        RawBytesDecoder
    }
}

impl Encoder for RawBytesEncoder {
    type Item = Vec<u8>;
    type Error = tonic::Status;

    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for RawBytesDecoder {
    type Item = Vec<u8>;
    type Error = tonic::Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        let len = src.remaining();
        let mut out = vec![0_u8; len];
        src.copy_to_slice(&mut out);
        Ok(Some(out))
    }
}
