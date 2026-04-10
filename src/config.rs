use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")] // Tự động map "auto" và "manual" từ string của Backend
pub enum ControlMode {
    Auto,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)] // CỰC KỲ QUAN TRỌNG: Lấy giá trị Default nếu JSON từ Backend gửi thiếu field
pub struct DeviceConfig {
    pub device_id: String,
    pub control_mode: ControlMode,
    pub is_enabled: bool,

    // ==========================================
    // 1. NGƯỠNG MỤC TIÊU (Từ Bảng DeviceConfig)
    // ==========================================
    pub ec_target: f32,
    pub ec_tolerance: f32,
    pub ph_target: f32,
    pub ph_tolerance: f32,

    // ==========================================
    // 2. QUẢN LÝ NƯỚC (Từ Bảng WaterConfig)
    // ==========================================
    pub water_level_min: f32,
    pub water_level_target: f32,
    pub water_level_max: f32,
    pub water_level_tolerance: f32,
    pub auto_refill_enabled: bool,
    pub auto_drain_overflow: bool,
    pub auto_dilute_enabled: bool,
    pub dilute_drain_amount_cm: f32,
    pub scheduled_water_change_enabled: bool,
    pub water_change_interval_sec: u64,
    pub scheduled_drain_amount_cm: f32,
    pub misting_on_duration_ms: u64,
    pub misting_off_duration_ms: u64,

    // ==========================================
    // 3. AN TOÀN (Từ Bảng SafetyConfig)
    // ==========================================
    pub emergency_shutdown: bool,
    pub max_ec_limit: f32,
    pub min_ec_limit: f32,
    pub min_ph_limit: f32,
    pub max_ph_limit: f32,
    pub max_ec_delta: f32,
    pub max_ph_delta: f32,
    pub max_dose_per_cycle: f32,
    pub water_level_critical_min: f32,
    pub max_refill_duration_sec: u64,
    pub max_drain_duration_sec: u64,

    pub ec_ack_threshold: f32,
    pub ph_ack_threshold: f32,
    pub water_ack_threshold: f32,

    // ==========================================
    // 4. CHÂM PHÂN (Từ Bảng DosingCalibration)
    // ==========================================
    pub ec_gain_per_ml: f32,
    pub ph_shift_up_per_ml: f32,
    pub ph_shift_down_per_ml: f32,
    pub active_mixing_sec: u64,
    pub sensor_stabilize_sec: u64,
    pub ec_step_ratio: f32,
    pub ph_step_ratio: f32,
    pub dosing_pump_capacity_ml_per_sec: f32,
    pub soft_start_duration: u64,
    pub scheduled_mixing_interval_sec: u64,
    pub scheduled_mixing_duration_sec: u64,

    // ==========================================
    // 5. CẢM BIẾN (Từ Bảng SensorCalibration)
    // ==========================================
    pub ph_v7: f32,
    pub ph_v4: f32,
    pub ec_factor: f32,
    pub ec_offset: f32,
    pub temp_offset: f32,
    pub temp_compensation_beta: f32,

    // --- LỌC NHIỄU & TẦN SUẤT ---
    pub sampling_interval: u64,
    pub publish_interval: u64,
    pub moving_average_window: u32,

    // ==========================================
    // 6. CÁC THÔNG SỐ LOCAL / MỞ RỘNG CỦA ESP32
    // (Không có trong Backend nhưng FSM cần dùng)
    // ==========================================
    pub dosing_pwm_percent: u32,
    pub osaka_mixing_pwm_percent: u32,
    pub osaka_misting_pwm_percent: u32,
    pub misting_temp_threshold: f32,
    pub high_temp_misting_on_duration_ms: u64,
    pub high_temp_misting_off_duration_ms: u64,

    // ==========================================
    // 7. 🟢 CỜ BẬT/TẮT HARDWARE (HARDWARE TOGGLES)
    // Cho phép hệ thống chạy bình thường dù thiếu cảm biến
    // ==========================================
    pub enable_ec_sensor: bool,
    pub enable_ph_sensor: bool,
    pub enable_water_level_sensor: bool,
    pub enable_temp_sensor: bool,
}

// Bảng giá trị Mặc định (Hardcode)
// CHỈ được dùng khi ESP32 vừa mất điện khởi động lên chưa có mạng
// HOẶC khi Backend gửi thiếu Field nào đó.
impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            device_id: "device_001".to_string(),
            control_mode: ControlMode::Manual,
            is_enabled: true,

            ec_target: 1.2,
            ec_tolerance: 0.05,
            ph_target: 6.0,
            ph_tolerance: 0.1,

            water_level_min: 15.0,
            water_level_target: 20.0,
            water_level_max: 24.0,
            water_level_tolerance: 1.0,
            auto_refill_enabled: true,
            auto_drain_overflow: true,
            auto_dilute_enabled: true,
            dilute_drain_amount_cm: 2.0,
            scheduled_water_change_enabled: false,
            water_change_interval_sec: 259200,
            scheduled_drain_amount_cm: 5.0,
            misting_on_duration_ms: 10000,
            misting_off_duration_ms: 180000,

            emergency_shutdown: false,
            max_ec_limit: 3.5,
            min_ec_limit: 1.0,
            min_ph_limit: 4.0,
            max_ph_limit: 8.5,
            max_ec_delta: 1.0,
            max_ph_delta: 1.5,
            max_dose_per_cycle: 2.0,
            water_level_critical_min: 5.0,
            max_refill_duration_sec: 120,
            max_drain_duration_sec: 120,

            ec_ack_threshold: 0.05,
            ph_ack_threshold: 0.1,
            water_ack_threshold: 0.5,

            ec_gain_per_ml: 0.015,
            ph_shift_up_per_ml: 0.02,
            ph_shift_down_per_ml: 0.025,
            active_mixing_sec: 5,
            sensor_stabilize_sec: 5,
            ec_step_ratio: 0.4,
            ph_step_ratio: 0.2,
            dosing_pump_capacity_ml_per_sec: 1.0,
            soft_start_duration: 3000,
            scheduled_mixing_interval_sec: 3600,
            scheduled_mixing_duration_sec: 300,

            ph_v7: 2.5,
            ph_v4: 3.0,
            ec_factor: 880.0,
            ec_offset: 0.0,
            temp_offset: 0.0,
            temp_compensation_beta: 0.02,

            sampling_interval: 1000,   // Lấy mẫu mỗi 1 giây
            publish_interval: 5000,    // Gửi MQTT mỗi 5 giây
            moving_average_window: 10, // Lọc trung bình cộng 10 lần

            dosing_pwm_percent: 50,
            osaka_mixing_pwm_percent: 60,
            osaka_misting_pwm_percent: 100,
            misting_temp_threshold: 30.0,
            high_temp_misting_on_duration_ms: 15000,
            high_temp_misting_off_duration_ms: 60000,

            // Mặc định cho phép toàn bộ cảm biến hoạt động
            enable_ec_sensor: true,
            enable_ph_sensor: true,
            enable_water_level_sensor: true,
            enable_temp_sensor: true,
        }
    }
}

pub type SharedConfig = Arc<RwLock<DeviceConfig>>;

pub fn create_shared_config() -> SharedConfig {
    Arc::new(RwLock::new(DeviceConfig::default()))
}

