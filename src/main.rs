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
use sensors::isolated_adc::{convert_voltage_to_ec, convert_voltage_to_ph, IsolatedPhEcReader};
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

    // --- NHÓM 1 & 2: KHỞI TẠO BƠM / VAN ---
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

    // let osaka_lpwm = LedcDriver::new(
    //     peripherals.ledc.channel2,
    //     timer_driver.clone(),
    //     peripherals.pins.gpioX, // Nếu dùng lại, hãy cấu hình chân khác (vd: gpio43)
    // )?;

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
        // osaka_lpwm,
    )?;

    // ===============================
    // 3.5. Sensor Init & Thread
    // ===============================
    info!("Chuẩn bị các chân Cảm biến...");

    let adc1_periph = peripherals.adc1;

    let pin_gpio4 = peripherals.pins.gpio4; // ADC1_CH3 (pH)
    let pin_gpio5 = peripherals.pins.gpio5; // ADC1_CH4 (EC)
    let pin_gpio6 = peripherals.pins.gpio6; // DS18B20 Temp
    let pin_gpio7 = peripherals.pins.gpio7; // Echo
    let pin_gpio8 = peripherals.pins.gpio8; // Mosfet pH
    let pin_gpio9 = peripherals.pins.gpio9; // Mosfet EC
    let pin_gpio10 = peripherals.pins.gpio10; // Trig

    let shared_sensor_data_clone = shared_sensor_data.clone();
    let shared_config_clone = shared_config.clone();

    // 🟢 TẠO CỜ GIAO TIẾP: TRUE = ĐỌC SIÊU TỐC, FALSE = ĐỌC BÌNH THƯỜNG
    let fast_sampling_mode = Arc::new(AtomicBool::new(false));
    let fast_sampling_clone = fast_sampling_mode.clone();

    // THREAD CHẠY CẢM BIẾN
    std::thread::Builder::new()
        .stack_size(8192) // Cấp phát 8KB Stack
        .name("sensor_thread".to_string())
        .spawn(move || {
            info!("Đang khởi tạo phần cứng Cảm biến...");

            let mut adc1 = AdcDriver::new(adc1_periph).unwrap();

            let mut adc_config = AdcChannelConfig::default();
            adc_config.attenuation = DB_11;

            let mut ph_chan = AdcChannelDriver::new(&adc1, pin_gpio4, &adc_config).unwrap();
            let mut ec_chan = AdcChannelDriver::new(&adc1, pin_gpio5, &adc_config).unwrap();

            let ph_mosfet = PinDriver::output(pin_gpio8).unwrap();
            let ec_mosfet = PinDriver::output(pin_gpio9).unwrap();
            let mut isolated_reader = IsolatedPhEcReader::new(ph_mosfet, ec_mosfet).unwrap();

            let temp_pin = PinDriver::input_output(pin_gpio6, esp_idf_hal::gpio::Pull::Up).unwrap();
            let mut ds18b20 = HydroponicTempSensor::new(temp_pin).unwrap();

            let trig_pin = PinDriver::output(pin_gpio10).unwrap();
            let echo_pin = PinDriver::input(pin_gpio7, esp_idf_hal::gpio::Pull::Up).unwrap();
            let mut jsn_sensor = JsnSr04t::new(trig_pin, echo_pin).unwrap();

            // CÁC BỘ ĐỆM DÀNH CHO MOVING AVERAGE
            let mut temp_history: VecDeque<f32> = VecDeque::new();
            let mut water_history: VecDeque<f32> = VecDeque::new();
            let mut ph_history: VecDeque<f32> = VecDeque::new();
            let mut ec_history: VecDeque<f32> = VecDeque::new();

            info!("🔬 Khởi động luồng đọc cảm biến (Time-Multiplexing + Moving Average)...");
            loop {
                // Lấy config mới nhất ở đầu mỗi vòng lặp để cập nhật cờ
                let config = shared_config_clone.read().unwrap().clone();
                let window_size = config.moving_average_window.max(1) as usize;

                // 1. ĐỌC THÔ TỪ CÁC CẢM BIẾN (Chỉ đọc khi cờ enable = true)

                let mut raw_temp = 25.0; // Mặc định an toàn
                if config.enable_temp_sensor {
                    if let Ok(Some(t)) = ds18b20.read_temperature() {
                        raw_temp = t;
                    }
                }

                let mut raw_water = config.water_level_target; // Giả lập nước đang ở mức tiêu chuẩn
                if config.enable_water_level_sensor {
                    if let Some(w) = jsn_sensor.get_distance_cm() {
                        raw_water = w;
                    } else {
                        // Nếu đọc lỗi, giữ giá trị cũ để tránh nhiễu
                        raw_water = *water_history.back().unwrap_or(&config.water_level_target);
                    }
                }

                let mut raw_ph = config.ph_target; // Giả lập pH chuẩn
                if config.enable_ph_sensor {
                    if let Ok(v_ph) =
                        isolated_reader.read_ph_voltage(|| adc1.read(&mut ph_chan).unwrap_or(0))
                    {
                        raw_ph = convert_voltage_to_ph(v_ph, &config);
                    }
                }

                let mut raw_ec = config.ec_target; // Giả lập EC chuẩn
                if config.enable_ec_sensor {
                    if let Ok(v_ec) =
                        isolated_reader.read_ec_voltage(|| adc1.read(&mut ec_chan).unwrap_or(0))
                    {
                        raw_ec = convert_voltage_to_ec(v_ec, raw_temp, &config);
                    }
                }

                // 2. HÀM CLOSURE ĐỂ CẬP NHẬT LỊCH SỬ & TÍNH TRUNG BÌNH CỘNG
                let mut calc_avg = |history: &mut VecDeque<f32>, new_val: f32| -> f32 {
                    history.push_back(new_val);
                    while history.len() > window_size {
                        history.pop_front();
                    }
                    let sum: f32 = history.iter().sum();
                    sum / (history.len() as f32)
                };

                // 3. ÁP DỤNG MOVING AVERAGE
                let avg_temp = calc_avg(&mut temp_history, raw_temp);
                let avg_water = calc_avg(&mut water_history, raw_water);
                let avg_ph = calc_avg(&mut ph_history, raw_ph);
                let avg_ec = calc_avg(&mut ec_history, raw_ec);

                // 4. LƯU GIÁ TRỊ ĐÃ LỌC VÀO SHARED MEMORY CHO FSM
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

                // 5. THAY ĐỔI TỐC ĐỘ LẤY MẪU DỰA VÀO LỆNH CỦA FSM
                let is_fast_mode = fast_sampling_clone.load(Ordering::Relaxed);
                let delay_ms = if is_fast_mode {
                    200 // Đọc rất nhanh (5 lần/giây) khi bơm đang xả/cấp nước
                } else {
                    config.sampling_interval // Bình thường (vd 1000ms)
                };

                thread::sleep(Duration::from_millis(delay_ms));
            }
        })?;

    let fsm_config = shared_config.clone();
    let fsm_sensor_data = shared_sensor_data.clone();
    let fsm_nvs = nvs.clone();

    std::thread::Builder::new()
        .stack_size(10240) // Cấp hẳn 10KB cho FSM vì đây là "não bộ" của hệ thống
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

                        let _ = client.subscribe(&topic_config, QoS::AtLeastOnce);
                        let _ = client.subscribe(&topic_command, QoS::AtLeastOnce);

                        info!("✅ Lệnh Subscribe đã được gửi thành công!");
                    }
                }
                ConnectionState::MqttDisconnected => {
                    warn!("📡 MQTT Client báo cáo: MẤT KẾT NỐI");
                    is_mqtt_connected = false;
                }
            }
        }

        // Logic Publish MQTT Định kỳ
        if is_mqtt_connected {
            let interval_ms = shared_config.read().unwrap().publish_interval;

            if last_sensor_publish.elapsed().as_millis() as u64 >= interval_ms {
                if let Some(client) = mqtt_client.as_mut() {
                    let sensors = shared_sensor_data.read().unwrap().clone();
                    let current_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;

                    let payload = format!(
                        r#"{{"device_id":"{}", "temp_value":{:.1}, "ec_value":{:.2}, "ph_value":{:.2}, "water_level":{:.1}, "timestamp_ms":{}}}"#,
                        DEVICE_ID, // Frontend cần device_id để check
                        sensors.temp_value,
                        sensors.ec_value,
                        sensors.ph_value,
                        sensors.water_level,
                        current_ms
                    );

                    let topic = format!("AGITECH/{}/sensor/data", DEVICE_ID);
                    let _ = client.publish(&topic, QoS::AtMostOnce, false, payload.as_bytes());
                }
                last_sensor_publish = std::time::Instant::now();
            }
        }

        if let Ok(payload) = fsm_rx.try_recv() {
            if is_mqtt_connected {
                if let Some(client) = mqtt_client.as_mut() {
                    let topic = format!("AGITECH/{}/controller/fsm", DEVICE_ID);
                    let _ = client.publish(&topic, QoS::AtLeastOnce, false, payload.as_bytes());
                }
            }
        }

        if let Ok(report_json) = dosing_report_rx.try_recv() {
            if is_mqtt_connected {
                if let Some(client) = mqtt_client.as_mut() {
                    let topic = format!("AGITECH/{}/controller/dosing_report", DEVICE_ID);
                    let _ = client.publish(&topic, QoS::AtLeastOnce, false, report_json.as_bytes());
                }
            }
        }

        // 🟢 CƠ CHẾ COMMAND MỚI: Bắt lệnh nội bộ từ FSM và chuyển thành Cờ (Flag) trong RAM
        if let Ok(sensor_cmd_json) = sensor_cmd_rx.try_recv() {
            if sensor_cmd_json.contains("\"state\": true")
                || sensor_cmd_json.contains("\"state\":true")
            {
                fast_sampling_mode.store(true, Ordering::Relaxed);
                info!("⚡ FSM YÊU CẦU: Bật chế độ đo Cảm biến liên tục (Fast Sampling)");
            } else {
                fast_sampling_mode.store(false, Ordering::Relaxed);
                info!("🐢 FSM YÊU CẦU: Trở về chế độ đo Cảm biến bình thường");
            }
        }

        thread::sleep(Duration::from_millis(100)); // Nhường CPU
    }
}
