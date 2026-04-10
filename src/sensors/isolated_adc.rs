use crate::config::DeviceConfig;
use esp_idf_hal::gpio::{Output, PinDriver};
use std::thread;
use std::time::Duration;

// Struct giờ đây vô cùng nhẹ nhàng, triệt tiêu toàn bộ ADC Generics
pub struct IsolatedPhEcReader<'a> {
    ph_mosfet: PinDriver<'a, Output>,
    ec_mosfet: PinDriver<'a, Output>,
}

impl<'a> IsolatedPhEcReader<'a> {
    pub fn new(
        mut ph_mosfet: PinDriver<'a, Output>,
        mut ec_mosfet: PinDriver<'a, Output>,
    ) -> anyhow::Result<Self> {
        // Đảm bảo cả 2 MOSFET đều bị ngắt khi vừa khởi động
        ph_mosfet.set_low()?;
        ec_mosfet.set_low()?;

        Ok(Self {
            ph_mosfet,
            ec_mosfet,
        })
    }

    // Truyền một hàm Callback (closure) để đọc ADC thay vì truyền trực tiếp thiết bị
    pub fn read_ph_voltage<F>(&mut self, mut read_adc_callback: F) -> anyhow::Result<f32>
    where
        F: FnMut() -> u16, // Hàm callback sẽ trả về giá trị ADC thô (u16)
    {
        // 1. NGẮT EC, BẬT pH
        self.ec_mosfet.set_low()?;
        self.ph_mosfet.set_high()?;

        // 2. Chờ mạch làm nóng và ổn định điện áp
        thread::sleep(Duration::from_millis(1500));

        // 3. Đọc lấy mẫu nhiều lần để lọc nhiễu
        let mut sum: u32 = 0;
        let samples = 20;
        for _ in 0..samples {
            sum += read_adc_callback() as u32;
            thread::sleep(Duration::from_millis(10));
        }

        // 4. TẮT pH
        self.ph_mosfet.set_low()?;

        let avg_mv = (sum as f32) / (samples as f32);
        let voltage = avg_mv / 1000.0;
        Ok(voltage)
    }

    pub fn read_ec_voltage<F>(&mut self, mut read_adc_callback: F) -> anyhow::Result<f32>
    where
        F: FnMut() -> u16,
    {
        // 1. NGẮT pH, BẬT EC
        self.ph_mosfet.set_low()?;
        self.ec_mosfet.set_high()?;

        // 2. Chờ mạch ổn định
        thread::sleep(Duration::from_millis(1500));

        // 3. Đọc lấy mẫu
        let mut sum: u32 = 0;
        let samples = 20;
        for _ in 0..samples {
            sum += read_adc_callback() as u32;
            thread::sleep(Duration::from_millis(10));
        }

        // 4. TẮT EC
        self.ec_mosfet.set_low()?;

        let avg_mv = (sum as f32) / (samples as f32);
        let voltage = avg_mv / 1000.0;
        Ok(voltage)
    }
}

// =========================================
// CÁC HÀM TÍNH TOÁN HIỆU CHUẨN (Calibration)
// =========================================

pub fn convert_voltage_to_ph(voltage: f32, config: &DeviceConfig) -> f32 {
    let slope = (config.ph_v7 - config.ph_v4) / 3.0;
    if slope.abs() < 0.001 {
        return 7.0;
    }
    let ph_current = 7.0 + (voltage - config.ph_v7) / slope;
    ph_current.clamp(0.0, 14.0)
}

pub fn convert_voltage_to_ec(voltage: f32, temperature_c: f32, config: &DeviceConfig) -> f32 {
    if voltage <= 0.01 {
        return 0.0;
    }
    let ec_raw_us = (voltage * config.ec_factor) + config.ec_offset;

    let real_temp = temperature_c + config.temp_offset;
    let temp_coefficient = 1.0 + config.temp_compensation_beta * (real_temp - 25.0);

    let ec_ms = (ec_raw_us / temp_coefficient) / 1000.0;
    ec_ms.clamp(0.0, 4.4)
}

