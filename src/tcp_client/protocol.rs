//! Strict protocol-v1 decoding. Unknown, duplicate and incompatible fields fail closed.
use super::{MAX_RESPONSE_FRAME_BYTES, Number};
use serde::de::{Deserialize, DeserializeOwned, Deserializer, Error, MapAccess, Visitor};
use serde_json::value::RawValue;
use std::{collections::BTreeMap, fmt};

#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    Value { value: Number, unit: String },
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    pub key: String,
    pub completed_at_unix_ms: u64,
    pub reading: Reading,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberType {
    Integer,
    Float,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    pub key: String,
    pub number_type: NumberType,
    pub min: Number,
    pub max: Number,
    pub unit: String,
    pub writable: bool,
    pub poll_period_ns: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerError {
    pub error: String,
    pub key: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResponseData {
    Ack,
    Hello { protocol: String, version: u64 },
    Parameters(Vec<Parameter>),
    Measurement(Measurement),
    Subscriptions(Vec<String>),
    NoData { key: String },
    Error(ServerError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Error,
    NoData,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: u64,
    pub data: ResponseData,
}

impl Response {
    pub fn status(&self) -> Status {
        match &self.data {
            ResponseData::NoData { .. } => Status::NoData,
            ResponseData::Error(_)
            | ResponseData::Measurement(Measurement {
                reading: Reading::Error(_),
                ..
            }) => Status::Error,
            _ => Status::Ok,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Response(Response),
    Measurement(Measurement),
    ServerError(ServerError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

// Raw fields preserve numeric literals and let nested objects reject duplicates too.
struct Object(BTreeMap<String, Box<RawValue>>);
impl<'de> Deserialize<'de> for Object {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor;
        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = Object;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object with unique fields")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Object, M::Error> {
                let mut fields = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    if fields.insert(key, value).is_some() {
                        return Err(M::Error::custom("duplicate field"));
                    }
                }
                Ok(Object(fields))
            }
        }
        deserializer.deserialize_map(ObjectVisitor)
    }
}

fn decode<T: DeserializeOwned>(text: &str) -> Result<T, ParseError> {
    serde_json::from_str(text).map_err(|e| ParseError(e.to_string()))
}
impl Object {
    fn take<T: DeserializeOwned>(&mut self, key: &str) -> Result<T, ParseError> {
        let raw = self
            .0
            .remove(key)
            .ok_or_else(|| ParseError(format!("missing {key}")))?;
        decode(raw.get())
    }
    fn optional<T: DeserializeOwned>(&mut self, key: &str) -> Result<Option<T>, ParseError> {
        if self.0.contains_key(key) {
            self.take(key).map(Some)
        } else {
            Ok(None)
        }
    }
    fn number(&mut self, key: &str) -> Result<Number, ParseError> {
        let raw: Box<RawValue> = self.take(key)?;
        if raw.get().contains(['.', 'e', 'E']) {
            let value: f64 = decode(raw.get())?;
            if value.is_finite() {
                return Ok(Number::Float(value));
            }
            Err(ParseError("non-finite float".into()))
        } else {
            decode(raw.get()).map(Number::Integer)
        }
    }
    fn end(self) -> Result<(), ParseError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(ParseError("unexpected or incompatible fields".into()))
        }
    }
    fn measurement(&mut self, status: &str) -> Result<Measurement, ParseError> {
        let key = self.take("key")?;
        let completed_at_unix_ms = self.take("completed_at_unix_ms")?;
        let reading = match status {
            "ok" => Reading::Value {
                value: self.number("value")?,
                unit: self.take("unit")?,
            },
            "error" => Reading::Error(self.take("error")?),
            _ => return Err(ParseError("invalid measurement status".into())),
        };
        Ok(Measurement {
            key,
            completed_at_unix_ms,
            reading,
        })
    }
}

fn parameter(mut object: Object) -> Result<Parameter, ParseError> {
    let number_type = match object.take::<String>("type")?.as_str() {
        "integer" => NumberType::Integer,
        "float" => NumberType::Float,
        _ => return Err(ParseError("invalid parameter type".into())),
    };
    let parameter = Parameter {
        key: object.take("key")?,
        number_type,
        min: object.number("min")?,
        max: object.number("max")?,
        unit: object.take("unit")?,
        writable: object.take("writable")?,
        poll_period_ns: object.take("poll_period_ns")?,
    };
    let valid_range = match (parameter.min, parameter.max, number_type) {
        (Number::Integer(a), Number::Integer(b), NumberType::Integer) => a <= b,
        (Number::Float(a), Number::Float(b), NumberType::Float) => a <= b,
        _ => false,
    };
    if !valid_range {
        return Err(ParseError("invalid parameter bounds".into()));
    }
    object.end()?;
    Ok(parameter)
}

/// Decode one complete UTF-8 frame (without LF), bounded to 64 KiB.
pub fn parse_message(frame: &str) -> Result<Incoming, ParseError> {
    if frame.len() > MAX_RESPONSE_FRAME_BYTES {
        return Err(ParseError("frame too long".into()));
    }
    let mut object: Object = decode(frame)?;
    let status: String = object.take("status")?;
    let result = if object.0.contains_key("event") {
        if object.take::<String>("event")? != "measurement" {
            return Err(ParseError("unknown event".into()));
        }
        Incoming::Measurement(object.measurement(&status)?)
    } else if object.0.contains_key("id") {
        let id = object.take("id")?;
        if id == 0 {
            return Err(ParseError("invalid response id".into()));
        }
        let data = match status.as_str() {
            "ok" if object.0.contains_key("protocol") => ResponseData::Hello {
                protocol: object.take("protocol")?,
                version: object.take("version")?,
            },
            "ok" if object.0.contains_key("parameters") => {
                let objects: Vec<Object> = object.take("parameters")?;
                ResponseData::Parameters(
                    objects
                        .into_iter()
                        .map(parameter)
                        .collect::<Result<_, _>>()?,
                )
            }
            "ok" if object.0.contains_key("keys") => {
                ResponseData::Subscriptions(object.take("keys")?)
            }
            "ok" if object.0.contains_key("key") => {
                ResponseData::Measurement(object.measurement(&status)?)
            }
            "ok" => ResponseData::Ack,
            "no_data" => ResponseData::NoData {
                key: object.take("key")?,
            },
            "error" if object.0.contains_key("completed_at_unix_ms") => {
                ResponseData::Measurement(object.measurement(&status)?)
            }
            "error" => ResponseData::Error(ServerError {
                error: object.take("error")?,
                key: object.optional("key")?,
            }),
            _ => return Err(ParseError("invalid response status".into())),
        };
        Incoming::Response(Response { id, data })
    } else {
        if status != "error" {
            return Err(ParseError("missing response id".into()));
        }
        Incoming::ServerError(ServerError {
            error: object.take("error")?,
            key: object.optional("key")?,
        })
    };
    object.end()?;
    Ok(result)
}
