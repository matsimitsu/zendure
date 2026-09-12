//! What a device *is*, and how a command reaches one.
//!
//! Two halves that belong together. The rated limits used to live as consts in
//! `models.rs` next to the wire types, which put a fact about the hardware in
//! the same file as the JSON it happens to be written with. They belong here:
//! one place to name a model, so a second one (a different Zendure, or another
//! vendor entirely) is a new `BatterySpec` rather than another pair of consts
//! to keep in sync.
//!
//! The other half is the capability trait a device adapter implements and the
//! loop that drives it. `main.rs` used to reach straight for `ZendureClient`
//! and apply `commands.first()`, silently dropping the rest — harmless with one
//! device, wrong the moment a step means "stop charging the car, start charging
//! the battery". `actuate` takes the whole list and reports on every element,
//! so there is no longer a place for a caller to decide on its own how many
//! devices there are.

use std::future::Future;

use serde::Serialize;

use crate::allocate::Directive;
use crate::command::Command;
use crate::units::PowerCap;
use crate::world::DeviceId;

/// A battery model's rated limits. What the hardware can do, as distinct from
/// what it currently reports it will accept — the second is a `BatteryState`
/// measurement, the first is a fact about the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatterySpec {
    pub max_charge_power: PowerCap,
    pub max_discharge_power: PowerCap,
}

/// Zendure solarFlow2400AC+. 800 W out is Germany's feed-in limit, stored on
/// the device as the read/write `inverseMaxPower` setpoint; 2400 W in is the
/// `chargeMaxLimit` setpoint. Both can be reset to 0 by the device, which is
/// why the controller writes them back at startup and falls back to them when
/// the device omits the field.
pub const AC2400_PLUS: BatterySpec = BatterySpec {
    max_charge_power: PowerCap::new(2400),
    max_discharge_power: PowerCap::new(800),
};

/// Writing a command to a battery.
///
/// `&self`, not `&mut self`: `ZendureClient` keeps its acMode and storage-mode
/// caches behind a `Mutex`, which is what lets the actuation loop borrow it
/// immutably from inside `tokio::select!`.
///
/// `-> impl Future` rather than `async fn` so the `Send` bound is written out.
/// A bare `async fn` in a trait leaves the future's auto-traits unspecified for
/// generic callers, which would bite the first time actuation moves onto a
/// spawned task — which step 7's "never block the control loop" requires.
///
/// `Error` is an associated type bounded only by `Display`, so an adapter
/// keeps its own error (`reqwest::Error` here) and `actuate` stringifies it at
/// the edge rather than every adapter converting into a shared error enum
/// nobody matches on.
pub trait BatteryController {
    type Error: std::fmt::Display;

    /// Which device this adapter writes to. `actuate` matches a directive's
    /// address against it rather than assuming the only adapter it holds is
    /// the right one — an `Outcome` that names a device must mean the write
    /// actually went there.
    fn id(&self) -> &DeviceId;

    fn apply(&self, command: &Command) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// One command's fate, as recorded. `command` is the `Display` string, which is
/// the format `command_tests.rs` pins and the raw log already quotes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Outcome {
    pub device: DeviceId,
    pub command: String,
    pub outcome: &'static str,
    pub error: Option<String>,
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
/// The `match` picks the controller for the class, so step 9 adds a `charger`
/// parameter and one arm rather than reworking the loop.
///
/// Within a class the directive's address picks the adapter. One adapter per
/// class today, so the match is `==` against its id rather than a lookup — but
/// it is a match, because `Outcome.device` is journalled and a line reading
/// `{"device":"battery-b","outcome":"ok"}` has to mean that box was written to.
/// Handing `battery-b`'s setpoint to `battery-a`'s adapter and recording it as
/// b's success is the failure mode this exists to make impossible.
pub async fn actuate<B: BatteryController>(
    battery: &B,
    directives: &[Directive],
    what: &str,
) -> Vec<Outcome> {
    let mut outcomes = Vec::with_capacity(directives.len());

    for directive in directives {
        // Stringified inside the arm that produced it, so `Outcome` stays one
        // concrete type across adapters with unrelated error types — and each
        // class logs the noun its operators would grep for.
        let result = match directive {
            Directive::Battery { device, command } if device == battery.id() => {
                battery.apply(command).await.map_err(|e| {
                    tracing::error!("Failed to apply {what} to battery {device}: {e}");
                    e.to_string()
                })
            }
            // Unroutable, not applied: an allocation named a device this
            // process does not drive. Reported as a failed outcome — so the
            // journal shows the command never landed and the caller's status
            // goes degraded — rather than dropped silently or sent to whoever
            // happens to be holding the adapter.
            Directive::Battery { device, .. } => {
                tracing::error!(
                    "Cannot apply {what} to {device}: no adapter for that device (this process \
                     drives battery {})",
                    battery.id(),
                );
                Err(format!("no adapter registered for device {device}"))
            }
        };

        outcomes.push(Outcome {
            device: directive.device().clone(),
            command: directive.describe(),
            outcome: if result.is_ok() { "ok" } else { "error" },
            error: result.err(),
        });
    }

    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::Setpoint;
    use std::sync::Mutex;

    /// Records what it was asked to do, and can be told to fail on the nth
    /// call. Three halves now: the order the commands arrived in is the
    /// property under test, a failure has to be injectable at a position other
    /// than the last one to prove the loop keeps going, and it answers for one
    /// device id so a directive addressed elsewhere has somewhere to not go.
    struct RecordingBattery {
        id: DeviceId,
        applied: Mutex<Vec<Command>>,
        fails_at: Option<usize>,
    }

    impl RecordingBattery {
        fn new(id: &str) -> Self {
            RecordingBattery {
                id: DeviceId::new(id),
                applied: Mutex::new(Vec::new()),
                fails_at: None,
            }
        }

        fn failing_at(id: &str, index: usize) -> Self {
            RecordingBattery {
                id: DeviceId::new(id),
                applied: Mutex::new(Vec::new()),
                fails_at: Some(index),
            }
        }

        fn applied(&self) -> Vec<Command> {
            self.applied.lock().unwrap().clone()
        }
    }

    impl BatteryController for RecordingBattery {
        type Error = String;

        fn id(&self) -> &DeviceId {
            &self.id
        }

        async fn apply(&self, command: &Command) -> Result<(), String> {
            // The guard is scoped and dropped before the function's implicit
            // await point for the same reason `ZendureClient::apply_command`
            // scopes its: a `MutexGuard` alive across an await would cost the
            // future its `Send`, and the trait demands it.
            let index = {
                let mut applied = self.applied.lock().unwrap();
                applied.push(*command);
                applied.len() - 1
            };

            if self.fails_at == Some(index) {
                return Err("device unreachable".to_string());
            }
            Ok(())
        }
    }

    fn directive(device: &str, command: Command) -> Directive {
        Directive::Battery {
            device: DeviceId::new(device),
            command,
        }
    }

    /// Every directive is applied, not just the first — the regression the old
    /// `commands.first()` actuation shipped. A single adapter answers for a
    /// single device, so the multi-directive list it can be handed is two
    /// commands to the same box; the second device's half of the seam is the
    /// routing test below, and step 9's second adapter.
    #[tokio::test]
    async fn applies_every_directive_in_order() {
        let battery = RecordingBattery::new("battery-a");
        let directives = [
            directive("battery-a", Command::SetIdle),
            directive("battery-a", Command::SetCharge(Setpoint::new(1200))),
        ];

        let outcomes = actuate(&battery, &directives, "decision").await;

        assert_eq!(
            battery.applied(),
            vec![Command::SetIdle, Command::SetCharge(Setpoint::new(1200))],
        );
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].device, DeviceId::new("battery-a"));
        assert_eq!(outcomes[0].command, "set_idle");
        assert_eq!(outcomes[0].outcome, "ok");
        assert_eq!(outcomes[1].device, DeviceId::new("battery-a"));
        assert_eq!(outcomes[1].command, "set_charge(1200W)");
        assert_eq!(outcomes[1].outcome, "ok");
    }

    /// An unreachable box must not leave the rest uncommanded — the failure is
    /// recorded and the loop carries on.
    #[tokio::test]
    async fn a_failure_does_not_stop_the_rest() {
        let battery = RecordingBattery::failing_at("battery-a", 0);
        let directives = [
            directive("battery-a", Command::SetIdle),
            directive("battery-a", Command::SetCharge(Setpoint::new(1200))),
        ];

        let outcomes = actuate(&battery, &directives, "decision").await;

        assert_eq!(
            battery.applied(),
            vec![Command::SetIdle, Command::SetCharge(Setpoint::new(1200))],
        );
        assert_eq!(outcomes[0].outcome, "error");
        assert_eq!(outcomes[0].error.as_deref(), Some("device unreachable"));
        assert_eq!(outcomes[1].outcome, "ok");
        assert_eq!(outcomes[1].error, None);
    }

    /// The routing half, which the id assertions above cannot prove because
    /// they pass by construction: a directive for a device this process does
    /// not drive must reach no adapter and must be reported as a failure. The
    /// alternative — the shape before this test existed — was writing
    /// `battery-b`'s setpoint into `battery-a` and journalling it as b's
    /// success. The owned directive behind it still lands, for the same reason
    /// an unreachable box does not stop the rest.
    #[tokio::test]
    async fn a_directive_for_another_device_is_an_error_and_is_not_applied() {
        let battery = RecordingBattery::new("battery-a");
        let directives = [
            directive("battery-b", Command::SetCharge(Setpoint::new(1200))),
            directive("battery-a", Command::SetIdle),
        ];

        let outcomes = actuate(&battery, &directives, "decision").await;

        assert_eq!(battery.applied(), vec![Command::SetIdle]);
        assert_eq!(outcomes[0].device, DeviceId::new("battery-b"));
        assert_eq!(outcomes[0].outcome, "error");
        assert_eq!(
            outcomes[0].error.as_deref(),
            Some("no adapter registered for device battery-b"),
        );
        assert_eq!(outcomes[1].device, DeviceId::new("battery-a"));
        assert_eq!(outcomes[1].outcome, "ok");
    }
}
