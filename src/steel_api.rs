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
//! ```
//! Network commands return positive request IDs, scoped to the connection.
//! The GUI retains the last 200 results/errors and counts evicted entries.
//! `outcome_unknown`/`OutcomeUnknown` must never be interpreted as non-execution.
//! `(lab/set! "enabled" #t)` recognizes Boolean but rejects it in protocol v1.
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
    tcp_client::{Number, Request},
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
/// Keeps the universal set signature ready for future protocol value types.
/// Boolean is recognized explicitly, never coerced to a numeric value.
pub fn set_number(value: &SteelVal) -> Result<Number, String> {
    match value {
        SteelVal::IntV(value) => Ok(Number::Integer(*value as i64)),
        SteelVal::BigNum(value) => value
            .to_string()
            .parse::<i64>()
            .map(Number::Integer)
            .map_err(|_| "Integer outside i64 range".into()),
        SteelVal::NumV(value) if value.is_finite() => Ok(Number::Float(*value)),
        SteelVal::BoolV(_) => Err(
            "Boolean is recognized but is not supported by lab-server protocol v1; nothing sent"
                .into(),
        ),
        _ => Err("Expected an i64 Integer or finite Float; this value type is unsupported".into()),
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
            tx.connect(address.parse().map_err(|_| {
                "Expected a numeric IP:port (for example 127.0.0.1:7240)".to_string()
            })?)
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
    engine.register_fn(
        "lab/set!",
        move |key: String, value: SteelVal| -> Result<SteelVal, String> {
            sender
                .request(Request::Set {
                    key,
                    value: set_number(&value)?,
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
