use esp_idf_hal::gpio::PinDriver;
use esp_idf_hal::ledc::config::TimerConfig;
use esp_idf_hal::ledc::{LedcDriver, LedcTimerDriver};
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_hal::units::FromValueType;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::mqtt::client::{EspMqttClient, QoS};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{AuthMethod, ClientConfiguration, Configuration, EspWifi};
use log::{error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

// 🟢 THÊM THƯ VIỆN UART ĐỂ GIAO TIẾP VỚI ESP32-C3
use esp_idf_hal::uart::{config::Config as UartConfig, UartDriver};

mod config;
mod controller;
mod mqtt;
mod pump;
mod sensors; // Vẫn giữ module này để code cũ không báo lỗi (dù không dùng ADC nữa)

use config::create_shared_config;
use mqtt::{create_shared_sensor_data, ConnectionState};
use pump::PumpController;
use crate::controller::start_fsm_control_loop;

const WIFI_SSID: &str = "Huynh Hong";
const WIFI_PASS: &str = "123443215";
const MQTT_URL: &str = "mqtt://interchange.proxy.rlwy.net:50133";
const DEVICE_ID: &str = "device_001";

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    info!("🚀 Khởi động hệ thống FSM Thủy canh Agitech (Phiên bản UART Sensor Node)...");

    let peripherals = Peripherals::take().unwrap();
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let shared_config = create_shared_config();
    let shared_sensor_data = create_shared_sensor_data();

    let (conn_tx, conn_rx) = mpsc::channel::<ConnectionState>();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (fsm_tx, fsm_rx) = mpsc::channel::<String>();
    let (dosing_report_tx, dosing_report_rx) = mpsc::channel::<String>();
    let (sensor_cmd_tx, sensor_cmd_rx) = mpsc::channel::<String>();

    let timer_driver = Arc::new(LedcTimerDriver::new(
        peripherals.ledc.timer0,
        &TimerConfig::new().frequency(20000.Hz()),
    )?);

    // ===============================
    // 1. KHỞI TẠO BƠM VÀ VAN (GIỮ NGUYÊN)
    // ===============================
    let valve_mist = PinDriver::output(peripherals.pins.gpio11)?;
    let osaka_en = PinDriver::output(peripherals.pins.gpio12.degrade_output())?;

    let water_pump_in = LedcDriver::new(peripherals.ledc.channel0, timer_driver.clone(), peripherals.pins.gpio13)?;
    let water_pump_out = LedcDriver::new(peripherals.ledc.channel7, timer_driver.clone(), peripherals.pins.gpio14)?;
    let osaka_rpwm = LedcDriver::new(peripherals.ledc.channel1, timer_driver.clone(), peripherals.pins.gpio15)?;
    let pump_a = LedcDriver::new(peripherals.ledc.channel3, timer_driver.clone(), peripherals.pins.gpio16)?;
    let pump_b = LedcDriver::new(peripherals.ledc.channel4, timer_driver.clone(), peripherals.pins.gpio17)?;
    let pump_ph_up = LedcDriver::new(peripherals.ledc.channel5, timer_driver.clone(), peripherals.pins.gpio18)?;
    let pump_ph_down = LedcDriver::new(peripherals.ledc.channel6, timer_driver.clone(), peripherals.pins.gpio21)?;

    let pump_controller = PumpController::new(
        pump_a, pump_b, pump_ph_up, pump_ph_down,
        valve_mist, water_pump_in, water_pump_out, osaka_en, osaka_rpwm,
    )?;

    // ===============================
    // 2. KHỞI TẠO LUỒNG UART ĐỌC CẢM BIẾN TỪ ESP32-C3
    // ===============================
    info!("🔌 Chuẩn bị giao tiếp UART với mạch ESP32-C3 Sensor Node...");

    // Sử dụng lại GPIO 4 và 5 (vốn là chân ADC cũ) để làm TX, RX
    let uart_tx = peripherals.pins.gpio4; // Nối vào RX của ESP32-C3
    let uart_rx = peripherals.pins.gpio5; // Nối vào TX của ESP32-C3
    let uart_port = peripherals.uart1;

    let shared_sensor_data_clone = shared_sensor_data.clone();
    let shared_config_clone = shared_config.clone();

    let fast_sampling_mode = Arc::new(AtomicBool::new(false));
    let fast_sampling_clone = fast_sampling_mode.clone();

    std::thread::Builder::new()
        .stack_size(8192)
        .name("sensor_uart_thread".to_string())
        .spawn(move || {
            let uart_config = UartConfig::new().baudrate(115200.into());
            let uart = UartDriver::new(
                uart_port,
                uart_tx,
                uart_rx,
                Option::<esp_idf_hal::gpio::AnyIOPin>::None,
                Option::<esp_idf_hal::gpio::AnyIOPin>::None,
                &uart_config,
            ).expect("❌ Lỗi khởi tạo UART1");

            let mut buf = [0u8; 128];
            let mut line_buffer = String::new();
            let mut last_config_hash = String::new();

            info!("🔬 Đang lắng nghe Cảm biến qua UART từ ESP32-C3...");

            loop {
                // 1. GỬI CONFIG XUỐNG ESP32-C3 NẾU CÓ THAY ĐỔI TỪ BACKEND/APP
                // Trong vòng lặp loop của sensor_uart_thread (src/main.rs)
                let config = shared_config_clone.read().unwrap().clone();

                // 🟢 Cập nhật Hash để theo dõi cả trạng thái enable
                let current_config_hash = format!(
                    "{}_{}_{}_{}_{}_{}_{}_{}",
                    config.ph_v7, config.ph_v4, config.ec_factor, config.ec_offset, 
                    config.temp_compensation_beta,
                    config.enable_ph_sensor, config.enable_ec_sensor, config.enable_temp_sensor
                );

                if current_config_hash != last_config_hash {
                    // 🟢 Gửi đầy đủ các cờ enable xuống C3
                    let config_json = format!(
                        r#"{{"ph_v7":{:.1}, "ph_v4":{:.1}, "ec_f":{:.2}, "beta":{:.3}, "en_ph":{}, "en_ec":{}, "en_temp":{}, "en_water":{}}}"#,
                        config.ph_v7, config.ph_v4, config.ec_factor, config.temp_compensation_beta,
                        config.enable_ph_sensor, config.enable_ec_sensor, config.enable_temp_sensor, config.enable_water_level_sensor
                    );
                    
                    let payload = format!("{}\n", config_json);
                    if let Err(e) = uart.write(payload.as_bytes()) {
                        error!("⚠️ Lỗi gửi UART config: {:?}", e);
                    } else {
                        last_config_hash = current_config_hash;
                        info!("🔄 Đã đồng bộ cấu hình & trạng thái cảm biến xuống Node C3");
                    }
                }

                // 2. LẮNG NGHE DỮ LIỆU TỪ ESP32-C3 GỬI LÊN
                match uart.read(&mut buf, 50) { // Timeout 50 ticks
                    Ok(bytes_read) if bytes_read > 0 => {
                        let chunk = String::from_utf8_lossy(&buf[..bytes_read]);
                        for c in chunk.chars() {
                            if c == '\n' {
                                let trimmed = line_buffer.trim();
                                if !trimmed.is_empty() && trimmed.starts_with('{') {
                                    // Parse chuỗi JSON thành dữ liệu
                                    match serde_json::from_str::<serde_json::Value>(trimmed) {
                                        Ok(json_data) => {
                                            let mut data = shared_sensor_data_clone.write().unwrap();
                                            
                                            if let Some(t) = json_data["temp"].as_f64() { data.temp_value = t as f32; }
                                            if let Some(w) = json_data["water"].as_f64() { data.water_level = w as f32; }
                                            if let Some(p) = json_data["ph"].as_f64() { data.ph_value = p as f32; }
                                            if let Some(e) = json_data["ec"].as_f64() { data.ec_value = e as f32; }
                                            
                                            // Đánh dấu thời gian nhận để FSM biết Cảm biến vẫn đang sống
                                            data.last_update_ms = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_millis() as u64;

                                            info!("📊 TỪ NODE C3 | Temp: {:.1}°C | Water: {:.1}cm | pH: {:.2} | EC: {:.2}", 
                                                data.temp_value, data.water_level, data.ph_value, data.ec_value);
                                        }
                                        Err(_) => {
                                            warn!("⚠️ Bỏ qua chuỗi UART lỗi hoặc bị đứt đoạn: {}", trimmed);
                                        }
                                    }
                                }
                                line_buffer.clear();
                            } else {
                                line_buffer.push(c);
                            }
                        }
                    }
                    _ => {} // Bỏ qua nếu không có byte nào
                }

                let is_fast_mode = fast_sampling_clone.load(Ordering::Relaxed);
                let delay_ms = if is_fast_mode { 100 } else { 500 };
                thread::sleep(Duration::from_millis(delay_ms));
            }
        })?;

    // ===============================
    // 3. KHỞI CHẠY BỘ ĐIỀU KHIỂN FSM
    // ===============================
    let fsm_config = shared_config.clone();
    let fsm_sensor_data = shared_sensor_data.clone();
    let fsm_nvs = nvs.clone();

    std::thread::Builder::new()
        .stack_size(10240)
        .name("fsm_thread".to_string())
        .spawn(move || {
            start_fsm_control_loop(
                fsm_config, fsm_sensor_data, pump_controller, fsm_nvs,
                cmd_rx, fsm_tx, dosing_report_tx, sensor_cmd_tx,
            );
        })?;

    // ===============================
    // 4. KẾT NỐI WIFI
    // ===============================
    let mut wifi = EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs.clone()))?;
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: WIFI_SSID.try_into().unwrap(),
        password: WIFI_PASS.try_into().unwrap(),
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    }))?;

    wifi.start()?;
    wifi.connect()?;

    let conn_tx_wifi = conn_tx.clone();
    thread::spawn(move || {
        let mut was_connected = false;
        loop {
            let is_l2_connected = wifi.is_connected().unwrap_or(false);
            let has_ip = wifi.sta_netif().get_ip_info().map(|info| !info.ip.is_unspecified()).unwrap_or(false);
            let is_fully_connected = is_l2_connected && has_ip;

            if is_fully_connected && !was_connected {
                let _ = conn_tx_wifi.send(ConnectionState::WifiConnected);
                was_connected = true;
            } else if !is_fully_connected && was_connected {
                let _ = conn_tx_wifi.send(ConnectionState::WifiDisconnected);
                was_connected = false;
                if !is_l2_connected {
                    let _ = wifi.connect();
                }
            }
            thread::sleep(Duration::from_secs(2));
        }
    });

    // ===============================
    // 5. MAIN EVENT LOOP (MQTT)
    // ===============================
    let mut mqtt_client: Option<EspMqttClient> = None;
    let mut is_mqtt_connected = false;

    info!("🔄 Đang chạy Main Event Loop...");

    let mut last_sensor_publish = std::time::Instant::now();
    let mut force_publish_next = false; 

    loop {
        if let Ok(state) = conn_rx.try_recv() {
            match state {
                ConnectionState::WifiConnected => {
                    info!("🛜 Đã kết nối WiFi. Tiến hành khởi tạo MQTT...");
                    if mqtt_client.is_none() {
                        match mqtt::init_mqtt_client(
                            MQTT_URL, shared_config.clone(), shared_sensor_data.clone(),
                            cmd_tx.clone(), conn_tx.clone(),
                        ) {
                            Ok(client) => mqtt_client = Some(client),
                            Err(e) => error!("❌ Lỗi khởi tạo MQTT: {:?}", e),
                        }
                    }
                }
                ConnectionState::WifiDisconnected => {
                    warn!("⚠️ Rớt mạng WiFi!");
                    is_mqtt_connected = false;
                    mqtt_client = None;
                }
                ConnectionState::MqttConnected => {
                    info!("📡 MQTT Client: ĐÃ KẾT NỐI THÀNH CÔNG");
                    is_mqtt_connected = true;

                    if let Some(client) = mqtt_client.as_mut() {
                        let topic_config = format!("AGITECH/{}/controller/config", DEVICE_ID);
                        let topic_command = format!("AGITECH/{}/controller/command", DEVICE_ID);
                        let topic_status = format!("AGITECH/{}/status", DEVICE_ID);
                        
                        let _ = client.publish(&topic_status, QoS::AtLeastOnce, false, r#"{"online": true}"#.as_bytes());
                        let _ = client.subscribe(&topic_config, QoS::AtLeastOnce);
                        let _ = client.subscribe(&topic_command, QoS::AtLeastOnce);
                    }
                }
                ConnectionState::MqttDisconnected => {
                    warn!("📡 MQTT Client: MẤT KẾT NỐI");
                    is_mqtt_connected = false;
                }
            }
        }

        if is_mqtt_connected {
            let interval_ms = shared_config.read().unwrap().publish_interval;

            if force_publish_next || last_sensor_publish.elapsed().as_millis() as u64 >= interval_ms {
                if let Some(client) = mqtt_client.as_mut() {
                    let sensors = shared_sensor_data.read().unwrap().clone();
                    let current_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;

                    let pumps = &sensors.pump_status;
                    let payload = format!(
                        r#"{{"device_id":"{}", "temp_value":{:.1}, "ec_value":{:.2}, "ph_value":{:.2}, "water_level":{:.1}, "pump_status": {{"pump_a":{}, "pump_b":{}, "ph_up":{}, "ph_down":{}, "osaka_pump":{}, "water_pump_in":{}, "water_pump_out":{}, "mist_valve":{}}}, "timestamp_ms":{}}}"#,
                        DEVICE_ID, sensors.temp_value, sensors.ec_value, sensors.ph_value, sensors.water_level,
                        pumps.pump_a, pumps.pump_b, pumps.ph_up, pumps.ph_down, pumps.osaka_pump, pumps.water_pump_in, pumps.water_pump_out, pumps.mist_valve, current_ms
                    );

                    let topic = format!("AGITECH/{}/sensors", DEVICE_ID);
                    let _ = client.publish(&topic, QoS::AtMostOnce, false, payload.as_bytes());
                }
                last_sensor_publish = std::time::Instant::now();
                force_publish_next = false; 
            }
        }

        if let Ok(payload) = fsm_rx.try_recv() {
            if is_mqtt_connected {
                if let Some(client) = mqtt_client.as_mut() {
                    let topic = format!("AGITECH/{}/fsm", DEVICE_ID);
                    let _ = client.publish(&topic, QoS::AtLeastOnce, false, payload.as_bytes());
                }
            }
        }

        if let Ok(report_json) = dosing_report_rx.try_recv() {
            if is_mqtt_connected {
                if let Some(client) = mqtt_client.as_mut() {
                    let topic = format!("AGITECH/{}/dosing_report", DEVICE_ID);
                    let _ = client.publish(&topic, QoS::AtLeastOnce, false, report_json.as_bytes());
                }
            }
        }

        if let Ok(sensor_cmd_json) = sensor_cmd_rx.try_recv() {
            if sensor_cmd_json.contains("\"command\":\"force_publish\"") {
                force_publish_next = true; 
            } else if sensor_cmd_json.contains("\"state\": true") || sensor_cmd_json.contains("\"state\":true") {
                fast_sampling_mode.store(true, Ordering::Relaxed);
            } else {
                fast_sampling_mode.store(false, Ordering::Relaxed);
            }
        }

        thread::sleep(Duration::from_millis(50)); 
    }
}
