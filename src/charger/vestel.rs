//! A Vestel EVC04 read over Modbus TCP. Read-only: nothing here writes a
//! register, so the charger's load-management heartbeat (6000) is never fed.
//!
//! The register map is evcc's `charger/vestel.go`: input registers (FC4) on
//! unit 255, u32 values high word first. None of it has been checked against
//! hardware yet, so every word a poll reads is captured as `vestel_modbus`
//! before [`parse`] interprets it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tokio::net::TcpStream;
use tokio_modbus::Slave;
use tokio_modbus::client::{Context, Reader, tcp};

use crate::device::{PollError, RawCapture};
use crate::scan;
use crate::sync::guard;
use crate::units::{ChargerDraw, MilliAmps, SessionDuration, Volts, WattHours};
use crate::world::DeviceId;

use super::state::{CableState, ChargePointStatus, ChargerState, PhaseMode};
use super::{ChargerReading, ChargerTelemetry};

pub const RAW_KIND: &str = "vestel_modbus";

/// One FC4 read: a run of consecutive input registers.
#[derive(Debug, Clone, Copy)]
struct Span {
    start: u16,
    count: u16,
}

const fn span(start: u16, count: u16) -> Span {
    Span { start, count }
}

const PHASES: u16 = 404;
const CHARGEPOINT_STATE: u16 = 1000;
const CABLE_STATE: u16 = 1004;
const CURRENTS: [u16; 3] = [1008, 1010, 1012];
const VOLTAGES: [u16; 3] = [1014, 1016, 1018];
const POWER: u16 = 1020;
const TOTAL_ENERGY: u16 = 1036;
const SESSION_ENERGY: u16 = 1502;
const SESSION_DURATION: u16 = 1508;

/// The reads a poll cannot do without: power (1020) is the one register a
/// decision reads, and status and cable say what that power means.
const REQUIRED_SPANS: [Span; 2] = [span(CHARGEPOINT_STATE, 5), span(CURRENTS[0], 14)];

/// Display-only reads that firmware is not known to answer. A refusal leaves
/// its fields empty rather than costing the reading, since losing the reading
/// would fail the discharge guard open.
const BEST_EFFORT_SPANS: [Span; 3] = [
    span(PHASES, 1),
    span(TOTAL_ENERGY, 2),
    span(SESSION_ENERGY, 8),
];

const SERIAL: Span = span(100, 25);
const BRAND: Span = span(190, 10);
const MODEL: Span = span(210, 5);
const FIRMWARE: Span = span(230, 50);

/// Watt-hours per count of register 1036. Unverified: evcc divides it by 10
/// to get kWh, which makes one count 0.1 kWh. Telemetry only, so a wrong
/// guess shows on the dashboard rather than reaching a decision.
const TOTAL_ENERGY_WH_PER_COUNT: f64 = 100.0;

/// What one poll read, keyed by each read's start register. Serializes as
/// the `vestel_modbus` capture body: `{"1000": [w, w, ...], ...}`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RegisterBlocks(BTreeMap<u16, Vec<u16>>);

impl RegisterBlocks {
    pub fn insert(&mut self, start: u16, words: Vec<u16>) {
        self.0.insert(start, words);
    }

    pub fn capture(&self) -> RawCapture {
        RawCapture {
            kind: RAW_KIND,
            body: serde_json::to_string(&self.0).expect("a map of integers always serializes"),
        }
    }

    fn word(&self, register: u16) -> Result<u16, String> {
        self.0
            .range(..=register)
            .next_back()
            .and_then(|(start, words)| words.get(usize::from(register - start)))
            .copied()
            .ok_or_else(|| format!("register {register} was not read"))
    }

    /// High word first, as evcc reads them.
    fn u32_at(&self, register: u16) -> Result<u32, String> {
        let high = self.word(register)?;
        let low = self.word(register + 1)?;
        Ok((u32::from(high) << 16) | u32::from(low))
    }
}

/// The decision-relevant reading out of one poll's registers.
pub fn parse(blocks: &RegisterBlocks) -> Result<ChargerState, String> {
    let current = |i: usize| blocks.word(CURRENTS[i]).map(|ma| MilliAmps(u32::from(ma)));
    let voltage = |i: usize| blocks.word(VOLTAGES[i]).map(|v| Volts(u32::from(v)));

    Ok(ChargerState {
        status: status(blocks.word(CHARGEPOINT_STATE)?),
        cable: cable(blocks.word(CABLE_STATE)?),
        power: ChargerDraw::new(blocks.u32_at(POWER)?),
        currents: [current(0)?, current(1)?, current(2)?],
        voltages: [voltage(0)?, voltage(1)?, voltage(2)?],
        phases: blocks.word(PHASES).ok().and_then(phases),
        session_energy: blocks
            .u32_at(SESSION_ENERGY)
            .ok()
            .map(|wh| WattHours(f64::from(wh))),
        session_duration: blocks
            .u32_at(SESSION_DURATION)
            .ok()
            .map(|secs| SessionDuration::new(Duration::from_secs(u64::from(secs)))),
    })
}

/// The charger's lifetime meter, in the unit [`TOTAL_ENERGY_WH_PER_COUNT`]
/// guesses.
pub fn total_energy(blocks: &RegisterBlocks) -> Result<WattHours, String> {
    Ok(WattHours(
        f64::from(blocks.u32_at(TOTAL_ENERGY)?) * TOTAL_ENERGY_WH_PER_COUNT,
    ))
}

/// High byte first, NULs dropped wherever they fall. The EVC04 on hand stores
/// one character per register in the low byte (`\0V\0E\0S...`), where evcc's
/// map assumed two; dropping every NUL reads both packings, and a NUL left
/// mid-string would cut the journald line at the first character.
pub fn decode_ascii(words: &[u16]) -> String {
    let bytes: Vec<u8> = words
        .iter()
        .flat_map(|word| word.to_be_bytes())
        .filter(|&byte| byte != 0)
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// Register 1000 counts through OCPP's states in their spec order.
fn status(code: u16) -> ChargePointStatus {
    match code {
        0 => ChargePointStatus::Available,
        1 => ChargePointStatus::Preparing,
        2 => ChargePointStatus::Charging,
        3 => ChargePointStatus::SuspendedEvse,
        4 => ChargePointStatus::SuspendedEv,
        5 => ChargePointStatus::Finishing,
        6 => ChargePointStatus::Reserved,
        7 => ChargePointStatus::Unavailable,
        8 => ChargePointStatus::Faulted,
        other => ChargePointStatus::Unknown(other),
    }
}

fn cable(code: u16) -> CableState {
    match code {
        0 => CableState::NoCable,
        1 => CableState::ChargerOnly,
        2 => CableState::VehicleConnected,
        3 => CableState::VehicleLocked,
        other => CableState::Unknown(other),
    }
}

/// A code outside the documented two reads as no phase mode; the raw
/// capture keeps the code itself for a parser fix.
fn phases(code: u16) -> Option<PhaseMode> {
    match code {
        0 => Some(PhaseMode::One),
        1 => Some(PhaseMode::Three),
        _ => None,
    }
}

/// Why a read came back empty, and whether the connection survived it.
struct ReadFailure {
    error: String,
    /// A timeout or transport error leaves the stream in an unknown state —
    /// a late reply would answer the next request — so it is dropped.
    broken: bool,
}

async fn read(
    context: &mut Context,
    span: Span,
    timeout: Duration,
) -> Result<Vec<u16>, ReadFailure> {
    let what = format!("registers {}+{}", span.start, span.count);
    match tokio::time::timeout(
        timeout,
        context.read_input_registers(span.start, span.count),
    )
    .await
    {
        Err(_) => Err(ReadFailure {
            error: format!("{what}: no answer within {timeout:?}"),
            broken: true,
        }),
        Ok(Err(e)) => Err(ReadFailure {
            error: format!("{what}: {e}"),
            broken: true,
        }),
        Ok(Ok(Err(exception))) => Err(ReadFailure {
            error: format!("{what}: {exception}"),
            broken: false,
        }),
        Ok(Ok(Ok(words))) => Ok(words),
    }
}

/// One Vestel, over one persistent connection opened on first use and
/// reopened after any failure that leaves it in doubt: an EVC04 often
/// accepts only a single client, so a connection per poll would race its
/// own teardown.
pub struct VestelClient {
    id: DeviceId,
    host: String,
    port: u16,
    unit: Slave,
    timeout: Duration,
    connection: tokio::sync::Mutex<Option<Context>>,
    last_seen: Mutex<Option<(ChargePointStatus, CableState)>>,
    /// Best-effort reads that failed last poll, so a firmware that never
    /// answers one warns once rather than every round.
    missing: Mutex<BTreeSet<u16>>,
}

impl VestelClient {
    pub fn new(id: DeviceId, host: String, port: u16, unit_id: u8, scan_period: Duration) -> Self {
        VestelClient {
            id,
            host,
            port,
            unit: Slave(unit_id),
            timeout: scan::request_timeout(scan_period),
            connection: tokio::sync::Mutex::new(None),
            last_seen: Mutex::new(None),
            missing: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn id(&self) -> &DeviceId {
        &self.id
    }

    pub async fn poll(&self) -> Result<ChargerReading, PollError> {
        let blocks = self.read_poll_spans().await?;

        let raw = blocks.capture();
        let state = parse(&blocks).map_err(|error| PollError {
            raw: Some(raw.clone()),
            error,
        })?;
        self.log_transition(&state);

        Ok(ChargerReading {
            state,
            telemetry: ChargerTelemetry {
                total_energy: total_energy(&blocks).ok(),
            },
            raw: Some(raw),
        })
    }

    async fn read_poll_spans(&self) -> Result<RegisterBlocks, PollError> {
        let mut connection = self.connection.lock().await;
        let context = match connection.as_mut() {
            Some(context) => context,
            None => connection.insert(self.connect().await?),
        };

        let mut blocks = RegisterBlocks::default();
        for span in REQUIRED_SPANS {
            match read(context, span, self.timeout).await {
                Ok(words) => blocks.insert(span.start, words),
                Err(failure) => {
                    if failure.broken {
                        *connection = None;
                    }
                    return Err(PollError {
                        raw: (!blocks.0.is_empty()).then(|| blocks.capture()),
                        error: format!("{}: {}", self.id, failure.error),
                    });
                }
            }
        }

        let mut missing = BTreeSet::new();
        for (i, span) in BEST_EFFORT_SPANS.into_iter().enumerate() {
            match read(context, span, self.timeout).await {
                Ok(words) => blocks.insert(span.start, words),
                Err(failure) => {
                    self.note_missing(span, &failure.error);
                    missing.insert(span.start);
                    // Nothing more can be asked of a connection in doubt.
                    if failure.broken {
                        *connection = None;
                        missing.extend(BEST_EFFORT_SPANS[i..].iter().map(|span| span.start));
                        break;
                    }
                }
            }
        }
        self.note_answering(missing);
        Ok(blocks)
    }

    fn note_missing(&self, span: Span, error: &str) {
        if !guard(&self.missing).contains(&span.start) {
            tracing::warn!(
                "{}: {error} — reading on without registers {}+{}",
                self.id,
                span.start,
                span.count
            );
        }
    }

    fn note_answering(&self, missing: BTreeSet<u16>) {
        let mut known = guard(&self.missing);
        for start in known.difference(&missing) {
            tracing::info!("{}: registers from {start} answer again", self.id);
        }
        *known = missing;
    }

    async fn connect(&self) -> Result<Context, PollError> {
        let address = format!("{}:{}", self.host, self.port);
        let failed = |error: String| PollError {
            raw: None,
            error: format!("{}: connecting to {address}: {error}", self.id),
        };

        let stream = match tokio::time::timeout(self.timeout, TcpStream::connect(&address)).await {
            Err(_) => return Err(failed(format!("no answer within {:?}", self.timeout))),
            Ok(Err(e)) => return Err(failed(e.to_string())),
            Ok(Ok(stream)) => stream,
        };
        let mut context = tcp::attach_slave(stream, self.unit);
        self.log_identity(&mut context)
            .await
            .map_err(|failure| failed(failure.error))?;

        Ok(context)
    }

    /// Informational only, so a register the firmware refuses shows as `?`
    /// rather than costing the connection.
    async fn log_identity(&self, context: &mut Context) -> Result<(), ReadFailure> {
        let mut fields: [String; 4] = Default::default();
        for (field, span) in fields.iter_mut().zip([BRAND, MODEL, SERIAL, FIRMWARE]) {
            *field = match read(context, span, self.timeout).await {
                Ok(words) => decode_ascii(&words),
                Err(failure) if failure.broken => return Err(failure),
                Err(failure) => {
                    tracing::warn!("{}: identity unreadable: {}", self.id, failure.error);
                    "?".to_string()
                }
            };
        }
        let [brand, model, serial, firmware] = fields;
        tracing::info!(
            "{}: connected to {brand} {model}, serial {serial}, firmware {firmware}",
            self.id
        );
        Ok(())
    }

    fn log_transition(&self, state: &ChargerState) {
        let now = (state.status, state.cable);
        let before = guard(&self.last_seen).replace(now);
        if before != Some(now) {
            tracing::info!("{}: {:?}, cable {:?}", self.id, state.status, state.cable);
        }
    }
}

#[cfg(test)]
#[path = "vestel_tests.rs"]
mod tests;
