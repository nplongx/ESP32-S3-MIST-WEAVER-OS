use crate::config::{DeviceConfig, SharedConfig};
use esp_idf_svc::mqtt::client::{EspMqttClient, EventPayload, MqttClientConfiguration, QoS};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::sync::{mpsc::Sender, Arc, RwLock};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionState {
    WifiConnected,
    WifiDisconnected,
    MqttConnected,
    MqttDisconnected,
}

// 🟢 MỚI: Thêm cấu trúc trạng thái bơm (Sẽ lưu trữ và gửi lên DB Backend)
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct PumpStatus {
    pub pump_a: bool,
    pub pump_b: bool,
    pub ph_up: bool,
    pub ph_down: bool,
    pub osaka_pump: bool,
    pub mist_valve: bool,
    pub water_pump_in: bool,
    pub water_pump_out: bool,
}

#[derive(Debug, Deserialize)]
pub struct IncomingSensorPayload {
    pub temp: Option<f32>,
    pub ec: Option<f32>,
    pub ph: Option<f32>,
    pub water_level: Option<f32>,
    pub timestamp_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorData {
    pub ec_value: f32,
    pub ph_value: f32,
    pub temp_value: f32,
    pub water_level: f32,
    pub last_update_ms: u64,
    // 🟢 MỚI: Lưu trữ trạng thái bơm
    #[serde(default)]
    pub pump_status: PumpStatus,
}

impl Default for SensorData {
    fn default() -> Self {
        Self {
            ec_value: 0.0,
            ph_value: 7.0,
            temp_value: 25.0,
            water_level: 20.0,
            last_update_ms: 0,
            pump_status: PumpStatus::default(), // Khởi tạo mặc định (Toàn bộ false)
        }
    }
}

pub type SharedSensorData = Arc<RwLock<SensorData>>;

pub fn create_shared_sensor_data() -> SharedSensorData {
    Arc::new(RwLock::new(SensorData::default()))
}

#[derive(Debug, Deserialize, Clone)]
pub struct MqttCommandPayload {
    pub action: String,
    pub pump: String,
    pub duration_sec: Option<u64>,
    pub pwm: Option<u32>,
}

pub fn init_mqtt_client(
    broker_url: &str,
    shared_config: SharedConfig,
    shared_sensor_data: SharedSensorData,
    cmd_tx: Sender<MqttCommandPayload>,
    conn_tx: Sender<ConnectionState>,
) -> anyhow::Result<EspMqttClient<'static>> {
    info!("🚀 Initializing MQTT client...");
    info!("Broker: {}", broker_url);

    let device_id = shared_config.read().unwrap().device_id.to_string();

    let topic_config = format!("AGITECH/{}/controller/config", device_id);
    let topic_command = format!("AGITECH/{}/controller/command", device_id);
    // let topic_sensors = format!("AGITECH/{}/sensor/data", device_id);

    info!("Subscribing topics:");
    info!("Config: {}", topic_config);
    info!("Command: {}", topic_command);
    // info!("Sensors: {}", topic_sensors);

    let topic_config_cb = topic_config.clone();
    let topic_command_cb = topic_command.clone();
    // let topic_sensors_cb = topic_sensors.clone();

    let mqtt_config = MqttClientConfiguration {
        buffer_size: 4096,
        keep_alive_interval: Some(std::time::Duration::from_secs(60)),
        password: Some("53zx37kxq3epbexgqt6rjlce1d0e0gwq"),
        username: Some("long"),
        ..Default::default()
    };

    let client = EspMqttClient::new_cb(broker_url, &mqtt_config, move |event| {
        debug!("📩 MQTT Event Received");

        match event.payload() {
            EventPayload::Connected(_) => {
                info!("✅ MQTT Broker Callback: Connected");
                if let Err(e) = conn_tx.send(ConnectionState::MqttConnected) {
                    error!("Failed to send MQTT connected state: {:?}", e);
                }
            }

            EventPayload::Disconnected => {
                warn!("⚠️ MQTT Broker Callback: Disconnected");
                if let Err(e) = conn_tx.send(ConnectionState::MqttDisconnected) {
                    error!("Failed to send MQTT disconnected state: {:?}", e);
                }
            }

            EventPayload::Received { topic, data, .. } => {
                let topic_str = topic.unwrap_or("");

                // ---- CONFIG UPDATE ----
                if topic_str == topic_config_cb {
                    debug!("⚙️ Processing CONFIG update");
                    match serde_json::from_slice::<DeviceConfig>(data) {
                        Ok(new_config) => {
                            info!("📦 New config received: {:?}", new_config);
                            if let Ok(mut config) = shared_config.write() {
                                *config = new_config;
                                info!("✅ Device config updated");
                            } else {
                                error!("❌ Failed to acquire config write lock");
                            }
                        }
                        Err(e) => error!("❌ Config JSON parse error: {:?}", e),
                    }
                }
                // ---- COMMAND ----
                else if topic_str == topic_command_cb {
                    debug!("🎮 Processing COMMAND");
                    match serde_json::from_slice::<MqttCommandPayload>(data) {
                        Ok(cmd) => {
                            info!("🎯 Command received: {:?}", cmd);
                            if let Err(e) = cmd_tx.send(cmd) {
                                error!("❌ Failed to forward command: {:?}", e);
                            }
                        }
                        Err(e) => error!("❌ Command JSON parse error: {:?}", e),
                    }
                }
                // ---- SENSOR DATA ----
                // else if topic_str == topic_sensors_cb {
                //     debug!("📊 Processing SENSOR data snapshot");
                //
                //     match serde_json::from_slice::<IncomingSensorPayload>(data) {
                //         Ok(payload) => {
                //             if let Ok(mut sensors) = shared_sensor_data.write() {
                //                 if let Some(t) = payload.temp {
                //                     sensors.temp_value = t;
                //                 }
                //                 if let Some(e) = payload.ec {
                //                     sensors.ec_value = e;
                //                 }
                //                 if let Some(p) = payload.ph {
                //                     sensors.ph_value = p;
                //                 }
                //                 if let Some(w) = payload.water_level {
                //                     sensors.water_level = w;
                //                 }
                //                 // Lưu ý: Ta KHÔNG chạm vào sensors.pump_status ở đây
                //                 // Vì Node Cảm Biến không biết trạng thái bơm, Controller mới biết.
                //
                //                 info!(
                //                     "🌱 Sensors Sync: Temp={:.1}°C | EC={:.2} | pH={:.2} | Level={:.1}cm",
                //                     sensors.temp_value, sensors.ec_value, sensors.ph_value, sensors.water_level
                //                 );
                //             } else {
                //                 error!("❌ Failed to acquire sensor write lock");
                //             }
                //         }
                //         Err(e) => {
                //             error!("❌ Sensor JSON parse error: {:?}", e);
                //             if let Ok(payload_str) = std::str::from_utf8(data) {
                //                 error!("Payload received: {}", payload_str);
                //             }
                //         }
                //     }
                // }
            }
            _ => {}
        }
    })?;

    info!("✅ MQTT client initialized");

    Ok(client)
}
