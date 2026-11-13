//! Human-readable diagnostic formatting; typed results remain in Integration.
use crate::{
    integration::{JournalEntry, JournalResult},
    plot_display::{measurement_time, scalar_text},
    tcp_client::*,
};
use std::fmt::Write;

fn number(value: Option<Number>) -> String {
    match value {
        Some(Number::Integer(v)) => v.to_string(),
        Some(Number::Float(v)) => v.to_string(),
        None => "none".into(),
    }
}
fn server_error(e: &ServerError) -> String {
    match &e.key {
        Some(key) => format!("Error: {}\n  key: {key}", e.error),
        None => format!("Error: {}", e.error),
    }
}
fn description(parameters: &[Parameter], actions: &[ActionDescription]) -> String {
    let mut text = String::from("describe\nStatus: ok\n\nParameters:\n");
    for p in parameters {
        let kind = match p.value_type {
            ScalarType::Integer => "Integer",
            ScalarType::Float => "Float",
            ScalarType::Boolean => "Boolean",
        };
        let _ = writeln!(
            text,
            "  {}\n    type: {kind}\n    unit: {}\n    writable: {}\n    min: {}\n    max: {}\n    poll_period_ns: {}\n",
            p.key,
            p.unit,
            p.writable,
            number(p.min),
            number(p.max),
            p.poll_period_ns
                .map(|v| v.to_string())
                .unwrap_or_else(|| "none".into())
        );
    }
    text.push_str("Actions:\n");
    for a in actions {
        let _ = writeln!(text, "  {}", a.key);
        for line in a.description.lines() {
            let _ = writeln!(text, "    {line}");
        }
    }
    text
}
fn response(data: &ResponseData) -> String {
    match data {
        ResponseData::Ack => "ACK · Status: ok".into(),
        ResponseData::Hello { protocol, version } => {
            format!("hello · Status: ok\n  protocol: {protocol}\n  version: {version}")
        }
        ResponseData::Parameters(p) => description(p, &[]),
        ResponseData::Description {
            parameters,
            actions,
        } => description(parameters, actions),
        ResponseData::Measurement(m) => {
            let value = match &m.reading {
                Reading::Value { value, unit } => format!(
                    "Status: ok\n  value: {}\n  unit: {unit}",
                    scalar_text(value)
                ),
                Reading::Error(e) => format!("Status: error\n  error: {e}"),
            };
            format!(
                "get · {}\n{value}\n  time: {}\n  completed_at_unix_ms: {}",
                m.key,
                measurement_time(m.completed_at_unix_ms)
                    .unwrap_or_else(|| "invalid timestamp".into()),
                m.completed_at_unix_ms
            )
        }
        ResponseData::Subscriptions(keys) => format!(
            "Subscriptions · Status: ok\n{}",
            keys.iter()
                .map(|k| format!("  {k}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        ResponseData::NoData { key } => format!("get · {key}\nStatus: no_data"),
        ResponseData::Error(e) => format!("Status: error\n{}", server_error(e)),
    }
}
impl JournalEntry {
    pub fn is_error(&self) -> bool {
        match &self.result {
            JournalResult::Response(r) => r.status() == Status::Error,
            JournalResult::RequestFailed { .. } | JournalResult::ServerError(_) => true,
            JournalResult::Message(text) => text != "Ready" && text != "Stopped: Shutdown",
        }
    }
    pub fn text(&self) -> String {
        let time: chrono::DateTime<chrono::Local> = self.received_at.into();
        let mut header = format!(
            "{} · Connection {}",
            time.format("%d.%m.%Y %H:%M:%S%.3f %:z"),
            self.connection
        );
        if let Some(id) = self.request_id() {
            let _ = write!(header, " · Request {id}");
        }
        let body = match &self.result {
            JournalResult::Message(text) => text.clone(),
            JournalResult::Response(r) => response(&r.data),
            JournalResult::ServerError(e) => server_error(e),
            JournalResult::RequestFailed { failure, .. } => match failure {
                RequestFailure::NotSent => "Error: NotSent — request was not sent".into(),
                RequestFailure::OutcomeUnknown => {
                    "Error: OutcomeUnknown — execution may have occurred; no automatic retry".into()
                }
                RequestFailure::ResponseUnavailable => {
                    "Error: ResponseUnavailable — no trustworthy response received".into()
                }
            },
        };
        format!("{header}\n{body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(data: ResponseData) -> JournalEntry {
        JournalEntry {
            connection: 3,
            received_at: std::time::UNIX_EPOCH,
            result: JournalResult::Response(Response { id: 17, data }),
        }
    }
    #[test]
    fn ack_is_short_and_description_is_nested_and_exact() {
        let ack = entry(ResponseData::Ack);
        assert!(!ack.is_error());
        let text = ack.text();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("Connection 3 · Request 17\nACK"));
        let d = entry(ResponseData::Description {
            parameters: vec![Parameter {
                key: "furnace/power".into(),
                value_type: ScalarType::Integer,
                min: Some(Number::Integer(i64::MIN)),
                max: Some(Number::Integer(i64::MAX)),
                unit: "W".into(),
                writable: true,
                poll_period_ns: Some(1000000000),
            }],
            actions: vec![ActionDescription {
                key: "furnace/apply".into(),
                description: "Apply staged\nconfiguration".into(),
            }],
        });
        let text = d.text();
        for part in [
            "Parameters:\n  furnace/power\n    type: Integer",
            "9223372036854775807",
            "Actions:\n  furnace/apply\n    Apply staged\n    configuration",
        ] {
            assert!(text.contains(part), "{text}");
        }
    }
    #[test]
    fn errors_are_explicit_and_marked() {
        let e = entry(ResponseData::Error(ServerError {
            error: "outcome_unknown".into(),
            key: Some("apply".into()),
        }));
        assert!(e.is_error());
        assert!(
            e.text()
                .contains("Status: error\nError: outcome_unknown\n  key: apply")
        );
        let mut e = e;
        e.result = JournalResult::RequestFailed {
            id: 18,
            failure: RequestFailure::OutcomeUnknown,
        };
        assert!(e.is_error());
        assert!(e.text().contains("execution may have occurred"));
    }
}
