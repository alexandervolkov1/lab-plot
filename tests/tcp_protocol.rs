use lab_plot::tcp_client::*;
use serde_json::{Value, json};

fn measurement(key: &str, value: Value) -> Value {
    json!({"event":"measurement","status":"ok","key":key,"value":value,"unit":"degC","completed_at_unix_ms":123})
}
#[test]
fn strict_parser_rejects_malformed_combinations_and_preserves_number_types() {
    let good = measurement("a", json!(2));
    let Incoming::Measurement(parsed) = parse_message(&good.to_string()).unwrap() else {
        panic!()
    };
    assert_eq!(
        parsed.reading,
        Reading::Value {
            value: ScalarValue::Integer(2),
            unit: "degC".into()
        }
    );
    for value in ["2.0", "2e0"] {
        let raw = format!(
            "{{\"event\":\"measurement\",\"status\":\"ok\",\"key\":\"a\",\"value\":{value},\"unit\":\"x\",\"completed_at_unix_ms\":0}}"
        );
        assert!(matches!(
            parse_message(&raw).unwrap(),
            Incoming::Measurement(Measurement {
                reading: Reading::Value {
                    value: ScalarValue::Float(2.0),
                    ..
                },
                ..
            })
        ));
    }
    for field in ["key", "unit", "value", "completed_at_unix_ms", "status"] {
        let mut bad = good.clone();
        bad.as_object_mut().unwrap().remove(field);
        assert!(parse_message(&bad.to_string()).is_err(), "{field}");
    }
    for (field, value) in [
        ("id", json!(2)),
        ("error", json!("bad")),
        ("unit", Value::Null),
        ("completed_at_unix_ms", json!(-1)),
        ("status", json!("no_data")),
        ("value", json!("2")),
    ] {
        let mut bad = good.clone();
        bad[field] = value;
        assert!(parse_message(&bad.to_string()).is_err(), "{field}");
    }
    for bad in [
        r#"{"id":1,"id":2,"status":"ok"}"#,
        r#"{"id":0,"status":"ok"}"#,
        r#"{"id":2,"status":"no_data","key":"a","value":1}"#,
        r#"{"id":2,"status":"error","error":"bad","unit":null}"#,
        r#"{"status":"error","error":"bad","completed_at_unix_ms":2}"#,
        r#"{"id":2,"status":"ok","parameters":[{"key":"p","key":"q"}]}"#,
        r#"{"event":"measurement","status":"error","key":"a","error":"bad"}"#,
        r#"{"event":"measurement","status":"ok","key":"a","value":9223372036854775808,"unit":"x","completed_at_unix_ms":1}"#,
        r#"{"event":"measurement","status":"ok","key":"a","value":1e9999,"unit":"x","completed_at_unix_ms":1}"#,
    ] {
        assert!(parse_message(bad).is_err(), "{bad}");
    }
    assert!(matches!(parse_message(r#"{"event":"measurement","status":"error","key":"a","error":"Empty response","completed_at_unix_ms":1}"#).unwrap(), Incoming::Measurement(Measurement { reading: Reading::Error(_), .. })));
}
