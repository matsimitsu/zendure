//! Every battery this process drives, and the loop that reaches the right one.
//!
//! `main.rs` used to own exactly one `ZendureClient` and hand it straight to
//! `device::actuate`. That was fine while there was one box, but it put a
//! ceiling on the whole crate: a second battery had nowhere to live. This
//! module is that ceiling lifted — a registry keyed by [`DeviceId`] and the
//! `actuate` loop rewritten to route through it instead of assuming its one
//! argument is the only adapter that exists.
//!
//! ## Why an enum, not `dyn BatteryController`
//!
//! The obvious shape for "a collection of things that implement a trait" is
//! `Vec<Box<dyn Trait>>`. It does not typecheck here, and would not even if it
//! did.
//!
//! It does not typecheck because [`BatteryController`] is not object-safe.
//! `apply` returns `impl Future<..> + Send` rather than a named type — RPITIT,
//! which cannot appear in a vtable — and `Error` is an associated type with no
//! `dyn`-compatible way to name it across implementors. Both are load-bearing,
//! not incidental: the RPITIT is what lets `ZendureClient::apply` keep an
//! unboxed, statically-known `Send` future (see `device.rs`'s own doc comment
//! on the trait), and the associated `Error` is what lets `ZendureClient` keep
//! `reqwest::Error` and a test double keep `String` instead of every adapter
//! converting into one shared error enum nobody matches on. Reshaping the
//! trait to erase both just to get a `dyn` would undo the reasoning that put
//! them there in the first place.
//!
//! It would not help even reshaped, because a boxed bridge trait — the usual
//! workaround, wrapping each adapter behind a second, object-safe trait that
//! boxes its future — puts a heap allocation on the decision path for every
//! single command. That is exactly the cost `device.rs` argues against.
//!
//! Generics do not solve it either: `actuate<B: BatteryController>` is generic
//! over *one* concrete type per call, so a `Vec` of it can hold many
//! `ZendureClient`s but never a `ZendureClient` and something else at once —
//! and holding more than one *kind* of adapter at a time is the entire
//! requirement a second battery (or, later, another vendor) creates.
//!
//! An enum is left, and it is also the repo's existing answer to "a small,
//! closed set of shapes with different data": `Measurement`, `Directive`,
//! `Command`, `Applied` and `ControlPath` are all enums for the same reason.
//! `Battery` costs one match arm per variant and, unlike a `dyn`, keeps every
//! adapter's future unboxed and known at compile time.
//!
//! ## What `actuate` was doing before this
//!
//! Before this module existed, `actuate` took `&B: BatteryController` — one
//! adapter — and matched a directive's address against `battery.id()`. That
//! "no adapter for that device" arm was not a real routing failure: it was
//! comparing the directive's address against *the only adapter the function
//! had*, so it could only ever prove "this directive is not for the one
//! device we have," never "we do not drive this device." With a real map, the
//! `None` branch below is what that comment always wanted to be — a genuine
//! lookup that can fail for two devices as easily as for one, but for one
//! device fails in exactly the same case it did before.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::allocate::Directive;
use crate::command::Command;
use crate::config::{Config, DeviceConfig};
#[cfg(test)]
use crate::device::RecordingBattery;
use crate::device::{
    AC2400_PLUS, Applied, BatteryController, BatteryMonitor, BatteryReading, BatterySpec,
    ControlPath, Outcome, PollError,
};
use crate::simulation::VirtualBattery;
use crate::world::DeviceId;
use crate::zendure::ZendureClient;

/// One battery adapter, in whichever shape it actually is.
///
/// `Virtual` holds an `Arc` rather than an owned `VirtualBattery`, unlike the
/// other two arms: `run.rs`'s synthetic meter (`source::synthetic`) reads the
/// same battery's `flow()` back into the meter reading it manufactures, which
/// is the whole point of that module — a naive synthetic feed that never
/// looks at the battery again just lies. `from_config`, below, is what shares
/// one clone into this registry and another into the meter task.
pub enum Battery {
    Zendure(ZendureClient),
    Virtual(Arc<VirtualBattery>),
    #[cfg(test)]
    Recording(RecordingBattery),
}

impl Battery {
    /// The Zendure adapter, wired up here rather than in `run.rs`. This is
    /// the only place that vendor is named when building the registry — the
    /// coordinator hands this a host and a serial and gets back an opaque
    /// `Battery`, the same way it already never sees `ZendureClient` once a
    /// directive is routed through `actuate`.
    pub fn zendure(ip: &str, sn: String) -> Self {
        Battery::Zendure(ZendureClient::new(ip, sn))
    }
}

/// Builds the registry `run.rs` drives, from configuration.
///
/// The one place a [`DeviceConfig`] becomes a live adapter — `run.rs` never
/// matches on `DeviceConfig` itself, the same discipline `Battery::zendure`
/// already kept for the vendor name. A virtual device always simulates the
/// one real spec this crate knows (`AC2400_PLUS`): `DeviceConfig::Virtual`
/// carries no rated limits of its own, the same way a real Zendure's rating
/// is a fact about the hardware and not a config knob.
///
/// "Exactly one device" is still enforced upstream, in
/// `config::take_device` — this only ever has the one `DeviceConfig` to
/// convert.
pub fn from_config(config: &Config) -> Devices {
    let battery = match &config.device {
        DeviceConfig::Zendure { ip, sn, .. } => Battery::zendure(ip, sn.clone()),
        DeviceConfig::Virtual {
            id,
            packs,
            soc,
            charge_efficiency,
            discharge_efficiency,
        } => Battery::Virtual(Arc::new(VirtualBattery::new(
            DeviceId::new(id.clone()),
            AC2400_PLUS,
            packs.clone(),
            *soc,
            *charge_efficiency,
            *discharge_efficiency,
        ))),
    };
    Devices::new([battery])
}

/// `Error = String`: each arm's own error type is stringified here, at
/// exactly the boundary `device.rs`'s trait doc already argues for — an
/// adapter keeps `reqwest::Error` or whatever it likes, and the registry is
/// the one place that turns it into the `String` an `Outcome` journals.
impl BatteryController for Battery {
    type Error = String;

    fn id(&self) -> &DeviceId {
        match self {
            Battery::Zendure(client) => client.id(),
            Battery::Virtual(battery) => BatteryController::id(battery.as_ref()),
            #[cfg(test)]
            Battery::Recording(battery) => battery.id(),
        }
    }

    async fn apply(&self, command: &Command) -> Result<(), String> {
        match self {
            Battery::Zendure(client) => client.apply(command).await.map_err(|e| e.to_string()),
            // `VirtualBattery::apply`'s error is `Infallible` — see that
            // impl's own doc comment — so there is nothing to stringify,
            // only the `Ok` to keep.
            Battery::Virtual(battery) => {
                battery.apply(command).await.unwrap();
                Ok(())
            }
            // Already `Result<(), String>` — no conversion needed, and
            // stringifying a `String` again would just clone it.
            #[cfg(test)]
            Battery::Recording(battery) => battery.apply(command).await,
        }
    }
}

/// The read side of the same seam, delegating exactly as `BatteryController`
/// does above: `run.rs` reaches through here for `prepare`/`poll` instead of
/// naming `ZendureClient`, so a second device is one more match arm rather
/// than a second name threaded through the coordinator loop.
///
/// `id` repeats `BatteryController`'s match rather than sharing it through a
/// helper: both traits name a same-shaped `id(&self) -> &DeviceId`, on
/// purpose, so the two capabilities agree on what a device is called — but
/// that means a call site with *both* traits in scope, as this module is, has
/// to say which one it means. `Devices::new` says so with a qualified call;
/// `run.rs` never imports `BatteryController` at all, so its `.id()` is
/// unambiguous and reaches this impl instead.
impl BatteryMonitor for Battery {
    fn id(&self) -> &DeviceId {
        match self {
            Battery::Zendure(client) => client.id(),
            Battery::Virtual(battery) => BatteryMonitor::id(battery.as_ref()),
            #[cfg(test)]
            Battery::Recording(battery) => battery.id(),
        }
    }

    fn spec(&self) -> &BatterySpec {
        match self {
            Battery::Zendure(client) => client.spec(),
            Battery::Virtual(battery) => battery.spec(),
            // `RecordingBattery` is the write-only double `actuate`'s tests
            // share (see its doc comment in `device.rs`) — it answers for a
            // device id and records commands, and was deliberately not
            // burdened with a spec or a read capability it does not need.
            // This constant is never read in production; it exists only so
            // this match is exhaustive in a test build.
            #[cfg(test)]
            Battery::Recording(_) => &AC2400_PLUS,
        }
    }

    async fn prepare(&self) -> Result<BatteryReading, PollError> {
        match self {
            Battery::Zendure(client) => client.prepare().await,
            Battery::Virtual(battery) => battery.prepare().await,
            #[cfg(test)]
            Battery::Recording(battery) => Err(unreadable(battery.id())),
        }
    }

    async fn poll(&self) -> Result<BatteryReading, PollError> {
        match self {
            Battery::Zendure(client) => client.poll().await,
            Battery::Virtual(battery) => battery.poll().await,
            #[cfg(test)]
            Battery::Recording(battery) => Err(unreadable(battery.id())),
        }
    }
}

/// The error a `RecordingBattery` reports for either read call — reachable
/// only if a test asks a write-only double for a reading, which none of
/// `actuate`'s tests today do. An `Err`, not a panic: consistent with the
/// registry's rule (see `actuate`'s "no adapter registered" arm) that a
/// misuse this module can name stays a reported failure rather than one that
/// takes the test binary down with it.
#[cfg(test)]
fn unreadable(id: &DeviceId) -> PollError {
    PollError {
        raw: None,
        error: format!("{id} is a write-only test double and cannot be read"),
    }
}

/// Every battery this process drives, keyed by the id its directives carry.
///
/// A `BTreeMap` rather than a `Vec` for the same reason `World` keeps its
/// devices in one: `batteries()` and `primary()` need a stable, deterministic
/// order (the journal replays a fixed sequence, and `allocate` already relies
/// on `World`'s id order), and a lookup by `DeviceId` should be a real lookup
/// rather than a linear scan re-implemented at every call site.
pub struct Devices {
    batteries: BTreeMap<DeviceId, Battery>,
}

impl Devices {
    /// Keys each battery by its own `id()`, so the registry and the adapter
    /// can never disagree about which slot it lives in.
    ///
    /// Qualified as `BatteryController::id` rather than `battery.id()`: with
    /// both `BatteryController` and `BatteryMonitor` in scope in this module,
    /// plain method-call syntax on a bare `Battery` is ambiguous — either
    /// trait's `id` would do, and picking one here is arbitrary but has to be
    /// written down.
    pub fn new(batteries: impl IntoIterator<Item = Battery>) -> Self {
        Devices {
            batteries: batteries
                .into_iter()
                .map(|battery| (BatteryController::id(&battery).clone(), battery))
                .collect(),
        }
    }

    /// The adapter for one device, or `None` if this process does not drive
    /// it. The genuine routing failure `actuate` below reports as an error
    /// outcome rather than a panic or a silent no-op.
    pub fn battery(&self, id: &DeviceId) -> Option<&Battery> {
        self.batteries.get(id)
    }

    /// Every registered battery, in id order. `allocate` still walks `World`
    /// to decide which devices get a directive, not this — so nothing calls
    /// this yet, the same way `World::batteries` sat unused for one commit
    /// before `allocate` existed. Part of the registry's surface regardless:
    /// a `Devices` with no way to iterate every device it holds would not be
    /// a registry.
    #[allow(dead_code)]
    pub fn batteries(&self) -> impl Iterator<Item = (&DeviceId, &Battery)> {
        self.batteries.iter()
    }

    /// The lowest id, mirroring `World::battery` so the registry and the
    /// world agree on which box is primary. `allocate` sizes a whole-house
    /// setpoint against `world.batteries().next()`; if this ever picked a
    /// different device the setpoint would be sized against one box and
    /// delivered to another.
    pub fn primary(&self) -> Option<(&DeviceId, &Battery)> {
        self.batteries.iter().next()
    }
}

/// Apply every directive, in order, reporting each.
///
/// A failure does not stop the rest: with two devices, one unreachable box
/// must not leave the other uncommanded. That is the whole reason taking only
/// the first command was a bug and not a style point.
///
/// Sequential rather than concurrent on purpose. The commands in one step can
/// depend on each other's order — "stop the car charger, then start charging
/// the battery" must not overlap on a supply that cannot carry both — and the
/// journal's list is worth reading as a sequence.
///
/// The directive's address picks the adapter through `Devices::battery`, a
/// real map lookup rather than a comparison against the one adapter a caller
/// happened to be holding. `Outcome.device` is journalled, and a line reading
/// `{"device":"battery-b","outcome":"ok"}` has to mean that box was written
/// to — handing `battery-b`'s setpoint to `battery-a`'s adapter and recording
/// it as b's success is the failure mode this exists to make impossible.
pub async fn actuate(
    devices: &Devices,
    directives: &[Directive],
    what: ControlPath,
) -> Vec<Outcome> {
    let mut outcomes = Vec::with_capacity(directives.len());

    for directive in directives {
        // Stringified inside the arm that produced it, so `Outcome` stays one
        // concrete type across adapters with unrelated error types — and each
        // class logs the noun its operators would grep for.
        let result = match directive {
            Directive::Battery { device, command } => match devices.battery(device) {
                Some(battery) => battery.apply(command).await.map_err(|e| {
                    tracing::error!("Failed to apply {what} to battery {device}: {e}");
                    e
                }),
                // Unroutable, not applied: an allocation named a device this
                // registry does not hold. Reported as a failed outcome — so
                // the journal shows the command never landed and the
                // caller's status goes degraded — rather than dropped
                // silently or sent to whoever happens to be the primary.
                None => {
                    tracing::error!(
                        "Cannot apply {what} to {device}: no adapter registered for that device",
                    );
                    Err(format!("no adapter registered for device {device}"))
                }
            },
        };

        outcomes.push(Outcome {
            device: directive.device().clone(),
            command: directive.describe(),
            applied: if result.is_ok() {
                Applied::Ok
            } else {
                Applied::Error
            },
            error: result.err(),
        });
    }

    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::Setpoint;

    fn directive(device: &str, command: Command) -> Directive {
        Directive::Battery {
            device: DeviceId::new(device),
            command,
        }
    }

    #[tokio::test]
    async fn applies_every_directive_in_order() {
        let battery = RecordingBattery::new("battery-a");
        let devices = Devices::new([Battery::Recording(battery)]);
        let directives = [
            directive("battery-a", Command::SetIdle),
            directive("battery-a", Command::SetCharge(Setpoint::new(1200))),
        ];

        let outcomes = actuate(&devices, &directives, ControlPath::Objective).await;

        let Some(Battery::Recording(battery)) = devices.battery(&DeviceId::new("battery-a")) else {
            panic!("battery-a must be registered");
        };
        assert_eq!(
            battery.applied(),
            vec![Command::SetIdle, Command::SetCharge(Setpoint::new(1200))],
        );
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].device, DeviceId::new("battery-a"));
        assert_eq!(outcomes[0].command, "set_idle");
        assert_eq!(outcomes[0].applied, Applied::Ok);
        assert_eq!(outcomes[1].device, DeviceId::new("battery-a"));
        assert_eq!(outcomes[1].command, "set_charge(1200W)");
        assert_eq!(outcomes[1].applied, Applied::Ok);
    }

    /// An unreachable box must not leave the rest uncommanded — the failure is
    /// recorded and the loop carries on.
    #[tokio::test]
    async fn a_failure_does_not_stop_the_rest() {
        let battery = RecordingBattery::failing_at("battery-a", 0);
        let devices = Devices::new([Battery::Recording(battery)]);
        let directives = [
            directive("battery-a", Command::SetIdle),
            directive("battery-a", Command::SetCharge(Setpoint::new(1200))),
        ];

        let outcomes = actuate(&devices, &directives, ControlPath::Objective).await;

        let Some(Battery::Recording(battery)) = devices.battery(&DeviceId::new("battery-a")) else {
            panic!("battery-a must be registered");
        };
        assert_eq!(
            battery.applied(),
            vec![Command::SetIdle, Command::SetCharge(Setpoint::new(1200))],
        );
        assert_eq!(outcomes[0].applied, Applied::Error);
        assert_eq!(outcomes[0].error.as_deref(), Some("device unreachable"));
        assert_eq!(outcomes[1].applied, Applied::Ok);
        assert_eq!(outcomes[1].error, None);
    }

    /// The routing half: a directive for a device this process does not drive
    /// must reach no adapter and must be reported as a failure. The owned
    /// directive behind it still lands, for the same reason an unreachable
    /// box does not stop the rest.
    #[tokio::test]
    async fn a_directive_for_another_device_is_an_error_and_is_not_applied() {
        let battery = RecordingBattery::new("battery-a");
        let devices = Devices::new([Battery::Recording(battery)]);
        let directives = [
            directive("battery-b", Command::SetCharge(Setpoint::new(1200))),
            directive("battery-a", Command::SetIdle),
        ];

        let outcomes = actuate(&devices, &directives, ControlPath::Objective).await;

        let Some(Battery::Recording(battery)) = devices.battery(&DeviceId::new("battery-a")) else {
            panic!("battery-a must be registered");
        };
        assert_eq!(battery.applied(), vec![Command::SetIdle]);
        assert_eq!(outcomes[0].device, DeviceId::new("battery-b"));
        assert_eq!(outcomes[0].applied, Applied::Error);
        assert_eq!(
            outcomes[0].error.as_deref(),
            Some("no adapter registered for device battery-b"),
        );
        assert_eq!(outcomes[1].device, DeviceId::new("battery-a"));
        assert_eq!(outcomes[1].applied, Applied::Ok);
    }

    /// The point of the change: with two adapters registered, a directive
    /// naming one reaches only that one. Impossible to even write against the
    /// old `actuate<B: BatteryController>`, which could hold exactly one
    /// adapter — this is the test that could not exist before the map did.
    #[tokio::test]
    async fn a_directive_reaches_the_device_it_names_and_the_other_is_untouched() {
        let a = RecordingBattery::new("battery-a");
        let b = RecordingBattery::new("battery-b");
        let devices = Devices::new([Battery::Recording(a), Battery::Recording(b)]);
        let directives = [directive(
            "battery-b",
            Command::SetCharge(Setpoint::new(900)),
        )];

        let outcomes = actuate(&devices, &directives, ControlPath::Objective).await;

        let Some(Battery::Recording(a)) = devices.battery(&DeviceId::new("battery-a")) else {
            panic!("battery-a must be registered");
        };
        let Some(Battery::Recording(b)) = devices.battery(&DeviceId::new("battery-b")) else {
            panic!("battery-b must be registered");
        };
        assert_eq!(
            a.applied(),
            Vec::new(),
            "the device not addressed took nothing"
        );
        assert_eq!(b.applied(), vec![Command::SetCharge(Setpoint::new(900))]);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].device, DeviceId::new("battery-b"));
        assert_eq!(outcomes[0].applied, Applied::Ok);
    }

    /// A directive naming a device the registry does not hold at all — not
    /// even under a different adapter — is an error outcome naming that
    /// device, and nothing anywhere is applied.
    #[tokio::test]
    async fn a_directive_for_an_unregistered_device_applies_nowhere() {
        let a = RecordingBattery::new("battery-a");
        let b = RecordingBattery::new("battery-b");
        let devices = Devices::new([Battery::Recording(a), Battery::Recording(b)]);
        let directives = [directive("battery-c", Command::SetIdle)];

        let outcomes = actuate(&devices, &directives, ControlPath::Objective).await;

        let Some(Battery::Recording(a)) = devices.battery(&DeviceId::new("battery-a")) else {
            panic!("battery-a must be registered");
        };
        let Some(Battery::Recording(b)) = devices.battery(&DeviceId::new("battery-b")) else {
            panic!("battery-b must be registered");
        };
        assert_eq!(a.applied(), Vec::new());
        assert_eq!(b.applied(), Vec::new());
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].device, DeviceId::new("battery-c"));
        assert_eq!(outcomes[0].applied, Applied::Error);
        assert_eq!(
            outcomes[0].error.as_deref(),
            Some("no adapter registered for device battery-c"),
        );
    }

    /// `primary()` picks the lowest id, matching `World::battery`'s rule —
    /// registered out of order here so passing means the map sorted them,
    /// not that they happened to be inserted in the right order.
    #[test]
    fn primary_is_the_lowest_id() {
        let devices = Devices::new([
            Battery::Recording(RecordingBattery::new("zzz")),
            Battery::Recording(RecordingBattery::new("aaa")),
            Battery::Recording(RecordingBattery::new("mmm")),
        ]);

        let (id, _) = devices.primary().expect("three batteries were registered");
        assert_eq!(id, &DeviceId::new("aaa"));
    }
}
