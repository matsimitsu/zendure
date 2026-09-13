//! Turning one objective decision into per-device commands.
//!
//! The objective decides *what the house should do* — charge at 1200 W, stand
//! down, hold. Allocation decides *which box does it*. Keeping the two apart is
//! what lets a second battery, or a car charger competing for the same surplus,
//! arrive as a change to this file and nothing else: the controller keeps
//! answering one question about the house, and `main.rs` keeps actuating a list
//! it does not interpret.
//!
//! Today the list is always one element long, because exactly one device is
//! registered. It is still a list, and still built by walking the world, so no
//! caller decides on its own that one element is all there can be.

use crate::command::Command;
use crate::models::ControlDecision;
use crate::world::{DeviceId, World};

/// One command addressed to one device, tagged by device class.
///
/// A per-class enum, mirroring `Measurement` on the output side, because
/// commands are not uniform across classes: a battery takes a power setpoint,
/// a charger takes a current plus an enable that is emphatically not
/// "set current to zero". A single flat command type would either grow charger
/// variants that `ZendureClient` has to match and reject, or force a lossy
/// common denominator.
///
/// The id rides on the variant rather than inside `Command`: `Command`'s
/// `Display` is the wire format the journal quotes and `command_tests.rs`
/// pins, and a device serial has no business in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive {
    Battery { device: DeviceId, command: Command },
}

impl Directive {
    pub fn device(&self) -> &DeviceId {
        match self {
            Directive::Battery { device, .. } => device,
        }
    }

    /// How this reads in the journal and in the replay render. `Command`'s
    /// `Display` already produces `set_discharge(145W)`; each class renders its
    /// own.
    pub fn describe(&self) -> String {
        match self {
            Directive::Battery { command, .. } => command.to_string(),
        }
    }
}

/// Turn one objective decision into per-device commands.
///
/// Identity while there is one battery — but written as an iteration over the
/// world rather than a hardcoded `vec![]`, so the dropped-command bug it
/// replaces is structurally unrepeatable. The split rule for a second battery
/// and the battery-vs-car ranking land here, and nowhere else knows how many
/// devices exist.
///
/// Order is `World`'s device order, which is sorted by id — so a replayed
/// journal lists the same devices in the same sequence every run, and
/// `actuate` applies them in a sequence that is reproducible rather than
/// whatever a hash happened to yield.
///
/// The two arms differ because what does not broadcast is a command's
/// *magnitude*:
///
/// - `SetIdle`/`SetStandby` carry none. "Stand down" means the same thing to
///   every box, and the MQTT failsafe is only a failsafe if it reaches all of
///   them.
/// - `SetCharge`/`SetDischarge` carry a whole-house figure sized against one
///   battery's headroom, so handing it to a second box asks the house for a
///   multiple of it. Splitting it is a policy — by headroom, fill-first,
///   highest-SoC-first — and the wrong one silently mis-commands hardware.
///
/// Until that rule exists a setpoint goes to the primary battery only, and the
/// rest are left loudly uncommanded rather than quietly over-commanded. No
/// panic and no `debug_assert`: on the decision path of an unattended
/// controller, a degraded fleet and an error line beat a dead process.
pub fn allocate(decision: &ControlDecision, world: &World) -> Vec<Directive> {
    let command = Command::from(decision);

    match command {
        Command::SetIdle | Command::SetStandby => world
            .batteries()
            .map(|(device, _)| Directive::Battery {
                device: device.clone(),
                command,
            })
            .collect(),

        Command::SetCharge(_) | Command::SetDischarge(_) => {
            let mut batteries = world.batteries();
            // No battery registered is the world's "nothing to command" case
            // rather than an error: the caller already reads an empty list as
            // "publish no status", exactly as it did before this arm existed.
            let Some((primary, _)) = batteries.next() else {
                return Vec::new();
            };

            let uncommanded: Vec<String> = batteries.map(|(id, _)| id.to_string()).collect();
            if !uncommanded.is_empty() {
                tracing::error!(
                    "{command} is a whole-house setpoint and there is no rule yet for splitting \
                     one across devices — commanding {primary} alone, leaving {} uncommanded. \
                     Deliberate: the alternative is handing every device the full figure, which \
                     would draw or feed a multiple of the target.",
                    uncommanded.join(", "),
                );
            }

            vec![Directive::Battery {
                device: primary.clone(),
                command,
            }]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::BatteryState;
    use crate::models::ControlMode;
    use crate::units::{BatteryPower, PowerCap, Setpoint, Soc};
    use crate::world::Measurement;

    fn battery() -> BatteryState {
        BatteryState {
            soc: Soc::new(50),
            max_discharge_power: PowerCap::new(800),
            max_charge_power: PowerCap::new(2400),
            current_power: BatteryPower(0),
            soc_calibrating: false,
            soc_limit_reached: false,
            fault: false,
        }
    }

    /// The ids are registered out of order at the call sites below, so an
    /// allocation that merely preserved insertion order would fail the
    /// ordering assertions rather than pass by luck.
    fn world_with(ids: &[&str]) -> World {
        let mut world = World::new();
        for id in ids {
            world.observe_device(DeviceId::new(*id), Measurement::Battery(battery()));
        }
        world
    }

    fn decision(mode: ControlMode, watts: i32) -> ControlDecision {
        ControlDecision {
            mode,
            power_watts: Setpoint::new(watts),
            ..ControlDecision::test_sample()
        }
    }

    /// The production configuration: one battery, one directive, carrying the
    /// setpoint the objective produced. This is the case that must not move.
    #[test]
    fn one_battery_gets_the_setpoint() {
        let world = world_with(&["battery-a"]);

        assert_eq!(
            allocate(&decision(ControlMode::Charge, 1200), &world),
            vec![Directive::Battery {
                device: DeviceId::new("battery-a"),
                command: Command::SetCharge(Setpoint::new(1200)),
            }],
        );
    }

    /// A command with no magnitude broadcasts, in id order: "stand down" means
    /// the same thing to every box, and the MQTT failsafe depends on reaching
    /// all of them.
    #[test]
    fn idle_reaches_every_battery_in_id_order() {
        let world = world_with(&["battery-b", "battery-a"]);

        assert_eq!(
            allocate(&decision(ControlMode::Idle, 0), &world),
            vec![
                Directive::Battery {
                    device: DeviceId::new("battery-a"),
                    command: Command::SetIdle,
                },
                Directive::Battery {
                    device: DeviceId::new("battery-b"),
                    command: Command::SetIdle,
                },
            ],
        );
    }

    /// Standby carries no magnitude either, so it broadcasts for the same
    /// reason — a box left in RAM mode while the rest go to Flash is a fleet
    /// half stood down.
    #[test]
    fn standby_reaches_every_battery() {
        let world = world_with(&["battery-a", "battery-b"]);

        assert_eq!(
            allocate(&decision(ControlMode::Standby, 0), &world),
            vec![
                Directive::Battery {
                    device: DeviceId::new("battery-a"),
                    command: Command::SetStandby,
                },
                Directive::Battery {
                    device: DeviceId::new("battery-b"),
                    command: Command::SetStandby,
                },
            ],
        );
    }

    /// The fence. 1200 W is a figure for the house, so two batteries must not
    /// each be told 1200 W. Until there is a split rule exactly one directive
    /// is emitted — the primary the objective sized the figure against — and
    /// the operator gets an error line naming the devices left out.
    #[test]
    fn a_setpoint_goes_to_the_primary_battery_only() {
        let world = world_with(&["battery-b", "battery-a", "battery-c"]);

        assert_eq!(
            allocate(&decision(ControlMode::Discharge, 600), &world),
            vec![Directive::Battery {
                device: DeviceId::new("battery-a"),
                command: Command::SetDischarge(Setpoint::new(600)),
            }],
        );
    }

    /// The primary is the lowest id, which is what `World::battery()` — and so
    /// `target_power`, which reads that battery's cap and current flow — is
    /// actually using. If the two ever disagreed, the setpoint would be sized
    /// against one box and sent to another.
    #[test]
    fn the_primary_is_the_battery_the_objective_sized_against() {
        let world = world_with(&["zzz", "aaa"]);
        let directives = allocate(&decision(ControlMode::Charge, 900), &world);

        assert_eq!(directives.len(), 1);
        assert_eq!(directives[0].device(), &DeviceId::new("aaa"));
        assert_eq!(
            world.batteries().next().map(|(id, _)| id),
            Some(directives[0].device()),
        );
    }

    /// A world with no device commands nothing rather than panicking — the
    /// caller's existing "empty list, publish no status" path.
    #[test]
    fn no_battery_commands_nothing() {
        let world = world_with(&[]);

        assert!(allocate(&decision(ControlMode::Charge, 1200), &world).is_empty());
        assert!(allocate(&decision(ControlMode::Idle, 0), &world).is_empty());
    }
}
