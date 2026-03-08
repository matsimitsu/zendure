use std::env;

#[allow(dead_code)]
pub struct Config {
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub mqtt_username: Option<String>,
    pub mqtt_password: Option<String>,
    pub mqtt_client_id: String,
    pub zendure_ip: String,
    pub zendure_sn: String,
    pub meter_topic: String,
    pub solar_topic: String,
    pub ha_publish_prefix: String,
    pub zendure_poll_interval_secs: u64,
    /// Safety margin subtracted from charge power to avoid grid import (W)
    pub charge_margin: i32,
    /// Safety margin subtracted from discharge power (W)
    pub discharge_margin: i32,
    /// Grid power below this triggers charging (W, negative = exporting)
    pub charge_start_threshold: f64,
    /// Grid power above this triggers discharging (W, positive = importing)
    pub discharge_start_threshold: f64,
    /// Minimum seconds before charge↔discharge toggle
    pub min_mode_duration_secs: u64,
    /// Minimum seconds between decisions (API protection)
    pub min_decision_interval_secs: u64,
    /// Minutes of idle before entering standby
    pub idle_timeout_minutes: u64,
    /// Warn when daily cycle count reaches this threshold
    pub cycle_warn_threshold: u32,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let mqtt_host = env::var("MQTT_HOST").map_err(|_| "MQTT_HOST is required")?;
        let zendure_ip = env::var("ZENDURE_IP").map_err(|_| "ZENDURE_IP is required")?;
        let zendure_sn = env::var("ZENDURE_SN").map_err(|_| "ZENDURE_SN is required")?;

        let mqtt_port = env::var("MQTT_PORT")
            .unwrap_or_else(|_| "1883".to_string())
            .parse::<u16>()
            .map_err(|_| "MQTT_PORT must be a valid port number")?;

        Ok(Config {
            mqtt_host,
            mqtt_port,
            mqtt_username: env::var("MQTT_USERNAME").ok(),
            mqtt_password: env::var("MQTT_PASSWORD").ok(),
            mqtt_client_id: env::var("MQTT_CLIENT_ID")
                .unwrap_or_else(|_| "zendure-controller".to_string()),
            zendure_ip,
            zendure_sn,
            meter_topic: env::var("METER_TOPIC").unwrap_or_else(|_| "tele/ISK5MT174".to_string()),
            solar_topic: env::var("SOLAR_TOPIC")
                .unwrap_or_else(|_| "homeassistant/solar/inverter_active_power".to_string()),
            ha_publish_prefix: env::var("HA_PUBLISH_PREFIX")
                .unwrap_or_else(|_| "zendure".to_string()),
            zendure_poll_interval_secs: env::var("ZENDURE_POLL_INTERVAL")
                .unwrap_or_else(|_| "10".to_string())
                .parse::<u64>()
                .map_err(|_| "ZENDURE_POLL_INTERVAL must be a number")?,
            charge_margin: env::var("CHARGE_MARGIN")
                .unwrap_or_else(|_| "50".to_string())
                .parse::<i32>()
                .map_err(|_| "CHARGE_MARGIN must be a number")?,
            discharge_margin: env::var("DISCHARGE_MARGIN")
                .unwrap_or_else(|_| "5".to_string())
                .parse::<i32>()
                .map_err(|_| "DISCHARGE_MARGIN must be a number")?,
            charge_start_threshold: env::var("CHARGE_START_THRESHOLD")
                .unwrap_or_else(|_| "-100.0".to_string())
                .parse::<f64>()
                .map_err(|_| "CHARGE_START_THRESHOLD must be a number")?,
            discharge_start_threshold: env::var("DISCHARGE_START_THRESHOLD")
                .unwrap_or_else(|_| "50.0".to_string())
                .parse::<f64>()
                .map_err(|_| "DISCHARGE_START_THRESHOLD must be a number")?,
            min_mode_duration_secs: env::var("MIN_MODE_DURATION")
                .unwrap_or_else(|_| "10".to_string())
                .parse::<u64>()
                .map_err(|_| "MIN_MODE_DURATION must be a number")?,
            min_decision_interval_secs: env::var("MIN_DECISION_INTERVAL")
                .unwrap_or_else(|_| "5".to_string())
                .parse::<u64>()
                .map_err(|_| "MIN_DECISION_INTERVAL must be a number")?,
            idle_timeout_minutes: env::var("IDLE_TIMEOUT_MINUTES")
                .unwrap_or_else(|_| "5".to_string())
                .parse::<u64>()
                .map_err(|_| "IDLE_TIMEOUT_MINUTES must be a number")?,
            cycle_warn_threshold: env::var("CYCLE_WARN_THRESHOLD")
                .unwrap_or_else(|_| "200".to_string())
                .parse::<u32>()
                .map_err(|_| "CYCLE_WARN_THRESHOLD must be a number")?,
        })
    }
}
