//! Turning one objective decision into per-device commands.
//!
//! The objective decides *what the house should do* — charge at 1200 W, stand
//! down, hold. Allocation decides *which box does it*. Keeping the two apart is
//! what lets a second battery, or a car charger competing for the same surplus,
//! arrive as a change to this file and nothing else: the controller keeps
//! answering one question about the house, and `main.rs` keeps actuating a list
//! it does not interpret.
//!
//! Today the list is always one element long. It is still a list, and still
//! built by walking the world, because the bug this module replaces was exactly
//! a caller deciding on its own that one element was all there could be.

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
/// common denominator. One variant today; step 9 adds a `Charger` variant and
/// one arm in `actuate`.
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

    /// How this reads in the journal and in step 8's replay render. `Command`'s
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
pub fn allocate(decision: &ControlDecision, world: &World) -> Vec<Directive> {
    let command = Command::from(decision);

    world
        .batteries()
        .map(|(device, _)| Directive::Battery {
            device: device.clone(),
            command,
        })
        .collect()
}
