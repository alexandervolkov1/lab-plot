//! Steel controls requests and curves; never executes per-measurement callbacks.
//!
//! Run `(lab/connect! "127.0.0.1:7240")` first. It acknowledges admission, not
//! connection success. In a later REPL submission check `(lab/status)` for
//! `"Ready"`, then configure and subscribe (no busy-wait loop is necessary):
//! ```scheme
//! (plot/add! "Heater" "degC")
//! (plot/add! "Sample" "degC")
//! (plot/bind! "Heater" "heater_temperature")
//! (plot/bind! "Sample" "sample_temperature")
//! (lab/subscribe! '("heater_temperature" "sample_temperature"))
//! (lab/get "heater_temperature")
//! (lab/set! "power" 20) ; Integer, unlike 20.0 (Float)
//! (lab/describe) ; Parameters and available actions appear in the GUI journal
//! (lab/invoke-action! "furnace1/apply")
//! ```
//! `lab/connect!` also accepts `tcp://127.0.0.1:7240`,
//! `ws://127.0.0.1:7241/application/v1` and `wss://example.org/application/v1`.
//! No arity or return-value change: `#t` means admission, `Ready` means hello passed.
//! Set `LAB_PLOT_WS_ORIGIN` and/or `LAB_PLOT_WS_HEADERS` before launching the app.
//! Headers are a JSON object, e.g. `{"Authorization":"Bearer <secret>"}`; obtain
//! real secrets through your launcher/secret manager, never Steel source or URLs.
//! These settings apply only to WS/WSS; the subprotocol is fixed automatically.
//! Plain `ws://` is restricted to loopback (127/8, ::1, or names resolving there).
//! Remote WebSocket servers require `wss://`, even without custom headers.
//! Redirects are not followed. WSS uses WebPKI roots and validates the server name.
//! Rust callers can add private trust anchors via `WebSocketOptions::trust_certificate`.
//! The Steel interface has no certificate-verification bypass or custom CA setting.
//!
//! Network commands return positive request IDs, scoped to the connection.
//! The GUI retains the last 200 results/errors and counts evicted entries.
//! `outcome_unknown`/`OutcomeUnknown` must never be interpreted as non-execution.
//! `(lab/set! "enabled" #t)` / `#f` send Boolean without numeric conversion.
//!
//! `(plot/unbind! "Heater")` keeps history; `(plot/remove! "Heater")` removes
//! the curve. Neither changes subscriptions. Use
//! `(lab/unsubscribe! '("heater_temperature"))` to stop receiving that key.
//! ACKs update the full subscription set; events queued before ACK remain valid.
//! `(plot/point! "manual" 1.0 20.0)` and `(plot/clear! "manual")` retain their
//! manual plotting semantics. Plot controls return admission, with application
//! errors shown in the GUI. The 256-command control queue rejects overflow.
//!
//! Replace a connection using `(lab/disconnect!)`, wait for `"Stopped"`, then
//! call `lab/connect!` again. Bindings/history survive; subscriptions do not.
use crate::{
    integration::ControlSender,
    plots::PlotCommand,
    tcp_client::{Endpoint, Request, ScalarValue},
};
use steel::{
    SteelVal,
    steel_vm::{engine::Engine, register_fn::RegisterFn},
};

fn name(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("Series name cannot be empty".into());
    }
    text_limit(value)
}
fn text_limit(value: &str) -> Result<(), String> {
    if value.len() > 16 * 1024 {
        Err("Argument exceeds 16 KiB".into())
    } else {
        Ok(())
    }
}
/// Preserve Integer, Float and Boolean without implicit conversions.
pub fn set_value(value: &SteelVal) -> Result<ScalarValue, String> {
    match value {
        SteelVal::IntV(value) => Ok(ScalarValue::Integer(*value as i64)),
        SteelVal::BigNum(value) => value
            .to_string()
            .parse::<i64>()
            .map(ScalarValue::Integer)
            .map_err(|_| "Integer outside i64 range".into()),
        SteelVal::NumV(value) if value.is_finite() => Ok(ScalarValue::Float(*value)),
        SteelVal::BoolV(value) => Ok(ScalarValue::Boolean(*value)),
        _ => Err(
            "Expected an i64 Integer, finite Float or Boolean; this value type is unsupported"
                .into(),
        ),
    }
}

// Steel 0.8.3's IntoSteelVal for u64 uses `as isize`; preserve the full TCP ID.
fn request_id(id: u64) -> SteelVal {
    match isize::try_from(id) {
        Ok(id) => SteelVal::IntV(id),
        Err(_) => SteelVal::BigNum(steel::gc::Gc::new(id.into())),
    }
}

pub fn register_plot_functions(engine: &mut Engine, sender: ControlSender) {
    let tx = sender.clone();
    engine.register_fn(
        "plot/add!",
        move |curve: String, unit: String| -> Result<bool, String> {
            name(&curve)?;
            text_limit(&unit)?;
            tx.plot(PlotCommand::Add { name: curve, unit })
        },
    );
    let tx = sender.clone();
    engine.register_fn(
        "plot/point!",
        move |curve: String, time: f64, value: f64| -> Result<bool, String> {
            name(&curve)?;
            if !time.is_finite() || !value.is_finite() {
                return Err("Time and value must be finite".into());
            }
            tx.plot(PlotCommand::Point {
                name: curve,
                time,
                value,
            })
        },
    );
    let tx = sender.clone();
    engine.register_fn(
        "plot/clear!",
        move |curve: String| -> Result<bool, String> {
            name(&curve)?;
            tx.plot(PlotCommand::Clear { name: curve })
        },
    );
    let tx = sender.clone();
    engine.register_fn(
        "plot/bind!",
        move |curve: String, key: String| -> Result<bool, String> {
            name(&curve)?;
            text_limit(&key)?;
            tx.plot(PlotCommand::Bind { name: curve, key })
        },
    );
    let tx = sender.clone();
    engine.register_fn(
        "plot/unbind!",
        move |curve: String| -> Result<bool, String> {
            name(&curve)?;
            tx.plot(PlotCommand::Unbind { name: curve })
        },
    );
    engine.register_fn(
        "plot/remove!",
        move |curve: String| -> Result<bool, String> {
            name(&curve)?;
            sender.plot(PlotCommand::Remove { name: curve })
        },
    );
}

pub fn register_lab_functions(engine: &mut Engine, sender: ControlSender) {
    let tx = sender.clone();
    engine.register_fn(
        "lab/connect!",
        move |address: String| -> Result<bool, String> {
            let endpoint = address
                .parse::<Endpoint>()
                .and_then(Endpoint::with_environment)
                .map_err(|_| {
                    "Invalid endpoint or WebSocket environment configuration".to_string()
                })?;
            tx.connect_endpoint(endpoint)
        },
    );
    let tx = sender.clone();
    engine.register_fn("lab/disconnect!", move || tx.disconnect());
    let tx = sender.clone();
    engine.register_fn("lab/status", move || tx.status().name().to_owned());
    let tx = sender.clone();
    engine.register_fn("lab/subscribe!", move |keys: Vec<String>| {
        tx.request(Request::Subscribe { keys }).map(request_id)
    });
    let tx = sender.clone();
    engine.register_fn("lab/unsubscribe!", move |keys: Vec<String>| {
        tx.request(Request::Unsubscribe { keys }).map(request_id)
    });
    let tx = sender.clone();
    engine.register_fn("lab/get", move |key: String| {
        tx.request(Request::Get { key }).map(request_id)
    });
    let tx = sender.clone();
    engine.register_fn("lab/describe", move || {
        tx.request(Request::Describe).map(request_id)
    });
    let tx = sender.clone();
    engine.register_fn("lab/invoke-action!", move |key: String| {
        tx.request(Request::InvokeAction { key }).map(request_id)
    });
    engine.register_fn(
        "lab/set!",
        move |key: String, value: SteelVal| -> Result<SteelVal, String> {
            sender
                .request(Request::Set {
                    key,
                    value: set_value(&value)?,
                })
                .map(request_id)
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tcp_ids_remain_positive_and_exact_across_machine_integer_boundary() {
        for id in [1, i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
            assert_eq!(request_id(id).to_string(), id.to_string());
        }
    }
}
