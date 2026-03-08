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
        })
    }
}
