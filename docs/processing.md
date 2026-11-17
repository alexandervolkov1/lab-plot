# Processing components from Steel

Requires lab-server's `components` API with `parameter_set` support. Connect with the usual `lab/connect!`
and wait for Ready before using these operations. They wait for the server reply;
errors raise Steel errors, and there are no automatic mutation retries.

| Function | Arguments |
| --- | --- |
| `lab/component-types` | none |
| `lab/components` | none; first page of instances |
| `lab/outputs` | none |
| `lab/component-create!` | configuration JSON string, expected revision |
| `lab/component-configure!` | full configuration JSON string, expected revision |
| `lab/parameter-set!` | component name, parameter name, scalar value; no revision read |
| `lab/component-delete!` | name, cascade Boolean, expected revision |
| `lab/controller-bind!` | name, target key, timeout ms, expected revision |
| `lab/controller-state!` | name, state string, expected revision |
| `lab/component!` | generic command JSON string, including pagination |

Results preserve descriptor data as association lists with string keys; JSON arrays
become lists, null becomes an empty list, and numbers/booleans retain their types.
The client contains no switch on concrete component types.

`lab/parameter-set!` sends one `components/parameter_set` request and waits for its
snapshot response. It keeps no local configuration copy. Integer (including i64
values beyond exact f64 precision), finite Float, Boolean and Choice strings retain
their JSON types; unsupported values are rejected locally. The server validates the
parameter name, type, range and cross-parameter constraints atomically. Errors raise
Steel errors and leave the configuration/revision unchanged. Success increments the
revision once and preserves all other fields and subscriptions. A running EMA keeps
its state when tau changes. PID retuning requires leaving Auto first; no operation
implicitly enables Auto or writes physical output.

```scheme
(lab/parameter-set! "loop" "setpoint" 35.0)
(lab/parameter-set! "loop" "kp" 3.0)
(lab/parameter-set! "loop" "ki" 0.2)
(lab/parameter-set! "smooth" "tau" 5.0)
(define (setpoint! value) (lab/parameter-set! "loop" "setpoint" value))

(plot/add! "Setpoint" "degC")
(plot/bind! "Setpoint" "processing/loop/setpoint")
(lab/subscribe! '("processing/loop/setpoint"))
```

Setpoint inherits the input unit and is published in Shadow and Auto with the PID
measurement time, starting with the next successful input calculation after a change.
Before that calculation, get returns the last sample. kp/ki/kd remain configuration
parameters; p/i/d are existing diagnostics. Optional CAS is available through
`lab/component!`, for example a JSON command with `action:"parameter_set"`, `name`,
`parameter`, `value` and `expected_revision`. Omitting the revision (or null) updates
the current server configuration under its lock; a supplied stale revision fails.

This example assumes a fresh server with revision 0; otherwise read `lab/components`
and use its revision. A concurrent edit requires reading the revision again, not retrying
the same mutation blindly.

```scheme
(lab/component-types)
(lab/components)

(lab/component-create!
  "{\"name\":\"smooth\",\"type_id\":\"ema\",\"inputs\":{\"input\":\"furnace1/sample_temperature\"},\"parameters\":{\"tau\":2.0},\"max_age_ms\":5000}"
  0)

(plot/add! "Smoothed sample" "degC")
(plot/bind! "Smoothed sample" "processing/smooth/value")
(lab/subscribe! '("processing/smooth/value"))
(lab/get "processing/smooth/value")

;; A full configuration replacement; EMA history is retained for a tau change.
(lab/component-configure!
  "{\"name\":\"smooth\",\"type_id\":\"ema\",\"inputs\":{\"input\":\"furnace1/sample_temperature\"},\"parameters\":{\"tau\":5.0},\"max_age_ms\":5000}"
  1)

;; Shadow calculation only: this creates no physical output authority.
(lab/component-create!
  "{\"name\":\"loop\",\"type_id\":\"pid\",\"inputs\":{\"input\":\"processing/smooth/value\"},\"parameters\":{\"setpoint\":40.0,\"kp\":2.0,\"ki\":0.1,\"kd\":0.0,\"output_min\":0.0,\"output_max\":100.0,\"max_gap\":5.0},\"max_age_ms\":5000}"
  2)
(lab/subscribe! '("processing/loop/output" "processing/loop/p"
                  "processing/loop/i" "processing/loop/d"))
(lab/component! "{\"action\":\"list\",\"offset\":0,\"limit\":16}")
(lab/outputs)
```

For an explicitly authorized hardware session, the separate operations are:
`(lab/controller-bind! "loop" "furnace1/power" 1000 3)`, then
`(lab/controller-state! "loop" "auto" 4)` after fresh input is available.
`(lab/controller-state! "loop" "stopped" 5)` releases authority without issuing
a physical zero-power command. These operations were tested only with memory-backed I/O.

After data loss a controller remains faulted until an explicit shadow restart, then
an independent Auto request. Outcome unknown blocks Auto for that server lifetime;
manual requests retain their explicit operator semantics. Inspect `lab/outputs`.

`(lab/components)` now includes `binding` on every instance. A controller without a
binding (and a filter) returns null, represented as an empty Steel list. A binding
is an association list such as `(("target" "furnace1/power") ("command_timeout_ms" 1000))`;
it remains visible in Shadow, Stopped and Fault. Another connected client can inspect
the same binding. Every mutation snapshot uses the same format. Recreating a name
does not inherit the deleted incarnation's binding.

To inspect the complete snapshot after binding, including its current revision:

```scheme
(lab/components)
;; Additional pages, when next_offset is non-null:
(lab/component! "{\"action\":\"list\",\"offset\":16,\"limit\":16}")
```

Changing a component's inputs closes affected subscriptions with
`observation_stream_closed`, including descendants and a rewire within one physical
port. Reconnect and subscribe again to begin the new history. Retuning parameters
alone preserves subscriptions; independent signals continue normally.

Deletion with active subscribers terminates those streams explicitly. Unsubscribe first
if retaining the connection is desired; then use `lab/component-delete!` with the current
revision. Cascade also stops/removes dependent controllers. Reconnection does not restart
controllers and does not replay earlier measurements.

Validation uses memory-backed I/O and loopback TCP/WS; no serial port is opened.
Run the main suite with `RUST_TEST_THREADS=2` to limit concurrent Steel startup:

```powershell
$env:RUST_TEST_THREADS = '2'
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --release
# Additional contract with the sibling lab-server checkout:
$env:CARGO_TARGET_DIR = (Join-Path (Get-Location) 'target')
cargo test --manifest-path tests/server_contract/Cargo.toml
cargo test --release --manifest-path tests/server_contract/Cargo.toml
```

The contract covers Arduino protocol parsing through EMA and PID into PlotState,
including a parameter_set update of the setpoint, its unit and timestamp. It also
covers Managed output authority, Manual/Auto and shutdown over TCP and WS. The main
suite checks one-request Steel parameter updates (Integer/Float/Boolean/Choice),
server errors and hidden/reopened REPL state. One existing smoke test needs an
external server and stays ignored by default.
