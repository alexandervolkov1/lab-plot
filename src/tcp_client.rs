//! Standalone protocol-v1 TCP client; no GUI or Steel integration.
//!
//! Start with [`Client::start`], poll [`Client::try_recv`] for [`ClientEvent::Ready`],
//! then clone [`Client::sender`] for producers. [`ClientSender::request`] returns
//! the admitted request ID or an explicit rejection (nothing sent). Replies are
//! matched by ID, while measurements arrive independently on the same socket.
//!
//! Defaults: 64 outgoing commands, 64 pending requests, 1024 incoming events,
//! 16 KiB outgoing / 64 KiB incoming frames. An event overflow closes the socket;
//! [`Client::state`] exposes the failure even when the event queue is full.
//! A termination-only reserve of at most 130 events preserves final request
//! results. Keep draining `try_recv` through `ClientEvent::Stopped`.
//!
//! A `set` rejected locally or reported as [`RequestFailure::NotSent`] cannot
//! execute. A sent `set` without a reliable reply yields `OutcomeUnknown`.
//! Server errors remain explicit in [`ResponseData::Error`]: `runtime_timeout`
//! means execution never started; `outcome_unknown` means it may have started.
//! No request is retried. EOF never proves stream completeness.
//!
//! Connection/hello timeouts default to 3 seconds; pending requests to 15 seconds.
//! A local timeout closes the connection and preserves uncertainty for sent sets.
//! [`Client::request_shutdown`] only signals; [`Client::shutdown`] and Drop join
//! the worker (potentially waiting for the bounded connection attempt).
use serde_json::{Value, json};

mod protocol;
mod worker;
pub use protocol::*;
pub use worker::*;

pub const MAX_REQUEST_FRAME_BYTES: usize = 16 * 1024;

/// Numeric parameter bounds. Boolean parameters have no numeric bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    Integer(i64),
    Float(f64),
}

/// A protocol value. Boolean is never coerced to a number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScalarValue {
    Integer(i64),
    Float(f64),
    Boolean(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Ping,
    Describe,
    Get { key: String },
    Set { key: String, value: ScalarValue },
    Subscribe { keys: Vec<String> },
    Unsubscribe { keys: Vec<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    InvalidId,
    NonFiniteFloat,
    FrameTooLong,
}

pub fn encode_request(id: u64, request: &Request) -> Result<String, EncodeError> {
    if id == 0 {
        return Err(EncodeError::InvalidId);
    }

    // Bound temporary serialization allocations as well as the resulting frame.
    let payload_size = match request {
        Request::Get { key } | Request::Set { key, .. } => key.len(),
        Request::Subscribe { keys } | Request::Unsubscribe { keys } => keys
            .iter()
            .try_fold(0usize, |size, key| {
                size.checked_add(key.len().saturating_add(3))
            })
            .unwrap_or(usize::MAX),
        _ => 0,
    };
    if payload_size > MAX_REQUEST_FRAME_BYTES {
        return Err(EncodeError::FrameTooLong);
    }

    let message = match request {
        Request::Ping => json!({
            "id": id,
            "op": "ping"
        }),

        Request::Describe => json!({
            "id": id,
            "op": "describe"
        }),

        Request::Get { key } => json!({
            "id": id,
            "op": "get",
            "key": key
        }),

        Request::Set { key, value } => {
            let value = match value {
                ScalarValue::Boolean(value) => Value::Bool(*value),
                ScalarValue::Integer(value) => Value::from(*value),

                ScalarValue::Float(value) => {
                    let number =
                        serde_json::Number::from_f64(*value).ok_or(EncodeError::NonFiniteFloat)?;

                    Value::Number(number)
                }
            };

            json!({
                "id": id,
                "op": "set",
                "key": key,
                "value": value
            })
        }

        Request::Subscribe { keys } => json!({
            "id": id,
            "op": "subscribe",
            "keys": keys
        }),

        Request::Unsubscribe { keys } => json!({
            "id": id,
            "op": "unsubscribe",
            "keys": keys
        }),
    };

    let mut line = message.to_string();

    if line.len() > MAX_REQUEST_FRAME_BYTES {
        return Err(EncodeError::FrameTooLong);
    }

    line.push('\n');

    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_basic_requests() {
        let cases = [
            (Request::Ping, "ping"),
            (Request::Describe, "describe"),
            (
                Request::Get {
                    key: "heater_temperature".into(),
                },
                "get",
            ),
            (
                Request::Subscribe {
                    keys: vec!["heater_temperature".into()],
                },
                "subscribe",
            ),
            (
                Request::Unsubscribe {
                    keys: vec!["heater_temperature".into()],
                },
                "unsubscribe",
            ),
        ];

        for (index, (request, op)) in cases.into_iter().enumerate() {
            let id = (index + 1) as u64;
            let line = encode_request(id, &request).unwrap();
            let parsed: Value = serde_json::from_str(&line).unwrap();

            assert_eq!(parsed["id"].as_u64(), Some(id));
            assert_eq!(parsed["op"].as_str(), Some(op));
            assert!(line.ends_with('\n'));
        }
    }

    #[test]
    fn preserves_integer_and_float_types() {
        let integer = encode_request(
            1,
            &Request::Set {
                key: "power".into(),
                value: ScalarValue::Integer(20),
            },
        )
        .unwrap();

        let float = encode_request(
            2,
            &Request::Set {
                key: "power".into(),
                value: ScalarValue::Float(20.0),
            },
        )
        .unwrap();

        let integer_json: Value = serde_json::from_str(&integer).unwrap();
        let float_json: Value = serde_json::from_str(&float).unwrap();

        assert!(integer_json["value"].as_number().unwrap().is_i64());
        assert!(float_json["value"].as_number().unwrap().is_f64());
        assert_eq!(float_json["value"].as_f64(), Some(20.0));
    }

    #[test]
    fn rejects_invalid_numbers_and_ids() {
        assert_eq!(
            encode_request(0, &Request::Ping),
            Err(EncodeError::InvalidId)
        );

        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                encode_request(
                    1,
                    &Request::Set {
                        key: "power".into(),
                        value: ScalarValue::Float(value),
                    }
                ),
                Err(EncodeError::NonFiniteFloat)
            );
        }
    }

    #[test]
    fn rejects_oversized_frames() {
        let request = Request::Get {
            key: "x".repeat(MAX_REQUEST_FRAME_BYTES),
        };

        assert_eq!(encode_request(1, &request), Err(EncodeError::FrameTooLong));
    }

    #[test]
    fn escapes_newlines_inside_strings() {
        let request = Request::Get { key: "a\nb".into() };

        let line = encode_request(1, &request).unwrap();

        assert_eq!(line.bytes().filter(|byte| *byte == b'\n').count(), 1);

        let parsed: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(parsed["key"].as_str(), Some("a\nb"));
    }
}

pub const MAX_RESPONSE_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    EmptyFrame,
    FrameTooLong,
    InvalidUtf8,
    IncompleteFrame,
}

#[derive(Debug, PartialEq, Eq)]
pub struct FrameFailure {
    pub frames: Vec<String>,
    pub error: FrameError,
}

#[derive(Default)]
pub struct FrameDecoder {
    pending: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, FrameFailure> {
        let mut frames = Vec::new();

        for chunk in bytes.split_inclusive(|&byte| byte == b'\n') {
            let has_lf = chunk.last() == Some(&b'\n');

            let data = if has_lf {
                &chunk[..chunk.len() - 1]
            } else {
                chunk
            };

            if data.len() > MAX_RESPONSE_FRAME_BYTES.saturating_sub(self.pending.len()) {
                return Err(FrameFailure {
                    frames,
                    error: FrameError::FrameTooLong,
                });
            }

            self.pending.extend_from_slice(data);

            if has_lf {
                let frame_bytes = self.pending.strip_suffix(b"\r").unwrap_or(&self.pending);

                if frame_bytes.is_empty() {
                    return Err(FrameFailure {
                        frames,
                        error: FrameError::EmptyFrame,
                    });
                }

                let frame = match std::str::from_utf8(frame_bytes) {
                    Ok(frame) => frame.to_owned(),

                    Err(_) => {
                        return Err(FrameFailure {
                            frames,
                            error: FrameError::InvalidUtf8,
                        });
                    }
                };

                frames.push(frame);
                self.pending.clear();
            }
        }

        Ok(frames)
    }

    pub fn finish(&self) -> Result<(), FrameError> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(FrameError::IncompleteFrame)
        }
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;

    #[test]
    fn joins_partial_frames_and_splits_multiple_frames() {
        let mut decoder = FrameDecoder::default();

        assert!(decoder.push(b"{\"id\":").unwrap().is_empty());

        assert_eq!(
            decoder.push(b"2}\n{\"event\":\"measure").unwrap(),
            vec![r#"{"id":2}"#.to_owned()]
        );

        assert_eq!(
            decoder.push(b"ment\"}\r\n").unwrap(),
            vec![r#"{"event":"measurement"}"#.to_owned()]
        );

        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn preserves_completed_frames_before_error() {
        let mut decoder = FrameDecoder::default();

        let failure = decoder.push(b"{\"id\":1}\n\n").unwrap_err();

        assert_eq!(failure.frames, vec![r#"{"id":1}"#.to_owned()]);

        assert_eq!(failure.error, FrameError::EmptyFrame);
    }

    #[test]
    fn rejects_invalid_utf8_and_oversized_frames() {
        let mut decoder = FrameDecoder::default();

        let failure = decoder.push(&[0xff, b'\n']).unwrap_err();

        assert_eq!(failure.error, FrameError::InvalidUtf8);

        let mut decoder = FrameDecoder::default();

        let oversized = vec![b'x'; MAX_RESPONSE_FRAME_BYTES + 1];

        let failure = decoder.push(&oversized).unwrap_err();

        assert_eq!(failure.error, FrameError::FrameTooLong);
    }

    #[test]
    fn detects_incomplete_frame_at_eof() {
        let mut decoder = FrameDecoder::default();

        assert!(decoder.push(b"{\"id\":42").unwrap().is_empty());

        assert_eq!(decoder.finish(), Err(FrameError::IncompleteFrame));
    }

    #[test]
    fn accepts_frame_at_exact_size_limit() {
        let mut decoder = FrameDecoder::default();

        let mut frame = vec![b'x'; MAX_RESPONSE_FRAME_BYTES];
        frame.push(b'\n');

        let frames = decoder.push(&frame).unwrap();

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), MAX_RESPONSE_FRAME_BYTES);
        assert_eq!(decoder.finish(), Ok(()));
    }
}
