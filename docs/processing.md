# Processing components from Steel

Requires the `components` API from lab-server's processing branch. The existing
`scripts/arduino-start.scm` remains unchanged. Connect with the usual `lab/connect!`
and wait for Ready before using these operations. They wait for the server reply;
errors raise Steel errors, and there are no automatic mutation retries.

| Function | Arguments |
| --- | --- |
| `lab/component-types` | none |
| `lab/components` | none; first page of instances |
| `lab/outputs` | none |
| `lab/component-create!` | configuration JSON string, expected revision |
| `lab/component-configure!` | full configuration JSON string, expected revision |
| `lab/component-delete!` | name, cascade Boolean, expected revision |
| `lab/controller-bind!` | name, target key, timeout ms, expected revision |
| `lab/controller-state!` | name, state string, expected revision |
| `lab/component!` | generic command JSON string, including pagination |

Results preserve descriptor data as association lists with string keys; JSON arrays
become lists, null becomes an empty list, and numbers/booleans retain their types.
The client contains no switch on concrete component types.

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

Deletion with active subscribers terminates those streams explicitly. Unsubscribe first
if retaining the connection is desired; then use `lab/component-delete!` with the current
revision. Cascade also stops/removes dependent controllers. Reconnection does not restart
controllers and does not replay earlier measurements.

Validation on this branch: Debug/Release builds, fmt, check --all-targets,
Clippy --all-targets -- -D warnings and git diff --check passed. Each full test
profile passed 143 tests, with one pre-existing external-server test ignored.
The opt-in `tests/server_contract/Cargo.toml` suite passed all three tests against
an in-process server, including real Arduino protocol parsing through EMA into
PlotState. The new pipeline test passed on three additional runs. No serial port
was opened. Four existing GUI/Steel tests hit their time limits during a simultaneous
server/client validation run; the complete isolated rerun passed without changing
those tests or their deadlines.
