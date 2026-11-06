use lab_plot::tcp_client::*;
use serde_json::{Value, json};

fn measurement(key: &str, value: Value) -> Value {
    json!({"event":"measurement","status":"ok","key":key,"value":value,"unit":"degC","completed_at_unix_ms":123})
}

#[test]
fn scalar_set_encoding_preserves_boolean_and_exact_numeric_types() {
    for (value, expected) in [
        (ScalarValue::Boolean(true), json!(true)),
        (ScalarValue::Boolean(false), json!(false)),
        (ScalarValue::Integer(20), json!(20)),
        (ScalarValue::Float(20.0), json!(20.0)),
        (
            ScalarValue::Integer(9_007_199_254_740_993),
            json!(9_007_199_254_740_993i64),
        ),
        (ScalarValue::Integer(i64::MIN), json!(i64::MIN)),
        (ScalarValue::Integer(i64::MAX), json!(i64::MAX)),
    ] {
        let frame = encode_request(
            2,
            &Request::Set {
                key: "value".into(),
                value,
            },
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(
            parsed,
            json!({"id":2,"op":"set","key":"value","value":expected})
        );
        assert_eq!(parsed["value"].to_string(), expected.to_string());
    }
}

#[test]
fn boolean_and_exact_numeric_get_and_measurement_decoding() {
    for (literal, expected) in [
        ("true", ScalarValue::Boolean(true)),
        ("false", ScalarValue::Boolean(false)),
        ("20", ScalarValue::Integer(20)),
        ("20.0", ScalarValue::Float(20.0)),
        ("2e1", ScalarValue::Float(20.0)),
        (
            "9007199254740993",
            ScalarValue::Integer(9_007_199_254_740_993),
        ),
        ("9223372036854775807", ScalarValue::Integer(i64::MAX)),
        ("-9223372036854775808", ScalarValue::Integer(i64::MIN)),
    ] {
        for prefix in [r#""id":2"#, r#""event":"measurement""#] {
            let raw = format!(
                r#"{{{prefix},"status":"ok","key":"a","value":{literal},"unit":"","completed_at_unix_ms":1234}}"#
            );
            let decoded = match parse_message(&raw).unwrap() {
                Incoming::Measurement(m)
                | Incoming::Response(Response {
                    data: ResponseData::Measurement(m),
                    ..
                }) => m,
                other => panic!("{other:?}"),
            };
            assert_eq!(
                decoded.reading,
                Reading::Value {
                    value: expected,
                    unit: String::new()
                }
            );
            assert_eq!(decoded.completed_at_unix_ms, 1234);
        }
    }
}

#[test]
fn boolean_describe_has_null_bounds_and_empty_unit() {
    let parameter = json!({"key":"enabled","type":"boolean","min":null,"max":null,"unit":"","writable":true,"poll_period_ns":null});
    let message =
        |parameter: Value| json!({"id":2,"status":"ok","parameters":[parameter]}).to_string();
    for period in [Value::Null, json!(10000000)] {
        let mut good = parameter.clone();
        good["poll_period_ns"] = period.clone();
        let Incoming::Response(Response {
            data: ResponseData::Parameters(parameters),
            ..
        }) = parse_message(&message(good)).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            parameters,
            vec![Parameter {
                key: "enabled".into(),
                value_type: ScalarType::Boolean,
                min: None,
                max: None,
                unit: String::new(),
                writable: true,
                poll_period_ns: period.as_u64()
            }]
        );
    }
    for (field, value) in [
        ("type", json!("string")),
        ("min", json!(0)),
        ("max", json!(1)),
        ("min", json!(false)),
        ("unit", json!("bool")),
        ("unit", Value::Null),
    ] {
        let mut bad = parameter.clone();
        bad[field] = value;
        assert!(parse_message(&message(bad)).is_err(), "{field}");
    }
    for field in ["type", "min", "max", "unit", "writable", "poll_period_ns"] {
        let mut bad = parameter.clone();
        bad.as_object_mut().unwrap().remove(field);
        assert!(parse_message(&message(bad)).is_err(), "missing {field}");
    }
    for (kind, min, max, expected) in [
        (
            "integer",
            json!(i64::MIN),
            json!(i64::MAX),
            (Number::Integer(i64::MIN), Number::Integer(i64::MAX)),
        ),
        (
            "float",
            json!(-20.0),
            json!(20.0),
            (Number::Float(-20.0), Number::Float(20.0)),
        ),
    ] {
        let mut good = parameter.clone();
        good["type"] = json!(kind);
        good["min"] = min;
        good["max"] = max;
        let Incoming::Response(Response {
            data: ResponseData::Parameters(parameters),
            ..
        }) = parse_message(&message(good.clone())).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (parameters[0].min, parameters[0].max),
            (Some(expected.0), Some(expected.1))
        );
        for invalid in [Value::Null, json!(true), json!("0")] {
            good["min"] = invalid;
            assert!(parse_message(&message(good.clone())).is_err());
        }
    }
}

#[test]
fn boolean_readings_reject_incompatible_fields_and_unsupported_values() {
    let good = json!({"event":"measurement","status":"ok","key":"enabled","value":true,"unit":"","completed_at_unix_ms":1});
    for (field, value) in [
        ("value", json!("true")),
        ("value", Value::Null),
        ("value", json!([])),
        ("value", json!({})),
        ("unit", json!("bool")),
        ("unit", Value::Null),
        ("error", json!("failed")),
        ("status", json!("error")),
    ] {
        let mut bad = good.clone();
        bad[field] = value;
        assert!(parse_message(&bad.to_string()).is_err(), "{bad}");
    }
    assert!(parse_message(r#"{"id":2,"status":"ok","key":"enabled","value":true,"value":false,"unit":"","completed_at_unix_ms":1}"#).is_err());
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
