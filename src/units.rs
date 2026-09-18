//! Physical quantities as newtypes.
//!
//! Two axes, per `CLAUDE.md`: **units** name what a number measures, and
//! **roles** distinguish values sharing a unit but meaning different things —
//! a signed flow, a non-negative cap, and a commanded setpoint are all watts,
//! and mixing them must not compile. Every type is `#[serde(transparent)]`,
//! serializing as the bare number it wraps, so every wire format (MQTT, HA
//! discovery, the journal, the RTE state file) is unchanged. Casts live only in named
//! conversions here, never in the decision path.

use std::fmt;
use std::ops::{Add, Neg, Sub};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Forward `Display` to the wrapped primitive so format specs survive:
/// `write!(f, "{}", self.0)` silently drops the caller's precision, turning
/// `format!("{v:.1}")` in `publish_rte` from `85.2` into `85.23456789`.
/// Delegating to the primitive's own `fmt` honours width, precision, sign and fill.
macro_rules! forward_display {
    ($t:ty, $inner:ty) => {
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                <$inner as fmt::Display>::fmt(&self.0, f)
            }
        }
    };
}

// `macro_rules!` is scoped to the rest of *this* file unless it is re-exported.
// Exported crate-locally so the next newtype outside this module gets the
// precision-forwarding behaviour by naming it rather than by remembering to
// reproduce it.
pub(crate) use forward_display;

/// `Deserialize` for a newtype whose constructor enforces an invariant,
/// routing the wire value through that constructor instead of writing the
/// field directly: a derived `Deserialize` builds the struct field-by-field,
/// bypassing every clamp. Serialization stays `transparent`, unchanged for any value
/// that was already valid.
macro_rules! validating_deserialize {
    ($t:ty, $inner:ty, $ctor:expr) => {
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                <$inner as Deserialize<'de>>::deserialize(d).map($ctor)
            }
        }
    };
}

/// The same, for a constructor that *rejects* rather than clamps: those
/// other constructors are infallible by design (a SOC of 200 clamps kindly
/// to 100), while this one has values it must refuse outright, and the refusal must
/// reach the deserializer as an error, not a silent default.
macro_rules! validating_deserialize_result {
    ($t:ty, $inner:ty, $ctor:expr) => {
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                <$inner as Deserialize<'de>>::deserialize(d)
                    .and_then(|v| $ctor(v).map_err(serde::de::Error::custom))
            }
        }
    };
}

pub(crate) use validating_deserialize_result;

// --- temperature -----------------------------------------------------------

/// Tenths of a Kelvin, which is how the Zendure reports every temperature.
///
/// A vendor encoding rather than a unit anyone thinks in. Naming it puts the
/// conversion in one function and makes the pair unmixable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeciKelvin(pub u32);

impl DeciKelvin {
    pub fn to_celsius(self) -> Celsius {
        // `from`, not `as`: `u32` to `f64` is lossless and saying so keeps the
        // decision-path rule about casts honest here too.
        Celsius(f64::from(self.0) / 10.0 - 273.15)
    }
}

/// Degrees Celsius. What a person and Home Assistant both read.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Celsius(pub f64);

forward_display!(Celsius, f64);

/// One pack's temperature reading. A named pair rather than `(usize, u32)`,
/// which said neither what the index counted nor what unit the number was
/// in. Belongs here beside the `DeciKelvin` it wraps: the adapter constructs
/// it, and `discovery.rs` only ever borrows what it's handed to publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackTemperature {
    pub index: usize,
    pub temp: DeciKelvin,
}

// --- Watts: the integer-watt arithmetic unit -------------------------------

/// Integer watts: the unit the device speaks and the controller computes
/// in — a working type, not a role, carrying no claim about sign or purpose.
/// Role types (`Setpoint`, `PowerCap`, `BatteryPower`, `PowerMargin`) convert into and
/// out of it through named methods, so every meaning change is a call you can grep for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Watts(pub i32);

forward_display!(Watts, i32);

impl Watts {
    pub const ZERO: Watts = Watts(0);

    /// A non-negative power reading from the device, which reports `u32` watts.
    pub fn from_device(w: u32) -> Self {
        // Device watts are bounded by the hardware's 2400 W rating; the clamp
        // is for a corrupt or hostile payload, not an expected value.
        Watts(w.min(i32::MAX as u32) as i32)
    }

    pub fn get(self) -> i32 {
        self.0
    }

    pub fn as_f64(self) -> f64 {
        f64::from(self.0)
    }
}

impl Add for Watts {
    type Output = Watts;
    fn add(self, rhs: Watts) -> Watts {
        Watts(self.0.saturating_add(rhs.0))
    }
}

impl Sub for Watts {
    type Output = Watts;
    fn sub(self, rhs: Watts) -> Watts {
        Watts(self.0.saturating_sub(rhs.0))
    }
}

impl Neg for Watts {
    type Output = Watts;
    fn neg(self) -> Watts {
        Watts(self.0.saturating_neg())
    }
}

// --- Roles over watts -----------------------------------------------------

/// Signed power at the grid meter, in watts. Positive = importing from the
/// grid, negative = exporting to it. `f64` because the Shelly reports
/// fractional watts and `ControlDecision.grid_power` is a float on the wire.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GridPower(pub f64);

forward_display!(GridPower, f64);

impl GridPower {
    pub const ZERO: GridPower = GridPower(0.0);

    /// How much we're importing, as integer watts. Negative when exporting.
    pub fn importing(self) -> Watts {
        Watts(self.0 as i32)
    }

    /// How much we're exporting, as integer watts. Negative when importing.
    pub fn exporting(self) -> Watts {
        Watts((-self.0) as i32)
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl Neg for GridPower {
    type Output = GridPower;
    fn neg(self) -> GridPower {
        GridPower(-self.0)
    }
}

/// The battery's own flow shifts the meter reading, so the controller decides
/// against the grid *underlying* it: what the house would be drawing if the
/// battery were idle.
impl Add<BatteryPower> for GridPower {
    type Output = GridPower;
    fn add(self, rhs: BatteryPower) -> GridPower {
        GridPower(self.0 + rhs.as_f64())
    }
}

/// Solar inverter production on the configured meter phase, in watts. Never
/// negative — production, not a net flow.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SolarPower(f64);

validating_deserialize!(SolarPower, f64, SolarPower::new);

forward_display!(SolarPower, f64);

impl SolarPower {
    pub const ZERO: SolarPower = SolarPower(0.0);

    /// Clamps at zero: the phase's *export* is the inverter's output, and an
    /// importing phase means no production to read.
    pub fn new(watts: f64) -> Self {
        SolarPower(if watts > 0.0 { watts } else { 0.0 })
    }

    /// Production read as the export on a meter phase, which the meter reports
    /// as a negative net flow.
    pub fn from_phase_export(phase: GridPower) -> Self {
        SolarPower::new(-phase.get())
    }

    /// Production as whole [`Watts`], rounded — a reading to display, not a
    /// setpoint to command.
    pub fn into_watts(self) -> Watts {
        Watts(self.0.round() as i32)
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// One forecast sample: predicted solar production at a point in time. Named
/// rather than a `(Timestamp, SolarPower)` tuple — same reasoning as
/// `PackTemperature` above (RUST-2): a buffer of pairs cannot say which half
/// is the clock and which is the reading, a buffer of these can't be
/// confused with anything else. Belongs here, beside the `SolarPower` and
/// `Timestamp` it wraps.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SolarForecastPoint {
    pub at: Timestamp,
    pub estimate: SolarPower,
}

/// Signed battery flow, in watts. Positive = discharging, negative = charging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BatteryPower(pub i32);

forward_display!(BatteryPower, i32);

impl BatteryPower {
    pub const ZERO: BatteryPower = BatteryPower(0);

    /// The device reports charge and discharge as two non-negative figures.
    pub fn from_flows(discharge: Watts, charge: Watts) -> Self {
        BatteryPower((discharge - charge).get())
    }

    /// How hard it is currently charging, or zero if it isn't.
    pub fn charging(self) -> Watts {
        Watts(self.0.saturating_neg().max(0))
    }

    /// How hard it is currently discharging, or zero if it isn't.
    pub fn discharging(self) -> Watts {
        Watts(self.0.max(0))
    }

    pub fn as_f64(self) -> f64 {
        f64::from(self.0)
    }

    /// The flow as a plain signed [`Watts`], for a caller that wants the
    /// whole number back rather than split by direction — `simulation.rs`
    /// integrating it with [`WattHours::integrate`], which has no notion of
    /// "charging" or "discharging", only a signed rate.
    pub fn into_watts(self) -> Watts {
        Watts(self.0)
    }
}

/// Saturating, exactly as `Watts`'s `Add` is: two packs cannot come near
/// `i32::MAX` watts, so the saturation is there for a corrupt reading rather
/// than an expected sum — and a corrupt reading must not panic a debug build in
/// the decision path.
impl Add for BatteryPower {
    type Output = BatteryPower;
    fn add(self, rhs: BatteryPower) -> BatteryPower {
        BatteryPower(self.0.saturating_add(rhs.0))
    }
}

/// The combined flow of several batteries, which is what the world's meter
/// correction is made of. Folds with `Add`, as `WattHours`'s `Sum` does, so the
/// overflow behaviour is stated once above rather than reinvented here.
impl std::iter::Sum for BatteryPower {
    fn sum<I: Iterator<Item = BatteryPower>>(iter: I) -> BatteryPower {
        iter.fold(BatteryPower::ZERO, Add::add)
    }
}

/// A non-negative power limit, in watts — what the device will accept, or what
/// the controller will not exceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PowerCap(u32);

forward_display!(PowerCap, u32);

impl PowerCap {
    /// Only tests assert against a zeroed cap; the decision path builds them
    /// from device limits.
    #[cfg(test)]
    pub const ZERO: PowerCap = PowerCap(0);

    pub const fn new(watts: u32) -> Self {
        PowerCap(watts)
    }

    pub fn min(self, other: PowerCap) -> PowerCap {
        PowerCap(self.0.min(other.0))
    }

    /// The cap as an upper bound for setpoint arithmetic. Saturating rather
    /// than casting: a `u32` above `i32::MAX` would otherwise wrap negative and
    /// make `clamp(0, max)` panic.
    pub fn watts(self) -> Watts {
        Watts(self.0.min(i32::MAX as u32) as i32)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// A commanded power setpoint, in watts. Never negative — the direction comes
/// from [`ControlMode`](crate::models::ControlMode), not from the sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Setpoint(i32);

validating_deserialize!(Setpoint, i32, Setpoint::new);

forward_display!(Setpoint, i32);

impl Setpoint {
    pub const ZERO: Setpoint = Setpoint(0);

    /// The only place non-negativity is enforced, so no call site re-checks it.
    pub fn clamped(watts: Watts, cap: PowerCap) -> Self {
        Setpoint(watts.get().clamp(0, cap.watts().get()))
    }

    /// A literal setpoint, clamped at zero. For fixed values (idle, standby)
    /// and tests; the decision path goes through [`Setpoint::clamped`].
    pub fn new(watts: i32) -> Self {
        Setpoint(watts.max(0))
    }

    /// Scale by the ramp factor applied on the first decision after a mode
    /// change. Truncates toward zero.
    pub fn ramped(self, factor: f64) -> Self {
        Setpoint((f64::from(self.0) * factor) as i32)
    }

    pub fn is_positive(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// A non-negative safety margin, in watts, subtracted from a setpoint so the
/// commanded power stays on the safe side of the grid reading it was derived
/// from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PowerMargin(u32);

forward_display!(PowerMargin, u32);

impl PowerMargin {
    pub fn new(watts: u32) -> Self {
        PowerMargin(watts)
    }

    pub fn watts(self) -> Watts {
        Watts(self.0.min(i32::MAX as u32) as i32)
    }
}

// --- State of charge and percentages --------------------------------------

/// State of charge, as whole percent. Clamped to 0–100 on construction, so no
/// call site re-checks the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Soc(u32);

validating_deserialize!(Soc, u32, Soc::new);

forward_display!(Soc, u32);

impl Soc {
    pub const ZERO: Soc = Soc(0);
    pub const FULL: Soc = Soc(100);

    pub fn new(percent: u32) -> Self {
        Soc(percent.min(100))
    }

    /// The device reports some SOC setpoints in tenths of a percent — 1000 is
    /// 100.0%. Naming the conversion is the point: reading one as whole percent
    /// is a 10x error of the same family as milliamps for amps.
    pub fn from_tenths(tenths: u32) -> Self {
        Soc::new(tenths / 10)
    }

    /// The fraction of the pack sitting above `floor`, 0.0–1.0. Saturating, so
    /// an SOC below the floor reads as nothing usable rather than underflowing.
    pub fn fraction_above(self, floor: Soc) -> f64 {
        f64::from(self.0.saturating_sub(floor.0)) / 100.0
    }

    /// A state of charge from `stored / capacity`. Rounds to a whole percent
    /// and routes through [`Soc::new`], so a value a hair below zero lands at
    /// 0 rather than wrapping. NaN (a zero-capacity pack computing `0.0 / 0.0`)
    /// maps to [`Soc::ZERO`] explicitly, rather than leaning on `NaN as u32` being a
    /// defined-but-meaningless `0`.
    pub fn from_fraction(fraction: f64) -> Self {
        if fraction.is_nan() {
            return Soc::ZERO;
        }
        // The cast saturates rather than wraps (float-to-int `as` has done so
        // since the 2018 edition): a negative fraction lands at 0, one above 1
        // lands at `u32::MAX`, and `Soc::new` clamps either the rest of the way.
        Soc::new((fraction * 100.0).round() as u32)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// A percentage that is not a state of charge — round-trip efficiency, today.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Percent(pub f64);

forward_display!(Percent, f64);

impl Percent {
    pub fn get(self) -> f64 {
        self.0
    }

    /// As a 0.0–1.0 factor.
    pub fn fraction(self) -> f64 {
        self.0 / 100.0
    }
}

/// A round-trip conversion efficiency, in percent, clamped to 1–100. Not a
/// bare [`Percent`]: `simulation.rs` divides by this, so a `0` would mint
/// infinite energy out of a battery that gave up nothing; clamping at 1 degrades a
/// nonsensical value to "almost total loss" instead of rejecting it.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Efficiency(f64);

validating_deserialize!(Efficiency, f64, Efficiency::new);

forward_display!(Efficiency, f64);

impl Efficiency {
    /// Clamps to 1–100. NaN clamps to neither bound under `f64::clamp` (it
    /// compares false against both, so an unguarded clamp returns NaN
    /// unchanged) and is mapped to 1 instead — worst-but-defined, rather than a value
    /// that turns every downstream energy computation into NaN.
    pub fn new(percent: f64) -> Self {
        if percent.is_nan() {
            return Efficiency(1.0);
        }
        Efficiency(percent.clamp(1.0, 100.0))
    }

    /// As a 0.0–1.0 factor, for multiplying a charge or dividing a discharge.
    pub fn fraction(self) -> f64 {
        self.0 / 100.0
    }

    /// The raw percent, read back only by the clamping tests. Production goes
    /// through [`Efficiency::fraction`].
    #[cfg(test)]
    pub fn get(self) -> f64 {
        self.0
    }
}

// --- Energy ---------------------------------------------------------------

/// Energy in watt-hours.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WattHours(pub f64);

forward_display!(WattHours, f64);

impl WattHours {
    pub const ZERO: WattHours = WattHours(0.0);

    /// Trapezoidal integration of power over an interval: the one genuinely
    /// dimensional computation in the crate. Watts times hours is watt-hours.
    pub fn integrate(previous: Watts, current: Watts, dt: Duration) -> Self {
        let hours = dt.as_secs_f64() / 3600.0;
        WattHours((previous.as_f64() + current.as_f64()) / 2.0 * hours)
    }

    pub fn to_kwh(self) -> KiloWattHours {
        KiloWattHours(self.0 / 1000.0)
    }

    /// Energy back to the average power over an interval — the inverse of
    /// [`WattHours::integrate`] for a span `simulation.rs` knows was at
    /// constant power. Guards `dt == 0` (else an infinite/NaN wattage) and
    /// rounds rather than truncates, since inverting an efficiency division leaves
    /// `999.9999999998`-style floats that truncation would turn into a systematic 1 W
    /// undercount.
    pub fn over(self, dt: Duration) -> Watts {
        if dt.is_zero() {
            return Watts::ZERO;
        }
        let hours = dt.as_secs_f64() / 3600.0;
        Watts((self.0 / hours).round() as i32)
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl Add for WattHours {
    type Output = WattHours;
    fn add(self, rhs: WattHours) -> WattHours {
        WattHours(self.0 + rhs.0)
    }
}

impl std::iter::Sum for WattHours {
    fn sum<I: Iterator<Item = WattHours>>(iter: I) -> WattHours {
        iter.fold(WattHours::ZERO, Add::add)
    }
}

/// Energy in kilowatt-hours — what gets published to Home Assistant.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KiloWattHours(pub f64);

forward_display!(KiloWattHours, f64);

impl KiloWattHours {
    pub const ZERO: KiloWattHours = KiloWattHours(0.0);

    pub fn get(self) -> f64 {
        self.0
    }
}

// --- Time -----------------------------------------------------------------

/// A wall-clock instant, as unix milliseconds.
///
/// Wall-clock rather than a monotonic `Instant` because a recorded event has to
/// replay identically later, which a process-relative counter can't do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub i64);

forward_display!(Timestamp, i64);

impl Timestamp {
    pub fn from_millis(ms: i64) -> Self {
        Timestamp(ms)
    }

    pub fn as_millis(self) -> i64 {
        self.0
    }
}

/// The one place a `chrono` instant becomes ours.
///
/// CLAUDE.md's rule is that a quantity crossing a boundary is one named call,
/// not a conversion spelled out wherever it is needed.
impl<Tz: chrono::TimeZone> From<chrono::DateTime<Tz>> for Timestamp {
    fn from(dt: chrono::DateTime<Tz>) -> Self {
        Timestamp(dt.timestamp_millis())
    }
}

/// A signed span between two [`Timestamp`]s, in milliseconds. Deliberately
/// not a `Duration`: a backwards NTP step makes a span negative, which reads
/// as "not yet elapsed"; saturating it to zero would flip every comparison against a
/// `Duration::ZERO` threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Elapsed(i64);

impl Elapsed {
    /// A `Duration` as whole milliseconds. Config durations are seconds or
    /// minutes, so the cast can't overflow.
    pub fn of(duration: Duration) -> Self {
        Elapsed(duration.as_millis() as i64)
    }

    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / 1000.0
    }

    pub fn as_millis(self) -> i64 {
        self.0
    }
}

impl Sub for Timestamp {
    type Output = Elapsed;
    fn sub(self, rhs: Timestamp) -> Elapsed {
        Elapsed(self.0 - rhs.0)
    }
}

impl Sub<Elapsed> for Timestamp {
    type Output = Timestamp;
    fn sub(self, rhs: Elapsed) -> Timestamp {
        Timestamp(self.0 - rhs.0)
    }
}

impl Add<Elapsed> for Timestamp {
    type Output = Timestamp;
    fn add(self, rhs: Elapsed) -> Timestamp {
        Timestamp(self.0 + rhs.0)
    }
}

/// Compare a span directly against a configured window, keeping the signed
/// semantics: `Elapsed(-5) < Duration::ZERO` is true.
impl PartialEq<Duration> for Elapsed {
    fn eq(&self, other: &Duration) -> bool {
        self.0 == Elapsed::of(*other).0
    }
}

impl PartialOrd<Duration> for Elapsed {
    fn partial_cmp(&self, other: &Duration) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(&Elapsed::of(*other).0)
    }
}

/// A journal retention window, in whole days. `prune` computes `now - days`,
/// so a negative value puts the cutoff in the *future*, deleting the entire
/// journal at every startup and midnight while reporting success; a value near
/// `i64::MAX` panics `chrono::Duration::days` on the writer thread, where the panic is
/// swallowed. The constructor is the only way in and clamps once; `Deserialize` routes
/// through it rather than deriving, since a transparent derive would let
/// `retention_days = 0` off a wire (a config file is a wire) put `cutoff` at *now*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct RetentionDays(i64);

validating_deserialize_result!(RetentionDays, i64, RetentionDays::new);

impl RetentionDays {
    /// Longest window we will honour. Far past any useful retention, and far
    /// below the point where day-to-millisecond arithmetic can overflow.
    pub const MAX_DAYS: i64 = 3_650;

    /// Rejects the values that destroy data rather than clamping them: zero and
    /// negative are almost certainly a typo, and silently reading them as "keep
    /// one day" would be its own surprise. An absurd upper value *is* clamped,
    /// since the intent there is unambiguous.
    pub fn new(days: i64) -> Result<Self, String> {
        if days <= 0 {
            return Err(format!("must be a positive number of days, got {days}"));
        }
        Ok(RetentionDays(days.min(Self::MAX_DAYS)))
    }

    /// The clamped day count, read only by the constructor's tests. `prune`
    /// asks for a [`Timestamp`] via [`RetentionDays::cutoff`].
    #[cfg(test)]
    pub fn days(self) -> i64 {
        self.0
    }

    /// The oldest timestamp worth keeping. Rows strictly before this go.
    ///
    /// Takes `now` rather than reading the clock, so the boundary is testable
    /// without waiting a day — the same reason the controller takes a `Clock`.
    pub fn cutoff(self, now: chrono::DateTime<chrono::Utc>) -> Timestamp {
        (now - chrono::Duration::days(self.0)).into()
    }
}

impl fmt::Display for RetentionDays {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        i64::fmt(&self.0, f)
    }
}

#[cfg(test)]
#[path = "units_tests.rs"]
mod tests;
