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
            Directive::Battery { command, .. } => battery.apply(command).await.map_err(|e| {
                tracing::error!("Failed to apply {what} to battery: {e}");
                e.to_string()
            }),
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
    /// call. Both halves matter: the order the commands arrived in is the
    /// property under test, and a failure has to be injectable at a position
    /// other than the last one to prove the loop keeps going.
    struct RecordingBattery {
        applied: Mutex<Vec<Command>>,
        fails_at: Option<usize>,
    }

    impl RecordingBattery {
        fn new() -> Self {
            RecordingBattery {
                applied: Mutex::new(Vec::new()),
                fails_at: None,
            }
        }

        fn failing_at(index: usize) -> Self {
            RecordingBattery {
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

    /// The two-directive case the seam exists for: one box stands down so
    /// another can take the surplus. Against the old `commands.first()` this
    /// applied the stop and never the start.
    #[tokio::test]
    async fn applies_every_directive_in_order() {
        let battery = RecordingBattery::new();
        let directives = [
            directive("battery-a", Command::SetIdle),
            directive("battery-b", Command::SetCharge(Setpoint::new(1200))),
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
        assert_eq!(outcomes[1].device, DeviceId::new("battery-b"));
        assert_eq!(outcomes[1].command, "set_charge(1200W)");
        assert_eq!(outcomes[1].outcome, "ok");
    }

    /// An unreachable box must not leave the rest uncommanded — the failure is
    /// recorded and the loop carries on.
    #[tokio::test]
    async fn a_failure_does_not_stop_the_rest() {
        let battery = RecordingBattery::failing_at(0);
        let directives = [
            directive("battery-a", Command::SetIdle),
            directive("battery-b", Command::SetCharge(Setpoint::new(1200))),
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
}
