//! A simulated EV charger, so the charger path runs end to end with no
//! hardware. A car plugs in and out on a daily schedule and charges at a
//! fixed current until its session target is reached, as a pure function of
//! the clock like [`crate::source::synthetic::HouseProfile`].

use std::time::Duration;

use chrono::{NaiveDateTime, TimeZone};
use chrono_tz::Tz;

use crate::clock::Clock;
use crate::device::PollError;
use crate::schedule::TimeOfDay;
use crate::units::{ChargerDraw, MilliAmps, SessionDuration, Timestamp, Volts, WattHours};
use crate::world::DeviceId;

use super::state::{CableState, ChargePointStatus, ChargerState, PhaseMode};
use super::{ChargerReading, ChargerTelemetry};

/// Every phase of the simulated supply, whether the car draws on it or not.
const NOMINAL_VOLTAGE: Volts = Volts(230);

/// One car's daily routine: when it is plugged in, how hard it charges and
/// how much it wants.
pub struct ChargeProfile {
    plug_in: TimeOfDay,
    unplug: TimeOfDay,
    current: MilliAmps,
    phases: PhaseMode,
    target: WattHours,
    timezone: Tz,
}

impl ChargeProfile {
    pub fn new(
        plug_in: TimeOfDay,
        unplug: TimeOfDay,
        current: MilliAmps,
        phases: PhaseMode,
        target: WattHours,
        timezone: Tz,
    ) -> Self {
        ChargeProfile {
            plug_in,
            unplug,
            current,
            phases,
            target,
            timezone,
        }
    }

    pub fn at(&self, clock: &Clock) -> ChargerState {
        match self.plugged_in_since(clock.now) {
            Some(since) => self.session(clock.now, since),
            None => self.unplugged(),
        }
    }

    /// When the session in progress at `now` began, or `None` outside the
    /// window. The window may wrap midnight, in which case a session seen
    /// before the unplug time began the evening before.
    fn plugged_in_since(&self, now: Timestamp) -> Option<Timestamp> {
        let local = self
            .timezone
            .timestamp_millis_opt(now.as_millis())
            .single()?;
        let time = local.time();
        let (plug_in, unplug) = (self.plug_in.as_naive_time(), self.unplug.as_naive_time());

        let in_window = if plug_in <= unplug {
            plug_in <= time && time < unplug
        } else {
            plug_in <= time || time < unplug
        };
        if !in_window {
            return None;
        }

        let date = if time >= plug_in {
            local.date_naive()
        } else {
            local.date_naive().pred_opt()?
        };
        self.instant_of(date.and_time(plug_in))
    }

    /// A local wall-clock time as an instant. One that falls inside a
    /// spring-forward gap never shows on the clock, so it resolves to the
    /// first minute that does, which is when the clock passed it.
    fn instant_of(&self, local: NaiveDateTime) -> Option<Timestamp> {
        const LONGEST_DST_GAP_MINUTES: i64 = 120;
        (0..=LONGEST_DST_GAP_MINUTES).find_map(|minutes| {
            self.timezone
                .from_local_datetime(&(local + chrono::Duration::minutes(minutes)))
                .earliest()
                .map(Timestamp::from)
        })
    }

    fn session(&self, now: Timestamp, since: Timestamp) -> ChargerState {
        let elapsed = Duration::from_millis(u64::try_from((now - since).as_millis()).unwrap_or(0));

        let mut currents = [MilliAmps::ZERO; 3];
        for current in currents.iter_mut().take(self.phases.count()) {
            *current = self.current;
        }
        let voltages = [NOMINAL_VOLTAGE; 3];
        let power: ChargerDraw = currents
            .iter()
            .map(|current| current.draw_at(NOMINAL_VOLTAGE))
            .sum();

        let drawn = power.over(elapsed);
        let (status, power, currents, session_energy) = if drawn.get() >= self.target.get() {
            (
                ChargePointStatus::SuspendedEv,
                ChargerDraw::ZERO,
                [MilliAmps::ZERO; 3],
                self.target,
            )
        } else {
            (ChargePointStatus::Charging, power, currents, drawn)
        };

        ChargerState {
            status,
            cable: CableState::VehicleLocked,
            power,
            currents,
            voltages,
            phases: self.phases,
            session_energy,
            session_duration: SessionDuration::new(elapsed),
        }
    }

    fn unplugged(&self) -> ChargerState {
        ChargerState {
            status: ChargePointStatus::Available,
            cable: CableState::NoCable,
            power: ChargerDraw::ZERO,
            currents: [MilliAmps::ZERO; 3],
            voltages: [NOMINAL_VOLTAGE; 3],
            phases: self.phases,
            session_energy: WattHours::ZERO,
            session_duration: SessionDuration::ZERO,
        }
    }
}

/// A [`ChargeProfile`] under an identity, answering reads off the wall clock.
pub struct SimulatedCharger {
    id: DeviceId,
    profile: ChargeProfile,
}

impl SimulatedCharger {
    pub fn new(id: DeviceId, profile: ChargeProfile) -> Self {
        SimulatedCharger { id, profile }
    }

    pub fn id(&self) -> &DeviceId {
        &self.id
    }

    pub fn at(&self, clock: &Clock) -> ChargerState {
        self.profile.at(clock)
    }

    /// Infallible, and with nothing to capture: there is no wire format
    /// between the car and the reader.
    pub async fn poll(&self) -> Result<ChargerReading, PollError> {
        let clock = Clock::now(self.profile.timezone);
        Ok(ChargerReading {
            state: self.at(&clock),
            telemetry: ChargerTelemetry::default(),
            raw: None,
        })
    }
}

#[cfg(test)]
#[path = "simulated_tests.rs"]
mod tests;
