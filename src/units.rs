//! Physical quantities as newtypes.
//!
//! Two axes, per `CLAUDE.md`. **Units** name what a number measures. **Roles**
//! distinguish values that share a unit but mean different things: a signed
//! flow at the meter, a non-negative cap, and a commanded setpoint are all
//! watts, and mixing them must not compile.
//!
//! Every type is `#[serde(transparent)]`, so it serializes as the bare number
//! it wraps and every wire format — MQTT, HA discovery, the NDJSON journal, the
//! RTE state file — is byte-identical to before these types existed.
//!
//! Casts live in here, inside named conversions, and nowhere else. A cast in
//! the decision path means a quantity crossed a boundary without anyone saying
//! what the conversion meant; a cast inside `GridPower::exporting` is that
//! statement.
//!
//! `Amps` / `MilliAmps` are deliberately absent. They belong to the charger
//! (step 9), where Peblar's milliamp setpoint and Vestel's whole-amp setpoint
//! are 1000x apart and must be distinct types. Adding them now would mean dead
//! code carrying an `#[allow]` until that adapter exists.

// Several accessors here are exercised only by the test modules and the
// wire-format guards, which the non-test build doesn't compile — the same
// situation `Config` and the wire DTOs in `models.rs` already carry this
// attribute for. Every item below has a call site; none is speculative API.
#![allow(dead_code)]

use std::fmt;
use std::ops::{Add, Neg, Sub};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Forward `Display` to the wrapped primitive so format specs survive.
///
/// This matters more than it looks: `write!(f, "{}", self.0)` silently drops
/// the caller's precision, which would turn `format!("{v:.1}")` in `publish_rte`
/// from `85.2` into `85.23456789`. Delegating to the primitive's own `fmt`
/// honours width, precision, sign and fill exactly as before.
macro_rules! forward_display {
    ($t:ty, $inner:ty) => {
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                <$inner as fmt::Display>::fmt(&self.0, f)
            }
        }
    };
}

// `macro_rules!` is scoped to the rest of *this* file unless it is re-exported,
// which is why `DeviceId`'s `Display` in `world.rs` was hand-written as a
// character-for-character copy of what this expands to. Exported crate-locally
// so the next newtype outside this module gets the precision-forwarding
// behaviour by naming it rather than by remembering to reproduce it.
pub(crate) use forward_display;

/// `Deserialize` for a newtype whose constructor enforces an invariant, routing
/// the wire value through that constructor instead of writing the field
/// directly.
///
/// `#[serde(transparent)]` derives *both* halves, and the derived `Deserialize`
/// builds the struct field-by-field — so every clamp in this module was
/// bypassed by anything that read a value back. That did not matter while the
/// only reader was the journal reading its own writes, because everything
/// written had already been through a constructor. It started mattering the
/// moment a person could hand us a number: `replay --set min_soc=1000` produced
/// `Soc(1000)`, and a hand-edited fixture could put any SOC in the world.
/// CLAUDE.md's rule is that validation lives in the constructor and no call
/// site re-checks; a derived `Deserialize` is a call site that skips it.
///
/// Serialization stays `transparent`, so the wire format — MQTT, HA discovery,
/// the journal, a fixture — is unchanged in both directions for any value that
/// was valid to begin with.
macro_rules! validating_deserialize {
    ($t:ty, $inner:ty, $ctor:expr) => {
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                <$inner as Deserialize<'de>>::deserialize(d).map($ctor)
            }
        }
    };
}

/// The same, for a constructor that *rejects* rather than clamps.
///
/// A sibling rather than a generalisation of the macro above: those
/// constructors are infallible by design — a SOC of 200 is a number someone
/// meant as a percentage and clamping it is the kind reading — while this one
/// has values it must refuse outright, and the refusal has to reach the
/// deserializer as an error rather than become a silent default.
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
/// A vendor encoding rather than a unit anyone thinks in, and it existed only
/// as a bare `u32` travelling next to a `f64` Celsius with a cast between them
/// — the one place in the crate where CLAUDE.md's rule was not applied. Naming
/// it puts the conversion in one function and makes the pair unmixable.
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
/// which said neither what the index was counting nor what unit the number
/// was in.
///
/// A device reading, not a wire-format concern — it used to live in
/// `mqtt/discovery.rs`, which meant a `device.rs` adapter producing one would
/// have had to depend on `mqtt` to name its own return type. It belongs here,
/// beside the `DeciKelvin` it wraps: the adapter constructs it, and
/// `discovery.rs` only ever borrows what it's handed to publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackTemperature {
    pub index: usize,
    pub temp: DeciKelvin,
}

// --- Watts: the integer-watt arithmetic unit -------------------------------

/// Integer watts. The unit the device speaks and the controller computes in —
/// a working type, not a role: it carries no claim about sign or purpose.
///
/// Role types (`Setpoint`, `PowerCap`, `BatteryPower`, `PowerMargin`) convert
/// into and out of it through named methods, so every place a quantity changes
/// meaning is a call you can grep for.
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
/// grid, negative = exporting to it.
///
/// `f64` because the Shelly reports fractional watts and because
/// `ControlDecision.grid_power` is a float on the wire.
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

    pub fn get(self) -> f64 {
        self.0
    }
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
    /// change. Truncates toward zero, as the original `as i32` did.
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

    /// A state of charge from `stored / capacity`, for `simulation.rs` reading
    /// its integrated energy back out as a percentage. `Soc` has no
    /// fractional representation, so this rounds to the nearest whole percent
    /// and routes through [`Soc::new`] — the same clamp every other
    /// constructor here goes through, which is what makes a capacity rounding
    /// error or a stored value that has drifted a hair below zero land at 0 or
    /// 100 instead of panicking or wrapping.
    ///
    /// NaN — a zero-capacity pack computing `0.0 / 0.0` — maps to
    /// [`Soc::ZERO`] rather than being handed to the rounding and the cast
    /// below: `NaN as u32` is a defined-but-meaningless `0` in Rust today, and
    /// this says that explicitly instead of leaning on it.
    pub fn from_fraction(fraction: f64) -> Self {
        if fraction.is_nan() {
            return Soc::ZERO;
        }
        // The cast saturates rather than wraps (Rust's `as` has done so for
        // float-to-int since the 2018 edition), so a negative fraction (stored
        // dipping a hair below zero from floating-point error) or one above 1
        // (a rounding blip past full) lands at 0 or `u32::MAX` and `Soc::new`
        // clamps it the rest of the way to the valid range.
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

/// A round-trip conversion efficiency, in percent, clamped to 1–100.
///
/// Not a bare [`Percent`]: `simulation.rs` divides by this to turn a
/// discharge's stored-energy loss back into the meter-side power that
/// produced it, and a `0` would mint infinite energy out of a battery that
/// gave up nothing. Clamping the low end at 1 rather than rejecting keeps it
/// a peer of `Soc` — a config or fixture with a nonsensical value degrades to
/// "almost total loss" instead of refusing to build a simulated battery over
/// it. The high end at 100 is the ordinary ceiling: nothing converts energy
/// at better than perfect.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Efficiency(f64);

validating_deserialize!(Efficiency, f64, Efficiency::new);

forward_display!(Efficiency, f64);

impl Efficiency {
    /// Clamps to 1–100. NaN clamps to neither bound under `f64::clamp` (it
    /// compares false against both, so an unguarded clamp would return the
    /// NaN unchanged) and is mapped to 1 instead — the worst-but-defined
    /// efficiency, rather than a value that turns every energy computation
    /// downstream into NaN.
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
    /// [`WattHours::integrate`] for a span `simulation.rs` already knows was
    /// at constant power, which is exactly the shape of `advance_to`'s
    /// clamped stored-energy delta over the interval it was clamped within.
    ///
    /// Guards `dt == 0`: dividing by zero seconds would produce an infinite
    /// or NaN wattage rather than "no time passed, so nothing flowed."
    ///
    /// Rounds rather than truncating: `simulation.rs` gets here by inverting
    /// an efficiency division and multiplication it just applied, and that
    /// round trip leaves the odd `999.9999999998`-style float behind even
    /// when every input was a whole watt. Truncating would turn that into a
    /// silent, systematic 1 W undercount; rounding recovers the whole number
    /// the computation actually meant.
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
/// It was open-coded as `Timestamp::from_millis(dt.timestamp_millis())` at four
/// sites — the clock, the journal's raw capture, retention's cutoff, and the
/// CLI's argument parser. CLAUDE.md's rule is that a quantity crossing a
/// boundary is one named call, not a conversion spelled out wherever it is
/// needed.
impl<Tz: chrono::TimeZone> From<chrono::DateTime<Tz>> for Timestamp {
    fn from(dt: chrono::DateTime<Tz>) -> Self {
        Timestamp(dt.timestamp_millis())
    }
}

/// A signed span between two [`Timestamp`]s, in milliseconds.
///
/// Signed, and deliberately not a `Duration`: a backwards NTP step makes a span
/// negative, and today that correctly reads as "not yet elapsed". Saturating it
/// to zero would flip every comparison against a `Duration::ZERO` threshold.
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
/// semantics: `Elapsed(-5) < Duration::ZERO` is true, as the bare `i64`
/// comparison it replaces was.
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

/// A journal retention window, in whole days.
///
/// A newtype because the alternative bit: retention was a bare `i64` threaded
/// through four signatures, and `prune` computes `now - days`. A negative value
/// therefore puts the cutoff in the *future*, and "delete everything older than
/// the cutoff" deletes the entire journal — at every startup and every midnight,
/// reporting it as a successful prune. A value near `i64::MAX` panicked
/// `chrono::Duration::days` on the writer thread, where the panic was swallowed.
///
/// Both are impossible to express now: the constructor is the only way in, it
/// clamps once, and `cutoff` owns the arithmetic so no call site repeats it.
///
/// `Deserialize` is routed through that constructor rather than derived, for
/// the reason [`validating_deserialize`] gives — and this type is the sharpest
/// case of it. A transparent derive builds the field directly, so
/// `retention_days = 0` read off a wire would produce `RetentionDays(0)`, put
/// `cutoff` at *now*, and delete the entire journal at startup and every
/// midnight: the exact failure the paragraph above says is impossible to
/// express. Nothing deserialized one while the only source was the environment,
/// which is why it went unnoticed; a configuration file is a wire.
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
