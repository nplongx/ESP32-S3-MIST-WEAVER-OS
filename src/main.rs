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
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use esp_idf_hal::adc::attenuation::DB_11;
use esp_idf_hal::adc::oneshot::config::AdcChannelConfig;
use esp_idf_hal::adc::oneshot::{AdcChannelDriver, AdcDriver};
use esp_idf_hal::gpio::Input;

use sensors::ds18b20::HydroponicTempSensor;
use sensors::jsn_sr04t::JsnSr04t;

mod config;
mod controller;
mod mqtt;
mod pump;
mod sensors;

use config::create_shared_config;
use mqtt::{create_shared_sensor_data, ConnectionState};
use pump::PumpController;

use crate::controller::start_fsm_control_loop;
use crate::sensors::isolated_adc::{convert_voltage_to_ec, convert_voltage_to_ph};

const WIFI_SSID: &str = "Huynh Hong";
const WIFI_PASS: &str = "123443215";
const MQTT_URL: &str = "mqtt://interchange.proxy.rlwy.net:50133";
const DEVICE_ID: &str = "device_001";

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    info!("🚀 Khởi động hệ thống FSM Thủy canh Agitech (Phiên bản 2 Bơm nước độc lập)...");

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

    // --- NHÓM 1 & 2: KHỞI TẠO BƠM / VAN (ĐÃ ÁNH XẠ LẠI CHO ESP32-S3) ---
    let valve_mist = PinDriver::output(peripherals.pins.gpio11)?;
    let osaka_en = PinDriver::output(peripherals.pins.gpio12.degrade_output())?;

    let water_pump_in = LedcDriver::new(
        peripherals.ledc.channel0,
        timer_driver.clone(),
        peripherals.pins.gpio13,
    )?;

    let water_pump_out = LedcDriver::new(
        peripherals.ledc.channel7,
        timer_driver.clone(),
        peripherals.pins.gpio14,
    )?;

    let osaka_rpwm = LedcDriver::new(
        peripherals.ledc.channel1,
        timer_driver.clone(),
        peripherals.pins.gpio15,
    )?;

    let pump_a = LedcDriver::new(
        peripherals.ledc.channel3,
        timer_driver.clone(),
        peripherals.pins.gpio16,
    )?;

    let pump_b = LedcDriver::new(
        peripherals.ledc.channel4,
        timer_driver.clone(),
        peripherals.pins.gpio17,
    )?;

    let pump_ph_up = LedcDriver::new(
        peripherals.ledc.channel5,
        timer_driver.clone(),
        peripherals.pins.gpio18,
    )?;

    let pump_ph_down = LedcDriver::new(
        peripherals.ledc.channel6,
        timer_driver.clone(),
        peripherals.pins.gpio21,
    )?;

    let pump_controller = PumpController::new(
        pump_a,
        pump_b,
        pump_ph_up,
        pump_ph_down,
        valve_mist,
        water_pump_in,
        water_pump_out,
        osaka_en,
        osaka_rpwm,
    )?;

    // ===============================
    // 3.5. Sensor Init & Thread
    // ===============================
    info!("Chuẩn bị các chân Cảm biến...");

    let adc1_periph = peripherals.adc1;
    let ph_pin = peripherals.pins.gpio4;
    let ec_pin = peripherals.pins.gpio5;

    let temp_pin =
        PinDriver::input_output(peripherals.pins.gpio6, esp_idf_hal::gpio::Pull::Up).unwrap();
    let mut ds18b20 = HydroponicTempSensor::new(temp_pin).unwrap();

    let trig_pin = PinDriver::output(peripherals.pins.gpio10).unwrap();
    let echo_pin = PinDriver::input(peripherals.pins.gpio7, esp_idf_hal::gpio::Pull::Up).unwrap();
    let mut jsn_sensor = JsnSr04t::new(trig_pin, echo_pin).unwrap();

    let shared_sensor_data_clone = shared_sensor_data.clone();
    let shared_config_clone = shared_config.clone();

    let fast_sampling_mode = Arc::new(AtomicBool::new(false));
    let fast_sampling_clone = fast_sampling_mode.clone();

    std::thread::Builder::new()
        .stack_size(8192)
        .name("sensor_thread".to_string())
        .spawn(move || {
            let mut adc1 = AdcDriver::new(adc1_periph).unwrap();
            let mut adc_config = AdcChannelConfig::default();
            adc_config.attenuation = DB_11;

            let mut ph_chan = AdcChannelDriver::new(&adc1, ph_pin, &adc_config).unwrap();
            let mut ec_chan = AdcChannelDriver::new(&adc1, ec_pin, &adc_config).unwrap();

            let mut temp_history: VecDeque<f32> = VecDeque::new();
            let mut water_history: VecDeque<f32> = VecDeque::new();
            let mut ph_history: VecDeque<f32> = VecDeque::new();
            let mut ec_history: VecDeque<f32> = VecDeque::new();

            let mut last_valid_temp = 25.0;
            let mut last_valid_water = 380.0;
            let mut last_valid_ph = 7.0;
            let mut last_valid_ec = 1.0;

            info!("🔬 Khởi động luồng đọc cảm biến (Đọc ADC trực tiếp + Moving Average)...");
            loop {
                let config = shared_config_clone.read().unwrap().clone();
                let window_size = config.moving_average_window.max(1) as usize;

                extern "C" {
                    fn read_ds18b20_temperature_from_c() -> f32;
                }

                let mut raw_temp = if config.enable_temp_sensor {
                    last_valid_temp
                } else {
                    0.0
                };

                if config.enable_temp_sensor {
                    let temp_c = unsafe { read_ds18b20_temperature_from_c() };
                    if temp_c > -50.0 && temp_c < 125.0 {
                        raw_temp = temp_c;
                        last_valid_temp = raw_temp;
                    } else {
                        log::warn!(
                            "⚠️ Lỗi từ code C DS18B20, dùng lại giá trị cũ: {:.1}",
                            raw_temp
                        );
                    }
                }

                let mut raw_water = if config.enable_water_level_sensor {
                    last_valid_water
                } else {
                    0.0
                };
                if config.enable_water_level_sensor {
                    if let Some(w) = jsn_sensor.get_distance_cm() {
                        raw_water = w;
                        last_valid_water = w;
                    }
                }

                let mut raw_ph = if config.enable_ph_sensor {
                    last_valid_ph
                } else {
                    0.0
                };
                if config.enable_ph_sensor {
                    match adc1.read(&mut ph_chan) {
                        Ok(raw_val) => {
                            let voltage_mv = raw_val as f32 * 3100.0 / 4095.0;
                            raw_ph = convert_voltage_to_ph(voltage_mv, &config);
                            last_valid_ph = raw_ph;
                        }
                        Err(e) => {
                            warn!(
                                "⚠️ Lỗi đọc thẳng ADC pH, dùng giá trị cũ: {:.2}. Lỗi: {:?}",
                                raw_ph, e
                            );
                        }
                    }
                }

                let mut raw_ec = if config.enable_ec_sensor {
                    last_valid_ec
                } else {
                    0.0
                };
                if config.enable_ec_sensor {
                    match adc1.read(&mut ec_chan) {
                        Ok(raw_val) => {
                            let voltage_mv = raw_val as f32 * 3100.0 / 4095.0;
                            raw_ec = convert_voltage_to_ec(voltage_mv, raw_temp, &config);
                            last_valid_ec = raw_ec;
                        }
                        Err(e) => {
                            warn!(
                                "⚠️ Lỗi đọc thẳng ADC EC, dùng giá trị cũ: {:.2}. Lỗi: {:?}",
                                raw_ec, e
                            );
                        }
                    }
                }

                let mut calc_avg = |history: &mut VecDeque<f32>, new_val: f32| -> f32 {
                    history.push_back(new_val);
                    while history.len() > window_size {
                        history.pop_front();
                    }
                    let sum: f32 = history.iter().sum();
                    sum / (history.len() as f32)
                };

                let avg_temp = calc_avg(&mut temp_history, raw_temp);
                let avg_water = calc_avg(&mut water_history, raw_water);
                let avg_ph = calc_avg(&mut ph_history, raw_ph);
                let avg_ec = calc_avg(&mut ec_history, raw_ec);

                {
                    let mut data = shared_sensor_data_clone.write().unwrap();
                    data.temp_value = avg_temp;
                    data.water_level = avg_water;
                    data.ph_value = avg_ph;
                    data.ec_value = avg_ec;
                }

                info!(
                    "📊 CẢM BIẾN | Nhiệt độ: {:.1}°C | Nước: {:.1}cm | pH: {:.2} | EC: {:.2} mS/cm",
                    avg_temp, avg_water, avg_ph, avg_ec
                );

                let is_fast_mode = fast_sampling_clone.load(Ordering::Relaxed);
                let delay_ms = if is_fast_mode {
                    200
                } else {
                    config.sampling_interval
                };

                thread::sleep(Duration::from_millis(delay_ms));
            }
        })?;

    let fsm_config = shared_config.clone();
    let fsm_sensor_data = shared_sensor_data.clone();
    let fsm_nvs = nvs.clone();

    std::thread::Builder::new()
        .stack_size(10240)
        .name("fsm_thread".to_string())
        .spawn(move || {
            start_fsm_control_loop(
                fsm_config,
                fsm_sensor_data,
                pump_controller,
                fsm_nvs,
                cmd_rx,
                fsm_tx,
                dosing_report_tx,
                sensor_cmd_tx,
            );
        })?;

    // ===============================
    // 4. WiFi Connect & Monitor
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
            let has_ip = wifi
                .sta_netif()
                .get_ip_info()
                .map(|info| !info.ip.is_unspecified())
                .unwrap_or(false);

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
    // 5. Main Connection Event Loop
    // ===============================
    let mut mqtt_client: Option<EspMqttClient> = None;
    let mut is_mqtt_connected = false;

    info!("🔄 Đang chạy Main Event Loop...");

    let mut last_sensor_publish = std::time::Instant::now();
    let mut force_publish_next = false; // 🟢 THÊM BIẾN CỜ ÉP PUBLISH

    loop {
        if let Ok(state) = conn_rx.try_recv() {
            match state {
                ConnectionState::WifiConnected => {
                    info!("🛜 Đã kết nối WiFi. Tiến hành khởi tạo/kiểm tra MQTT...");
                    if mqtt_client.is_none() {
                        match mqtt::init_mqtt_client(
                            MQTT_URL,
                            shared_config.clone(),
                            shared_sensor_data.clone(),
                            cmd_tx.clone(),
                            conn_tx.clone(),
                        ) {
                            Ok(client) => {
                                mqtt_client = Some(client);
                            }
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
                    info!("📡 MQTT Client báo cáo: ĐÃ KẾT NỐI THÀNH CÔNG");
                    is_mqtt_connected = true;

                    if let Some(client) = mqtt_client.as_mut() {
                        let topic_config = format!("AGITECH/{}/controller/config", DEVICE_ID);
                        let topic_command = format!("AGITECH/{}/controller/command", DEVICE_ID);

                        let topic_status = format!("AGITECH/{}/status", DEVICE_ID);
                        let status_payload = r#"{"online": true}"#;
                        let _ = client.publish(
                            &topic_status,
                            QoS::AtLeastOnce,
                            false,
                            status_payload.as_bytes(),
                        );

                        let _ = client.subscribe(&topic_config, QoS::AtLeastOnce);
                        let _ = client.subscribe(&topic_command, QoS::AtLeastOnce);

                        info!("✅ Lệnh Subscribe và báo Online đã được gửi thành công!");
                    }
                }
                ConnectionState::MqttDisconnected => {
                    warn!("📡 MQTT Client báo cáo: MẤT KẾT NỐI");
                    is_mqtt_connected = false;
                }
            }
        }

        // Logic Publish MQTT Định kỳ hoặc Ép buộc
        if is_mqtt_connected {
            let interval_ms = shared_config.read().unwrap().publish_interval;

            // 🟢 THAY ĐỔI ĐIỀU KIỆN PUBLISH: Đợi hết giờ HOẶC có lệnh force_publish
            if force_publish_next || last_sensor_publish.elapsed().as_millis() as u64 >= interval_ms
            {
                if let Some(client) = mqtt_client.as_mut() {
                    let sensors = shared_sensor_data.read().unwrap().clone();
                    let current_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;

                    let pumps = &sensors.pump_status;

                    let payload = format!(
                        r#"{{"device_id":"{}", "temp_value":{:.1}, "ec_value":{:.2}, "ph_value":{:.2}, "water_level":{:.1}, "pump_status": {{"pump_a":{}, "pump_b":{}, "ph_up":{}, "ph_down":{}, "osaka_pump":{}, "water_pump_in":{}, "water_pump_out":{}, "mist_valve":{}}}, "timestamp_ms":{}}}"#,
                        DEVICE_ID,
                        sensors.temp_value,
                        sensors.ec_value,
                        sensors.ph_value,
                        sensors.water_level,
                        pumps.pump_a,
                        pumps.pump_b,
                        pumps.ph_up,
                        pumps.ph_down,
                        pumps.osaka_pump,
                        pumps.water_pump_in,
                        pumps.water_pump_out,
                        pumps.mist_valve,
                        current_ms
                    );

                    let topic = format!("AGITECH/{}/sensors", DEVICE_ID);
                    let _ = client.publish(&topic, QoS::AtMostOnce, false, payload.as_bytes());

                    if force_publish_next {
                        info!("🚀 Đã Publish cưỡng bức (Force Publish) trạng thái hiện tại!");
                    }
                }
                last_sensor_publish = std::time::Instant::now();
                force_publish_next = false; // Reset cờ sau khi publish
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

        // 🟢 CƠ CHẾ COMMAND: Phân loại lệnh nội bộ từ FSM
        if let Ok(sensor_cmd_json) = sensor_cmd_rx.try_recv() {
            if sensor_cmd_json.contains("\"command\":\"force_publish\"") {
                force_publish_next = true; // 🟢 Bật cờ để publish ngay ở vòng lặp tiếp theo
            } else if sensor_cmd_json.contains("\"state\": true")
                || sensor_cmd_json.contains("\"state\":true")
            {
                fast_sampling_mode.store(true, Ordering::Relaxed);
                info!("⚡ FSM YÊU CẦU: Bật chế độ đo Cảm biến liên tục (Fast Sampling)");
            } else {
                fast_sampling_mode.store(false, Ordering::Relaxed);
                info!("🐢 FSM YÊU CẦU: Trở về chế độ đo Cảm biến bình thường");
            }
        }

        thread::sleep(Duration::from_millis(50)); // Giảm xuống 50ms để bắt lệnh nội bộ nhạy hơn
    }
}

