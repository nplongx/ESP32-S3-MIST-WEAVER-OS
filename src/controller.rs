use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition, EspNvs};
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{ControlMode, DeviceConfig, SharedConfig};
use crate::mqtt::{MqttCommandPayload, PumpStatus, SensorData};
use crate::pump::{PumpController, PumpType, WaterDirection};

pub type SharedSensorData = Arc<RwLock<SensorData>>;

#[derive(Debug, Clone, PartialEq)]
pub enum PendingDose {
    EC {
        dose_ml: f32,
        duration_ms: u64,
        target_ec: f32,
        pwm_percent: u32,
    },
    PH {
        is_up: bool,
        dose_ml: f32,
        duration_ms: u64,
        target_ph: f32,
        pwm_percent: u32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SystemState {
    Monitoring,
    EmergencyStop,
    SystemFault(String),
    WaterRefilling {
        target_level: f32,
        start_time: u64,
    },
    WaterDraining {
        target_level: f32,
        start_time: u64,
    },
    StartingOsakaPump {
        finish_time: u64,
        pending_action: PendingDose,
    },
    DosingEC {
        finish_time: u64,
        dose_ml: f32,
        target_ec: f32,
        start_ec: f32,
        start_ph: f32,
    },
    DosingPH {
        finish_time: u64,
        is_up: bool,
        dose_ml: f32,
        target_ph: f32,
        start_ec: f32,
        start_ph: f32,
    },
    ActiveMixing {
        finish_time: u64,
    },
    Stabilizing {
        finish_time: u64,
    },
}

impl SystemState {
    pub fn to_payload_string(&self) -> String {
        match self {
            SystemState::Monitoring => "Monitoring".to_string(),
            SystemState::EmergencyStop => "EmergencyStop".to_string(),
            SystemState::SystemFault(reason) => format!("SystemFault:{}", reason),
            SystemState::WaterRefilling { .. } => "WaterRefilling".to_string(),
            SystemState::WaterDraining { .. } => "WaterDraining".to_string(),
            SystemState::DosingEC { .. } => "DosingEC".to_string(),
            SystemState::DosingPH { .. } => "DosingPH".to_string(),
            SystemState::StartingOsakaPump { .. } => "StartingOsakaPump".to_string(),
            SystemState::ActiveMixing { .. } => "ActiveMixing".to_string(),
            SystemState::Stabilizing { .. } => "Stabilizing".to_string(),
        }
    }
}

pub struct ControlContext {
    pub current_state: SystemState,
    pub last_water_change_time: u64,
    pub ec_retry_count: u8,
    pub ph_retry_count: u8,
    pub water_refill_retry_count: u8,
    pub last_ec_before_dosing: Option<f32>,
    pub last_ph_before_dosing: Option<f32>,
    pub last_ph_dosing_is_up: Option<bool>,
    pub last_water_before_refill: Option<f32>,
    pub last_water_before_drain: Option<f32>,
    pub previous_ec_value: Option<f32>,
    pub previous_ph_value: Option<f32>,
    pub last_continuous_level: bool,

    // BIẾN QUẢN LÝ ĐA LUỒNG
    pub is_misting_active: bool,
    pub last_mist_toggle_time: u64,
    pub is_scheduled_mixing_active: bool,
    pub last_mixing_start_sec: u64,
    pub fsm_osaka_active: bool,
    pub current_osaka_pwm: u32,
    pub pump_status: PumpStatus,

    // Quản lý thời gian tự động tắt cho chế độ Manual
    pub manual_timeouts: HashMap<String, u64>,
}

impl Default for ControlContext {
    fn default() -> Self {
        Self {
            current_state: SystemState::Monitoring,
            last_water_change_time: 0,
            ec_retry_count: 0,
            ph_retry_count: 0,
            water_refill_retry_count: 0,
            last_ec_before_dosing: None,
            last_ph_before_dosing: None,
            last_ph_dosing_is_up: None,
            last_water_before_refill: None,
            last_water_before_drain: None,
            previous_ec_value: None,
            previous_ph_value: None,
            last_continuous_level: false,
            is_misting_active: false,
            last_mist_toggle_time: 0,
            is_scheduled_mixing_active: false,
            last_mixing_start_sec: 0,
            fsm_osaka_active: false,
            current_osaka_pwm: 0,
            pump_status: PumpStatus::default(),
            manual_timeouts: HashMap::new(),
        }
    }
}

impl ControlContext {
    fn stop_all_pumps(&mut self, pump_ctrl: &mut PumpController) {
        let _ = pump_ctrl.stop_all();
        self.pump_status = PumpStatus::default();
        self.is_misting_active = false;
        self.is_scheduled_mixing_active = false;
        self.fsm_osaka_active = false;
        self.current_osaka_pwm = 0;
        self.manual_timeouts.clear(); // Hủy toàn bộ timeout khi stop_all
    }

    fn reset_faults(&mut self) {
        self.ec_retry_count = 0;
        self.ph_retry_count = 0;
        self.water_refill_retry_count = 0;
        self.last_ec_before_dosing = None;
        self.last_ph_before_dosing = None;
        self.last_water_before_refill = None;
        self.fsm_osaka_active = false;
        self.current_state = SystemState::Monitoring;
    }

    pub fn turn_off_pump(&mut self, pump_name: &str, pump_ctrl: &mut PumpController) {
        let _ = match pump_name {
            "A" => {
                self.pump_status.pump_a = false;
                pump_ctrl.set_pump_state(PumpType::NutrientA, false)
            }
            "B" => {
                self.pump_status.pump_b = false;
                pump_ctrl.set_pump_state(PumpType::NutrientB, false)
            }
            "PH_UP" => {
                self.pump_status.ph_up = false;
                pump_ctrl.set_pump_state(PumpType::PhUp, false)
            }
            "PH_DOWN" => {
                self.pump_status.ph_down = false;
                pump_ctrl.set_pump_state(PumpType::PhDown, false)
            }
            "OSAKA_PUMP" => {
                self.pump_status.osaka_pump = false;
                pump_ctrl.set_osaka_pump(false)
            }
            "MIST_VALVE" => {
                self.pump_status.mist_valve = false;
                self.is_misting_active = false;
                pump_ctrl.set_mist_valve(false)
            }
            "WATER_PUMP" => {
                self.pump_status.water_pump_in = false;
                pump_ctrl.set_water_pump(WaterDirection::Stop)
            }
            "DRAIN_PUMP" => {
                self.pump_status.water_pump_out = false;
                pump_ctrl.set_water_pump(WaterDirection::Stop)
            }
            _ => Ok(()),
        };
    }

    fn check_and_update_noise(&mut self, sensors: &SensorData, config: &DeviceConfig) -> bool {
        let mut is_noisy = false;
        if config.enable_ec_sensor {
            if let Some(prev_ec) = self.previous_ec_value {
                if (sensors.ec_value - prev_ec).abs() > config.max_ec_delta {
                    warn!("⚠️ Nhiễu EC. Bỏ qua nhịp này!");
                    is_noisy = true;
                }
            }
            self.previous_ec_value = Some(sensors.ec_value);
        }

        if config.enable_ph_sensor {
            if let Some(prev_ph) = self.previous_ph_value {
                if (sensors.ph_value - prev_ph).abs() > config.max_ph_delta {
                    warn!("⚠️ Nhiễu pH. Bỏ qua nhịp này!");
                    is_noisy = true;
                }
            }
            self.previous_ph_value = Some(sensors.ph_value);
        }

        is_noisy
    }

    fn verify_sensor_ack(&mut self, sensors: &SensorData, config: &DeviceConfig) {
        if config.enable_ec_sensor {
            if let Some(last_ec) = self.last_ec_before_dosing {
                if (sensors.ec_value - last_ec) >= config.ec_ack_threshold {
                    self.ec_retry_count = 0;
                } else {
                    self.ec_retry_count += 1;
                    warn!("⚠️ EC không tăng! Lần thử: {}/3", self.ec_retry_count);
                }
                self.last_ec_before_dosing = None;
            }
        }

        if config.enable_ph_sensor {
            if let Some(last_ph) = self.last_ph_before_dosing {
                let is_up = self.last_ph_dosing_is_up.unwrap_or(true);
                let is_ack_ok = if is_up {
                    (sensors.ph_value - last_ph) >= config.ph_ack_threshold
                } else {
                    (last_ph - sensors.ph_value) >= config.ph_ack_threshold
                };
                if is_ack_ok {
                    self.ph_retry_count = 0;
                } else {
                    self.ph_retry_count += 1;
                    warn!("⚠️ pH không đổi hướng! Lần thử: {}/3", self.ph_retry_count);
                }
                self.last_ph_before_dosing = None;
                self.last_ph_dosing_is_up = None;
            }
        }

        if config.enable_water_level_sensor {
            if let Some(w) = self.last_water_before_refill {
                if (sensors.water_level - w) >= config.water_ack_threshold {
                    self.water_refill_retry_count = 0;
                } else {
                    self.water_refill_retry_count += 1;
                    warn!(
                        "⚠️ Mực nước không tăng! Lần thử: {}/3",
                        self.water_refill_retry_count
                    );
                }
                self.last_water_before_refill = None;
            }
            if let Some(w) = self.last_water_before_drain {
                if (w - sensors.water_level) >= config.water_ack_threshold {
                    self.water_refill_retry_count = 0;
                } else {
                    self.water_refill_retry_count += 1;
                    warn!(
                        "⚠️ Mực nước không giảm! Lần thử: {}/3",
                        self.water_refill_retry_count
                    );
                }
                self.last_water_before_drain = None;
            }
        }
    }
}

fn get_current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_millis() as u64
}
fn get_current_time_sec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs()
}

pub fn start_fsm_control_loop(
    shared_config: SharedConfig,
    shared_sensors: SharedSensorData,
    mut pump_ctrl: PumpController,
    nvs_partition: EspDefaultNvsPartition,
    cmd_rx: Receiver<MqttCommandPayload>,
    fsm_mqtt_tx: Sender<String>,
    dosing_report_tx: Sender<String>,
    sensor_cmd_tx: Sender<String>,
) {
    std::thread::spawn(move || {
        let mut ctx = ControlContext::default();
        let mut last_reported_state = "".to_string();

        let mut nvs = EspNvs::new(nvs_partition, "agitech", true).ok();
        let current_time_on_boot = get_current_time_sec();
        ctx.last_water_change_time = nvs
            .as_mut()
            .and_then(|flash| flash.get_u64("last_w_change").unwrap_or(None))
            .unwrap_or_else(|| {
                if let Some(flash) = nvs.as_mut() {
                    let _ = flash.set_u64("last_w_change", current_time_on_boot);
                }
                current_time_on_boot
            });
        ctx.last_mixing_start_sec = current_time_on_boot;

        info!("🚀 Bắt đầu chạy Máy trạng thái (FSM) Đa luồng Hợp nhất...");

        loop {
            std::thread::sleep(Duration::from_secs(1));
            let config = shared_config.read().unwrap().clone();
            let sensors = shared_sensors.read().unwrap().clone();
            let current_time_ms = get_current_time_ms();
            let current_time_sec = current_time_ms / 1000;

            // Xử lý lệnh Manual từ người dùng qua MQTT
            process_mqtt_commands(&cmd_rx, &config, &mut pump_ctrl, &mut ctx, current_time_ms);

            // KIỂM TRA QUÁ HẠN (TIMEOUT) CHO CHẾ ĐỘ MANUAL
            let mut expired_pumps = Vec::new();
            for (pump, &finish_time) in &ctx.manual_timeouts {
                if current_time_ms >= finish_time {
                    expired_pumps.push(pump.clone());
                }
            }
            for pump in expired_pumps {
                ctx.manual_timeouts.remove(&pump);
                info!("⏱️ HẾT GIỜ (SAFE TIMEOUT): Tự động tắt bơm {}!", pump);
                ctx.turn_off_pump(&pump, &mut pump_ctrl);
            }

            let is_sensor_disconnected = sensors.last_update_ms != 0
                && current_time_ms > sensors.last_update_ms
                && (current_time_ms - sensors.last_update_ms) > 30_000;
            if is_sensor_disconnected {
                if ctx.current_state != SystemState::EmergencyStop
                    && !matches!(ctx.current_state, SystemState::SystemFault(_))
                {
                    error!("📡⏳ MẤT KẾT NỐI BOARD CẢM BIẾN! Dừng hệ thống.");
                    ctx.stop_all_pumps(&mut pump_ctrl);
                    ctx.current_state = SystemState::SystemFault("SENSOR_DISCONNECTED".to_string());
                }
                report_state_if_changed(&ctx.current_state, &mut last_reported_state, &fsm_mqtt_tx);
                continue;
            }

            if ctx.check_and_update_noise(&sensors, &config)
                && config.control_mode == ControlMode::Auto
            {
                continue;
            }

            // 🟢 TÍCH HỢP FLAG AN TOÀN VÀO KIỂM TRA NGƯỠNG
            // Tránh báo lỗi giả làm treo hệ thống nếu người dùng đã tắt cờ cảm biến
            let is_water_critical = config.enable_water_level_sensor
                && (sensors.water_level < config.water_level_critical_min);

            let is_ec_out_of_bounds = config.enable_ec_sensor
                && (sensors.ec_value < config.min_ec_limit
                    || sensors.ec_value > config.max_ec_limit);

            let is_ph_out_of_bounds = config.enable_ph_sensor
                && (sensors.ph_value < config.min_ph_limit
                    || sensors.ph_value > config.max_ph_limit);

            if config.emergency_shutdown
                || is_water_critical
                || is_ec_out_of_bounds
                || is_ph_out_of_bounds
            {
                if ctx.current_state != SystemState::EmergencyStop {
                    error!("⚠️ DỪNG KHẨN CẤP Toàn bộ hệ thống! (Vượt ngưỡng an toàn hoặc có lệnh kích hoạt)");
                    ctx.stop_all_pumps(&mut pump_ctrl);
                    ctx.current_state = SystemState::EmergencyStop;
                }
            } else if !config.is_enabled {
                if ctx.current_state != SystemState::Monitoring {
                    ctx.stop_all_pumps(&mut pump_ctrl);
                    ctx.current_state = SystemState::Monitoring;
                }
            } else if ctx.current_state == SystemState::EmergencyStop {
                info!("✅ Hệ thống an toàn trở lại.");
                ctx.current_state = SystemState::Monitoring;
            } else if config.control_mode == ControlMode::Auto {
                // ===============================================
                // 🟢 LUỒNG 1: QUẢN LÝ PHUN SƯƠNG THEO NHIỆT ĐỘ
                // ===============================================
                let is_hot = config.enable_temp_sensor
                    && (sensors.temp_value >= config.misting_temp_threshold);
                let on_duration = if is_hot {
                    config.high_temp_misting_on_duration_ms
                } else {
                    config.misting_on_duration_ms
                };
                let off_duration = if is_hot {
                    config.high_temp_misting_off_duration_ms
                } else {
                    config.misting_off_duration_ms
                };

                if ctx.is_misting_active {
                    if current_time_ms >= ctx.last_mist_toggle_time + on_duration {
                        let _ = pump_ctrl.set_mist_valve(false);
                        ctx.is_misting_active = false;
                        ctx.last_mist_toggle_time = current_time_ms;
                        ctx.pump_status.mist_valve = false;
                    }
                } else {
                    if current_time_ms >= ctx.last_mist_toggle_time + off_duration {
                        let _ = pump_ctrl.set_mist_valve(true);
                        ctx.is_misting_active = true;
                        ctx.last_mist_toggle_time = current_time_ms;
                        ctx.pump_status.mist_valve = true;
                    }
                }

                // ===============================================
                // 🟢 LUỒNG 2: LẬP LỊCH TRỘN (JET MIXING)
                // ===============================================
                if config.scheduled_mixing_interval_sec > 0
                    && config.scheduled_mixing_duration_sec > 0
                {
                    if ctx.is_scheduled_mixing_active {
                        if current_time_sec
                            >= ctx.last_mixing_start_sec + config.scheduled_mixing_duration_sec
                        {
                            info!("✅ Hoàn tất chu kỳ Jet Mixing.");
                            ctx.is_scheduled_mixing_active = false;
                        }
                    } else {
                        if current_time_sec
                            >= ctx.last_mixing_start_sec + config.scheduled_mixing_interval_sec
                        {
                            info!("🔄 Bắt đầu chu kỳ Jet Mixing định kỳ...");
                            ctx.is_scheduled_mixing_active = true;
                            ctx.last_mixing_start_sec = current_time_sec;
                        }
                    }
                } else {
                    ctx.is_scheduled_mixing_active = false;
                }

                // ===============================================
                // 🟢 LUỒNG 3: FSM CHÂM PHÂN & BÙ NƯỚC
                // ===============================================
                if !matches!(ctx.current_state, SystemState::SystemFault(_)) {
                    run_auto_fsm(
                        current_time_ms,
                        &config,
                        &sensors,
                        &mut ctx,
                        &mut pump_ctrl,
                        &mut nvs,
                        &dosing_report_tx,
                    );
                }

                // ===============================================
                // 🟢 TRÁI TIM HỢP NHẤT: BƠM OSAKA VỚI PWM ĐỘNG
                // ===============================================
                let needs_osaka =
                    ctx.fsm_osaka_active || ctx.is_misting_active || ctx.is_scheduled_mixing_active;

                if needs_osaka {
                    let target_pwm = if ctx.is_misting_active {
                        config.osaka_misting_pwm_percent
                    } else {
                        config.osaka_mixing_pwm_percent
                    };

                    if !ctx.pump_status.osaka_pump {
                        let _ = pump_ctrl.start_osaka_pump_soft(target_pwm);
                        ctx.pump_status.osaka_pump = true;
                        ctx.current_osaka_pwm = target_pwm;
                    } else if ctx.current_osaka_pwm != target_pwm {
                        info!(
                            "🔄 Đang điều chỉnh tốc độ Osaka sang {}% (Tránh xung đột tài nguyên)",
                            target_pwm
                        );
                        let _ = pump_ctrl.set_osaka_pump_pwm(target_pwm);
                        ctx.current_osaka_pwm = target_pwm;
                    }
                } else {
                    if ctx.pump_status.osaka_pump {
                        let _ = pump_ctrl.set_osaka_pump_pwm(0);
                        ctx.pump_status.osaka_pump = false;
                        ctx.current_osaka_pwm = 0;
                    }
                }
            } else if !matches!(
                ctx.current_state,
                SystemState::Monitoring | SystemState::SystemFault(_)
            ) {
                info!("Chuyển sang chế độ MANUAL.");
                ctx.stop_all_pumps(&mut pump_ctrl);
                ctx.current_state = SystemState::Monitoring;
            }

            // Gửi cờ continuous level cho ESP32 sensor node
            let needs_continuous = matches!(
                ctx.current_state,
                SystemState::WaterRefilling { .. } | SystemState::WaterDraining { .. }
            );
            if needs_continuous != ctx.last_continuous_level {
                let payload = format!(
                    r#"{{"command":"continuous_level", "state": {}}}"#,
                    needs_continuous
                );
                if let Err(e) = sensor_cmd_tx.send(payload) {
                    error!("❌ Lỗi gửi lệnh continuous_level: {:?}", e);
                }
                ctx.last_continuous_level = needs_continuous;
            }

            report_state_if_changed(&ctx.current_state, &mut last_reported_state, &fsm_mqtt_tx);

            if let Ok(mut sensors_lock) = shared_sensors.write() {
                sensors_lock.pump_status = ctx.pump_status.clone();
            }
        }
    });
}

fn report_state_if_changed(
    current_state: &SystemState,
    last_reported_state: &mut String,
    fsm_mqtt_tx: &Sender<String>,
) {
    let current_state_str = current_state.to_payload_string();
    if current_state_str != *last_reported_state {
        let payload = format!(r#"{{"current_state": "{}"}}"#, current_state_str);
        if fsm_mqtt_tx.send(payload).is_ok() {
            info!("📡 Trạng thái FSM: [{}]", current_state_str);
        }
        *last_reported_state = current_state_str;
    }
}

fn process_mqtt_commands(
    cmd_rx: &Receiver<MqttCommandPayload>,
    config: &DeviceConfig,
    pump_ctrl: &mut PumpController,
    ctx: &mut ControlContext,
    current_time_ms: u64,
) {
    while let Ok(cmd) = cmd_rx.try_recv() {
        if cmd.action == "reset_fault" {
            info!("🔄 Nhận lệnh Reset. Khôi phục hệ thống...");
            ctx.stop_all_pumps(pump_ctrl);
            ctx.reset_faults();
            continue;
        }

        if config.control_mode == ControlMode::Auto {
            warn!("Bỏ qua lệnh thủ công ({}) vì đang ở AUTO.", cmd.pump);
            continue;
        }

        if cmd.action == "set_pwm" {
            if let Some(pwm_val) = cmd.pwm {
                info!("🕹️ MANUAL MODE: Lệnh SET_PWM {} = {}%", cmd.pump, pwm_val);
                let is_on = pwm_val > 0;

                if is_on {
                    if let Some(duration) = cmd.duration_sec {
                        let finish_time = current_time_ms + (duration as u64 * 1000);
                        ctx.manual_timeouts.insert(cmd.pump.clone(), finish_time);
                        info!(
                            "⏱️ Cài đặt an toàn: Bơm {} sẽ tắt sau {} giây.",
                            cmd.pump, duration
                        );
                    }
                } else {
                    ctx.manual_timeouts.remove(&cmd.pump);
                }

                let _ = match cmd.pump.as_str() {
                    "OSAKA_PUMP" => {
                        ctx.pump_status.osaka_pump = is_on;
                        pump_ctrl.set_osaka_pump_pwm(pwm_val)
                    }
                    "A" => {
                        ctx.pump_status.pump_a = is_on;
                        pump_ctrl.set_dosing_pump_pwm(PumpType::NutrientA, is_on, pwm_val)
                    }
                    "B" => {
                        ctx.pump_status.pump_b = is_on;
                        pump_ctrl.set_dosing_pump_pwm(PumpType::NutrientB, is_on, pwm_val)
                    }
                    "PH_UP" => {
                        ctx.pump_status.ph_up = is_on;
                        pump_ctrl.set_dosing_pump_pwm(PumpType::PhUp, is_on, pwm_val)
                    }
                    "PH_DOWN" => {
                        ctx.pump_status.ph_down = is_on;
                        pump_ctrl.set_dosing_pump_pwm(PumpType::PhDown, is_on, pwm_val)
                    }
                    _ => {
                        warn!("Bơm {} không hỗ trợ điều khiển tốc độ (PWM)!", cmd.pump);
                        Ok(())
                    }
                };
            }
            continue;
        }

        let is_on = cmd.action == "pump_on";
        info!(
            "🕹️ MANUAL MODE: Lệnh {} = {}",
            cmd.pump,
            if is_on { "BẬT" } else { "TẮT" }
        );

        if is_on {
            if let Some(duration) = cmd.duration_sec {
                let finish_time = current_time_ms + (duration as u64 * 1000);
                ctx.manual_timeouts.insert(cmd.pump.clone(), finish_time);
                info!(
                    "⏱️ Cài đặt an toàn: Bơm {} sẽ tắt sau {} giây.",
                    cmd.pump, duration
                );
            }
        } else {
            ctx.manual_timeouts.remove(&cmd.pump);
        }

        let _ = match cmd.pump.as_str() {
            "A" => {
                ctx.pump_status.pump_a = is_on;
                pump_ctrl.set_pump_state(PumpType::NutrientA, is_on)
            }
            "B" => {
                ctx.pump_status.pump_b = is_on;
                pump_ctrl.set_pump_state(PumpType::NutrientB, is_on)
            }
            "PH_UP" => {
                ctx.pump_status.ph_up = is_on;
                pump_ctrl.set_pump_state(PumpType::PhUp, is_on)
            }
            "PH_DOWN" => {
                ctx.pump_status.ph_down = is_on;
                pump_ctrl.set_pump_state(PumpType::PhDown, is_on)
            }
            "OSAKA_PUMP" => {
                ctx.pump_status.osaka_pump = is_on;
                pump_ctrl.set_osaka_pump(is_on)
            }
            "MIST_VALVE" => {
                ctx.pump_status.mist_valve = is_on;
                ctx.is_misting_active = is_on;
                ctx.last_mist_toggle_time = current_time_ms;
                pump_ctrl.set_mist_valve(is_on)
            }
            "WATER_PUMP" => {
                ctx.pump_status.water_pump_in = is_on;
                if is_on {
                    ctx.pump_status.water_pump_out = false;
                }
                pump_ctrl.set_water_pump(if is_on {
                    WaterDirection::In
                } else {
                    WaterDirection::Stop
                })
            }
            "DRAIN_PUMP" => {
                ctx.pump_status.water_pump_out = is_on;
                if is_on {
                    ctx.pump_status.water_pump_in = false;
                }
                pump_ctrl.set_water_pump(if is_on {
                    WaterDirection::Out
                } else {
                    WaterDirection::Stop
                })
            }
            _ => {
                warn!("Tên bơm không hợp lệ");
                Ok(())
            }
        };
    }
}

// ==========================================
// HÀM HỖ TRỢ: CHẠY LOGIC AUTO FSM
// ==========================================
fn run_auto_fsm(
    current_time_ms: u64,
    config: &DeviceConfig,
    sensors: &SensorData,
    ctx: &mut ControlContext,
    pump_ctrl: &mut PumpController,
    nvs: &mut Option<EspDefaultNvs>,
    dosing_report_tx: &Sender<String>,
) {
    let current_time_sec = current_time_ms / 1000;

    match ctx.current_state {
        SystemState::SystemFault(ref reason) => {
            warn!("🚨 BÁO LỖI: [{}]. Chờ reset...", reason);
        }

        SystemState::Monitoring => {
            ctx.verify_sensor_ack(sensors, config);

            // 1. Kiểm tra lịch thay nước
            if config.scheduled_water_change_enabled
                && (current_time_sec - ctx.last_water_change_time
                    > config.water_change_interval_sec)
            {
                let target = (sensors.water_level - config.scheduled_drain_amount_cm)
                    .max(config.water_level_min);
                ctx.last_water_change_time = current_time_sec;
                if let Some(flash) = nvs.as_mut() {
                    let _ = flash.set_u64("last_w_change", current_time_sec);
                }
                ctx.current_state = SystemState::WaterDraining {
                    target_level: target,
                    start_time: current_time_ms,
                };
                let _ = pump_ctrl.set_water_pump(WaterDirection::Out);
                ctx.pump_status.water_pump_out = true;
                ctx.pump_status.water_pump_in = false;
                ctx.fsm_osaka_active = false;
            }
            // 2. Cấp nước tự động
            else if config.auto_refill_enabled
                && sensors.water_level < (config.water_level_target - config.water_level_tolerance)
            {
                if ctx.water_refill_retry_count >= 3 {
                    ctx.stop_all_pumps(pump_ctrl);
                    ctx.current_state = SystemState::SystemFault("WATER_REFILL_FAILED".to_string());
                } else {
                    ctx.last_water_before_refill = Some(sensors.water_level);
                    ctx.current_state = SystemState::WaterRefilling {
                        target_level: config.water_level_target,
                        start_time: current_time_ms,
                    };
                    let _ = pump_ctrl.set_water_pump(WaterDirection::In);
                    ctx.pump_status.water_pump_in = true;
                    ctx.pump_status.water_pump_out = false;
                    ctx.fsm_osaka_active = false;
                }
            }
            // 3. Xả tràn tự động
            else if config.auto_drain_overflow && sensors.water_level > config.water_level_max {
                ctx.current_state = SystemState::WaterDraining {
                    target_level: config.water_level_target,
                    start_time: current_time_ms,
                };
                let _ = pump_ctrl.set_water_pump(WaterDirection::Out);
                ctx.pump_status.water_pump_out = true;
                ctx.pump_status.water_pump_in = false;
                ctx.fsm_osaka_active = false;
            }
            // 4. Pha loãng EC
            else if config.auto_dilute_enabled
                && sensors.ec_value > (config.ec_target + config.ec_tolerance)
            {
                let target = (sensors.water_level - config.dilute_drain_amount_cm)
                    .max(config.water_level_min);
                ctx.current_state = SystemState::WaterDraining {
                    target_level: target,
                    start_time: current_time_ms,
                };
                let _ = pump_ctrl.set_water_pump(WaterDirection::Out);
                ctx.pump_status.water_pump_out = true;
                ctx.pump_status.water_pump_in = false;
                ctx.fsm_osaka_active = false;
            } else {
                let mut is_dosing_active = false;

                // 5. Tính toán châm EC
                if sensors.ec_value < (config.ec_target - config.ec_tolerance) {
                    if ctx.ec_retry_count >= 3 {
                        ctx.stop_all_pumps(pump_ctrl);
                        ctx.current_state =
                            SystemState::SystemFault("EC_DOSING_FAILED".to_string());
                        is_dosing_active = true;
                    } else {
                        let safe_pwm = config.dosing_pwm_percent.clamp(1, 100);
                        let active_capacity =
                            config.dosing_pump_capacity_ml_per_sec * (safe_pwm as f32 / 100.0);
                        let dose_ml = ((config.ec_target - sensors.ec_value)
                            / config.ec_gain_per_ml
                            * config.ec_step_ratio)
                            .clamp(0.0, config.max_dose_per_cycle);
                        let duration_ms = ((dose_ml / active_capacity) * 1000.0) as u64;

                        if duration_ms > 0 {
                            ctx.last_ec_before_dosing = Some(sensors.ec_value);
                            ctx.current_state = SystemState::StartingOsakaPump {
                                finish_time: current_time_ms + config.soft_start_duration,
                                pending_action: PendingDose::EC {
                                    dose_ml,
                                    duration_ms,
                                    target_ec: config.ec_target,
                                    pwm_percent: safe_pwm,
                                },
                            };
                            ctx.fsm_osaka_active = true;
                            is_dosing_active = true;
                        }
                    }
                }

                // 6. Tính toán chỉnh pH
                if !is_dosing_active
                    && (sensors.ph_value - config.ph_target).abs() > config.ph_tolerance
                {
                    if ctx.ph_retry_count >= 3 {
                        ctx.stop_all_pumps(pump_ctrl);
                        ctx.current_state =
                            SystemState::SystemFault("PH_DOSING_FAILED".to_string());
                        is_dosing_active = true;
                    } else {
                        let is_ph_up = sensors.ph_value < config.ph_target;
                        let diff = (sensors.ph_value - config.ph_target).abs();
                        let ratio = if is_ph_up {
                            config.ph_shift_up_per_ml
                        } else {
                            config.ph_shift_down_per_ml
                        };

                        let safe_pwm = config.dosing_pwm_percent.clamp(1, 100);
                        let active_capacity =
                            config.dosing_pump_capacity_ml_per_sec * (safe_pwm as f32 / 100.0);
                        let dose_ml = (diff / ratio * config.ph_step_ratio)
                            .clamp(0.0, config.max_dose_per_cycle);
                        let duration_ms = ((dose_ml / active_capacity) * 1000.0) as u64;

                        if duration_ms > 0 {
                            ctx.last_ph_before_dosing = Some(sensors.ph_value);
                            ctx.last_ph_dosing_is_up = Some(is_ph_up);

                            ctx.current_state = SystemState::StartingOsakaPump {
                                finish_time: current_time_ms + config.soft_start_duration,
                                pending_action: PendingDose::PH {
                                    is_up: is_ph_up,
                                    dose_ml,
                                    duration_ms,
                                    target_ph: config.ph_target,
                                    pwm_percent: safe_pwm,
                                },
                            };
                            ctx.fsm_osaka_active = true;
                            is_dosing_active = true;
                        }
                    }
                }

                if !is_dosing_active {
                    ctx.fsm_osaka_active = false;
                }
            }
        }

        SystemState::WaterRefilling {
            target_level,
            start_time,
        } => {
            if sensors.water_level >= target_level
                || (current_time_ms - start_time) > (config.max_refill_duration_sec as u64 * 1000)
            {
                let _ = pump_ctrl.set_water_pump(WaterDirection::Stop);
                ctx.pump_status.water_pump_in = false;
                ctx.pump_status.water_pump_out = false;
                ctx.fsm_osaka_active = true;
                ctx.current_state = SystemState::ActiveMixing {
                    finish_time: current_time_ms + (config.active_mixing_sec as u64 * 1000),
                };
            }
        }

        SystemState::WaterDraining {
            target_level,
            start_time,
        } => {
            if sensors.water_level <= target_level
                || (current_time_ms - start_time) > (config.max_drain_duration_sec as u64 * 1000)
            {
                let _ = pump_ctrl.set_water_pump(WaterDirection::Stop);
                ctx.pump_status.water_pump_in = false;
                ctx.pump_status.water_pump_out = false;
                ctx.fsm_osaka_active = false;
                ctx.current_state = SystemState::Stabilizing {
                    finish_time: current_time_ms + (config.sensor_stabilize_sec as u64 * 1000),
                };
            }
        }

        SystemState::StartingOsakaPump {
            finish_time,
            ref pending_action,
        } => {
            if current_time_ms >= finish_time {
                let action = pending_action.clone();
                match action {
                    PendingDose::EC {
                        dose_ml,
                        duration_ms,
                        target_ec,
                        pwm_percent,
                    } => {
                        let _ =
                            pump_ctrl.set_dosing_pump_pwm(PumpType::NutrientA, true, pwm_percent);
                        let _ =
                            pump_ctrl.set_dosing_pump_pwm(PumpType::NutrientB, true, pwm_percent);
                        ctx.pump_status.pump_a = true;
                        ctx.pump_status.pump_b = true;

                        ctx.current_state = SystemState::DosingEC {
                            finish_time: current_time_ms + duration_ms,
                            dose_ml,
                            target_ec,
                            start_ec: sensors.ec_value,
                            start_ph: sensors.ph_value,
                        };
                    }
                    PendingDose::PH {
                        is_up,
                        dose_ml,
                        duration_ms,
                        target_ph,
                        pwm_percent,
                    } => {
                        let _ = pump_ctrl.set_dosing_pump_pwm(
                            if is_up {
                                PumpType::PhUp
                            } else {
                                PumpType::PhDown
                            },
                            true,
                            pwm_percent,
                        );
                        if is_up {
                            ctx.pump_status.ph_up = true;
                        } else {
                            ctx.pump_status.ph_down = true;
                        }

                        ctx.current_state = SystemState::DosingPH {
                            finish_time: current_time_ms + duration_ms,
                            is_up,
                            dose_ml,
                            target_ph,
                            start_ec: sensors.ec_value,
                            start_ph: sensors.ph_value,
                        };
                    }
                }
            }
        }

        SystemState::DosingEC {
            finish_time,
            dose_ml,
            target_ec,
            start_ph,
            start_ec,
        } => {
            if current_time_ms >= finish_time {
                let _ = pump_ctrl.set_dosing_pump_pwm(PumpType::NutrientA, false, 0);
                ctx.pump_status.pump_a = false;
                let _ = pump_ctrl.set_dosing_pump_pwm(PumpType::NutrientB, false, 0);
                ctx.pump_status.pump_b = false;

                let report_json = format!(
                    r#"{{"start_ec":{:.2},"start_ph":{:.2},"pump_a_ml":{:.2},"pump_b_ml":{:.2},"ph_up_ml":0.0,"ph_down_ml":0.0,"target_ec":{:.2},"target_ph":{:.2}}}"#,
                    start_ec, start_ph, dose_ml, dose_ml, target_ec, config.ph_target
                );
                let _ = dosing_report_tx.send(report_json);
                ctx.current_state = SystemState::ActiveMixing {
                    finish_time: current_time_ms + (config.active_mixing_sec as u64 * 1000),
                };
            }
        }

        SystemState::DosingPH {
            finish_time,
            is_up,
            dose_ml,
            target_ph,
            start_ec,
            start_ph,
        } => {
            if current_time_ms >= finish_time {
                let _ = pump_ctrl.set_dosing_pump_pwm(PumpType::PhUp, false, 0);
                ctx.pump_status.ph_up = false;
                let _ = pump_ctrl.set_dosing_pump_pwm(PumpType::PhDown, false, 0);
                ctx.pump_status.ph_down = false;

                let ph_up_ml = if is_up { dose_ml } else { 0.0 };
                let ph_down_ml = if !is_up { dose_ml } else { 0.0 };
                let report_json = format!(
                    r#"{{"start_ec":{:.2},"start_ph":{:.2},"pump_a_ml":0.0,"pump_b_ml":0.0,"ph_up_ml":{:.2},"ph_down_ml":{:.2},"target_ec":{:.2},"target_ph":{:.2}}}"#,
                    start_ec, start_ph, ph_up_ml, ph_down_ml, config.ec_target, target_ph
                );
                let _ = dosing_report_tx.send(report_json);
                ctx.current_state = SystemState::ActiveMixing {
                    finish_time: current_time_ms + (config.active_mixing_sec as u64 * 1000),
                };
            }
        }

        SystemState::ActiveMixing { finish_time } => {
            if current_time_ms >= finish_time {
                ctx.fsm_osaka_active = false;
                ctx.current_state = SystemState::Stabilizing {
                    finish_time: current_time_ms + (config.sensor_stabilize_sec as u64 * 1000),
                };
            }
        }

        SystemState::Stabilizing { finish_time } => {
            if current_time_ms >= finish_time {
                ctx.current_state = SystemState::Monitoring;
            }
        }

        SystemState::EmergencyStop => {}
    }
}

