//! Turning one objective decision into per-device commands: the objective
//! decides *what* the house should do, allocation decides *which box* does
//! it. Keeping them apart means a second battery or a competing car charger
//! is a change to this file alone.
//!
//! The list is always one element today, because exactly one device is
//! registered — but it's built by walking the world, not hardcoded, so no
//! caller assumes that's permanent.

use crate::command::Command;
use crate::models::ControlDecision;
use crate::world::{DeviceId, World};

/// One command addressed to one device, tagged by device class — commands
/// aren't uniform across classes (a battery takes a power setpoint, a
/// charger a current plus an enable). The id rides on the variant, not
/// inside `Command`, whose `Display` is the wire format the journal and
/// `command_tests.rs` pin.
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

/// Turns one objective decision into per-device commands, walking `World`
/// (sorted by id, so replay/actuate order is reproducible) rather than using
/// a hardcoded list. `SetIdle`/`SetStandby` carry no magnitude and broadcast
/// to every device since the failsafe only works if it reaches all of them; a sized
/// `SetCharge`/`SetDischarge` is scoped to one battery's headroom, so it goes to the
/// primary only, leaving the rest logged as uncommanded rather than over-commanded — no
/// panic on this decision path.
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
