//! Every battery this process drives, and the loop that reaches the right one.
//!
//! `Vec<Box<dyn BatteryController>>` does not compile: the trait is not
//! object-safe (`apply` returns `impl Future + Send`, RPITIT can't appear in
//! a vtable, and `Error` has no `dyn`-compatible spelling). Both are
//! load-bearing — RPITIT keeps `ZendureClient::apply`'s future unboxed, and
//! the associated `Error` lets each adapter keep its own type — so fixing
//! that costs a heap allocation per command. Generics don't help either: one concrete
//! type per call can't hold two adapter kinds at once. Hence an enum.

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

/// One battery adapter, in whichever shape it actually is. `Virtual` holds
/// an `Arc`, not an owned `VirtualBattery`: `run.rs`'s synthetic meter reads
/// the same battery's `flow()` back into the reading it manufactures, so
/// `from_config` shares one clone into this registry and another into the meter task.
pub enum Battery {
    Zendure(ZendureClient),
    Virtual(Arc<VirtualBattery>),
    #[cfg(test)]
    Recording(RecordingBattery),
}

impl Battery {
    /// The only place that names the Zendure vendor when building the registry:
    /// the coordinator hands this a host and serial and gets back an opaque
    /// `Battery`, the same way it never sees `ZendureClient` once a directive is
    /// routed through `actuate`.
    pub fn zendure(ip: &str, sn: String) -> Self {
        Battery::Zendure(ZendureClient::new(ip, sn))
    }
}

/// Builds the registry `run.rs` drives. The only place a [`DeviceConfig`]
/// becomes a live adapter — `run.rs` never matches on it directly. A virtual
/// device always simulates `AC2400_PLUS`, since `DeviceConfig::Virtual`
/// carries no rated limits of its own. "Exactly one device" is enforced upstream in
/// `config::take_device`.
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

/// The read side of the same seam: `run.rs` reaches through here for
/// `prepare`/`poll` instead of naming `ZendureClient`. `id` repeats
/// `BatteryController`'s match rather than sharing it, since both traits
/// share `id(&self) -> &DeviceId` on purpose — a call site with both in scope (like
/// `Devices::new`) must disambiguate with a qualified call.
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
            // `RecordingBattery` is a write-only test double, deliberately not given a
            // spec or read capability. This constant is never read in production — it
            // exists only so this match stays exhaustive in a test build.
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
/// only if a test asks a write-only double for a reading. An `Err`, not a
/// panic: consistent with the registry's rule that a nameable misuse stays a
/// reported failure rather than one that takes the process down.
#[cfg(test)]
fn unreadable(id: &DeviceId) -> PollError {
    PollError {
        raw: None,
        error: format!("{id} is a write-only test double and cannot be read"),
    }
}

/// Every battery this process drives, keyed by the id its directives carry.
/// A `BTreeMap`, not a `Vec`: `primary()` needs the stable, deterministic
/// order the journal replay and `allocate` rely on, and a lookup by
/// `DeviceId` should be real, not a linear scan.
pub struct Devices {
    batteries: BTreeMap<DeviceId, Battery>,
}

impl Devices {
    /// Keys each battery by its own `id()`, so the registry and adapter can
    /// never disagree about which slot it lives in. Qualified as
    /// `BatteryController::id`, not `battery.id()`: with both `BatteryController`
    /// and `BatteryMonitor` in scope, plain method syntax is ambiguous.
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

    /// The lowest id, mirroring `World::battery` so the registry and world agree
    /// on which box is primary. `allocate` sizes a whole-house setpoint against
    /// `world.batteries().next()` — if this ever disagreed, the setpoint would
    /// be sized against one box and delivered to another.
    pub fn primary(&self) -> Option<(&DeviceId, &Battery)> {
        self.batteries.iter().next()
    }
}

/// Applies every directive in order, reporting each; a failure doesn't stop
/// the rest. Sequential, not concurrent — commands in one step can depend on
/// order (stop the car charger, then start the battery, must not overlap on
/// one supply). `Outcome.device` is journalled, so a line naming a device means that
/// box was actually written to.
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
                // Unroutable: an allocation named a device this registry does not hold.
                // Reported as a failed outcome, so the journal shows the command never
                // landed and the caller's status goes degraded — rather than dropped
                // silently or sent to whoever happens to be primary.
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
