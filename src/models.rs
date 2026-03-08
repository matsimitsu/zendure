use serde::{Deserialize, Serialize};
use std::fmt;

// --- MQTT input: smart meter reading ---

/// DSMR P1 smart meter reading, received via MQTT on the meter topic.
/// Per-phase power values are always positive (V × |I| × PF) — the meter
/// cannot distinguish import from export. Solar is on phase 1, so that
/// phase needs correction (see main.rs coordinator loop).
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct MeterReading {
    /// Meter identifier
    pub device_id: String,
    /// Cumulative energy imported from grid, total (kWh) — OBIS 1-0:1.8.0
    pub consumption_total_kwh: f64,
    /// Cumulative energy imported, tariff 1 / high (kWh) — OBIS 1-0:1.8.1
    pub consumption_t1_kwh: f64,
    /// Cumulative energy imported, tariff 2 / low (kWh) — OBIS 1-0:1.8.2
    pub consumption_t2_kwh: f64,
    /// Cumulative energy exported to grid, total (kWh) — OBIS 1-0:2.8.0
    pub production_total_kwh: f64,
    /// Cumulative energy exported, tariff 1 (kWh) — OBIS 1-0:2.8.1
    pub production_t1_kwh: f64,
    /// Cumulative energy exported, tariff 2 (kWh) — OBIS 1-0:2.8.2
    pub production_t2_kwh: f64,
    /// Phase 1 voltage (V) — OBIS 1-0:32.7.0
    pub phase1_voltage: f64,
    /// Phase 2 voltage (V) — OBIS 1-0:52.7.0
    pub phase2_voltage: f64,
    /// Phase 3 voltage (V) — OBIS 1-0:72.7.0
    pub phase3_voltage: f64,
    /// Phase 1 current, unsigned (A) — OBIS 1-0:31.7.0
    pub phase1_current: f64,
    /// Phase 2 current, unsigned (A) — OBIS 1-0:51.7.0
    pub phase2_current: f64,
    /// Phase 3 current, unsigned (A) — OBIS 1-0:71.7.0
    pub phase3_current: f64,
    /// Grid frequency (Hz) — OBIS 1-0:14.7.0
    pub frequency: f64,
    /// Phase 1 power factor — OBIS 1-0:33.7.0
    pub phase1_pf: f64,
    /// Phase 2 power factor — OBIS 1-0:53.7.0
    pub phase2_pf: f64,
    /// Phase 3 power factor — OBIS 1-0:73.7.0
    pub phase3_pf: f64,
    /// Phase 1 power (W), always positive — computed as V × |I| × PF
    pub phase1_power: f64,
    /// Phase 2 power (W), always positive — computed as V × |I| × PF
    pub phase2_power: f64,
    /// Phase 3 power (W), always positive — computed as V × |I| × PF
    pub phase3_power: f64,
    /// Sum of all phase powers (W), always positive
    pub total_power: f64,
    /// ISO 8601 timestamp of the reading
    pub timestamp: String,
}

// --- Zendure REST API types ---

/// Top-level response from GET /properties/report on the Zendure AC 2400+.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZendureReport {
    /// Unix timestamp from device
    pub timestamp: Option<u64>,
    /// Incrementing message counter
    pub message_id: Option<u64>,
    /// Device serial number (e.g. "HEC4NENCN490270")
    pub sn: Option<String>,
    /// API version
    pub version: Option<u64>,
    /// Product identifier (e.g. "solarFlow2400AC+")
    pub product: Option<String>,
    /// All device properties
    pub properties: ZendureProperties,
    /// Per-battery-pack data
    pub pack_data: Option<Vec<PackData>>,
}

/// Device properties from the Zendure AC 2400+.
/// All fields are Option because the API may omit any field.
/// Values confirmed from live device at 192.168.1.253.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZendureProperties {
    /// Battery state of charge (%) — 0-100
    pub electric_level: Option<u32>,
    /// Battery discharge power (W)
    pub pack_input_power: Option<u32>,
    /// Battery charge power (W)
    pub output_pack_power: Option<u32>,
    /// Power output to home (W)
    pub output_home_power: Option<u32>,
    /// Grid input power (W)
    pub grid_input_power: Option<u32>,
    /// Total solar/PV input power (W)
    pub solar_input_power: Option<u32>,
    /// PV channel 1 power (W)
    pub solar_power1: Option<u32>,
    /// PV channel 2 power (W)
    pub solar_power2: Option<u32>,
    /// PV channel 3 power (W)
    pub solar_power3: Option<u32>,
    /// PV channel 4 power (W)
    pub solar_power4: Option<u32>,
    /// Battery pack state: 0=idle, 1=charging, 2=discharging
    pub pack_state: Option<u32>,
    /// Grid connection state: 0=disconnected, 1=connected
    pub grid_state: Option<u32>,
    /// AC inverter status: 0=off, 1=on, 2=standby
    pub ac_status: Option<u32>,
    /// AC mode (RW): 1=charge, 2=discharge
    pub ac_mode: Option<u32>,
    /// AC charging power limit (W) (RW)
    pub input_limit: Option<u32>,
    /// Device output power limit (W) (RW)
    pub output_limit: Option<u32>,
    /// Target SOC in tenths of % (RW) — e.g. 1000 = 100.0%, range 700-1000
    pub soc_set: Option<u32>,
    /// Minimum SOC in tenths of % (RW) — e.g. 100 = 10.0%, range 0-500
    pub min_soc: Option<u32>,
    /// Maximum inverter output power (W) (RW) — e.g. 800
    pub inverse_max_power: Option<u32>,
    /// Reverse flow control (RW): 0=off, 1=on, 2=auto
    pub grid_reverse: Option<u32>,
    /// Grid standard (RW): 0=Germany, 1=UK, ..., 9=Italy
    pub grid_standard: Option<u32>,
    /// Maximum charge power capability (W) — e.g. 2400
    pub charge_max_limit: Option<u32>,
    /// Number of connected battery packs
    pub pack_num: Option<u32>,
    /// WiFi signal strength (dBm), e.g. -71
    pub rssi: Option<i32>,
    /// Enclosure temperature in tenths of Kelvin — e.g. 3001 = 300.1K ≈ 27°C
    pub hyper_tmp: Option<u32>,
    /// Pass-through state: 0=off, 1=on
    pub pass: Option<u32>,
    /// Reverse flow state: 0=off, 1=on
    pub reverse_state: Option<u32>,
    /// Remaining discharge time (minutes)
    pub remain_out_time: Option<u32>,
    /// SOC calibration state: 0=not calibrating, 1=calibrating
    pub soc_status: Option<u32>,
    /// DC state: 0=off, 1=on, 2=standby
    pub dc_status: Option<u32>,
    /// PV input state: 0=off, 1=on
    pub pv_status: Option<u32>,
    /// Data ready flag: 0=not ready, 1=ready
    pub data_ready: Option<u32>,
    /// Fault severity level (0=none)
    pub fault_level: Option<u32>,
    /// Error flag: 0=no error, 1=error
    pub is_error: Option<u32>,
    /// Off-grid output power (W)
    pub grid_off_power: Option<u32>,
    /// Off-grid mode (RW): 0=standard, 1=economic, 2=off
    pub grid_off_mode: Option<u32>,
    /// LED lamp state: 0=off, 1=on
    pub lamp_switch: Option<u32>,
    /// Smart/flash write mode (RW): 0=off, 1=on
    pub smart_mode: Option<u32>,
    /// Phase switch setting
    pub phase_switch: Option<u32>,
    /// Battery calibration time (minutes) (RW)
    pub bat_cal_time: Option<u32>,
    /// AC coupling state bitmask
    pub ac_coupling_state: Option<u32>,
    /// Dry contact relay state: 0=open, 1=closed
    pub dry_node_state: Option<u32>,
    /// Off-grid state: 0=grid-tied, 1=off-grid
    pub off_grid_state: Option<u32>,

    /// Battery voltage in hundredths of V — e.g. 4929 = 49.29V
    #[serde(rename = "BatVolt")]
    pub bat_volt: Option<u32>,
    /// Fan control (RW): 0=off, 1=on
    #[serde(rename = "Fanmode")]
    pub fan_mode: Option<u32>,
    /// Fan speed (RW): 0=auto, 1=low, 2=high
    #[serde(rename = "Fanspeed")]
    pub fan_speed_setting: Option<u32>,
    /// IoT cloud connection state
    #[serde(rename = "IOTState")]
    pub iot_state: Option<u32>,
    /// LCN (local communication network) state
    #[serde(rename = "LCNState")]
    pub lcn_state: Option<u32>,
    /// OTA firmware update state
    #[serde(rename = "OTAState")]
    pub ota_state: Option<u32>,
    /// Voltage-based wakeup setting
    #[serde(rename = "VoltWakeup")]
    pub volt_wakeup: Option<u32>,
    /// Firmware voltage activation threshold (V)
    #[serde(rename = "FMVolt")]
    pub fm_volt: Option<u32>,
}

/// Per-battery-pack data from the `packData` array in ZendureReport.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackData {
    /// Battery pack serial number
    pub sn: Option<String>,
    /// Pack type identifier (e.g. 500)
    pub pack_type: Option<u32>,
    /// Pack state of charge (%) — 0-100
    pub soc_level: Option<u32>,
    /// Pack state: 0=idle, 1=charging, 2=discharging
    pub state: Option<u32>,
    /// Pack power (W), signed
    pub power: Option<i32>,
    /// Maximum cell temperature in tenths of Kelvin — e.g. 3001 = 27°C
    pub max_temp: Option<u32>,
    /// Total pack voltage in hundredths of V — e.g. 4930 = 49.30V
    pub total_vol: Option<u32>,
    /// Battery current (A), 16-bit two's complement
    pub batcur: Option<i32>,
    /// Maximum individual cell voltage in hundredths of V
    pub max_vol: Option<u32>,
    /// Minimum individual cell voltage in hundredths of V
    pub min_vol: Option<u32>,
    /// Firmware version number
    pub soft_version: Option<u32>,
}

/// Request body for POST /properties/write.
#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub struct ZendureWriteRequest {
    /// Device serial number
    pub sn: String,
    /// Key-value pairs of properties to set
    pub properties: serde_json::Value,
}

// --- Control decision ---

/// Battery storage mode: Flash saves ~19W idle power but requires a 5s wake delay.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StorageMode {
    /// smartMode: 1 — active, ready for charge/discharge commands
    Ram,
    /// smartMode: 0 — low-power standby
    Flash,
}

/// What the controller wants the battery to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ControlMode {
    Charge,
    Discharge,
    Idle,
    Standby,
}

impl fmt::Display for ControlMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlMode::Charge => write!(f, "charge"),
            ControlMode::Discharge => write!(f, "discharge"),
            ControlMode::Idle => write!(f, "idle"),
            ControlMode::Standby => write!(f, "standby"),
        }
    }
}

/// Daily cycle counters, published to HA for monitoring relay toggling.
#[derive(Debug, Clone)]
pub struct CycleCounts {
    /// Total mode transitions today
    pub daily_transitions: u32,
    /// Times cooldown suppressed a charge↔discharge toggle today
    pub daily_cooldown_suppressions: u32,
}

/// Output of the controller, published to MQTT for HA graphing.
#[derive(Debug, Clone, Serialize)]
pub struct ControlDecision {
    /// Charge, Discharge, or Idle
    pub mode: ControlMode,
    /// Target power (W): positive = charge, negative = discharge
    pub power_watts: i32,
    /// Human-readable explanation of why this decision was made
    pub reason: String,
    /// Net grid power at time of decision (W): positive = importing, negative = exporting
    pub grid_power: f64,
    /// Solar production at time of decision (W)
    pub solar_power: f64,
}
