//! IR photosensor and IR-cut filter driver (`ak_drv_ir_*`).
//!
//! Used to decide when to flip the ISP between the day and night profiles.

use crate::error::{check, Result};

/// Reading from the light-dependent resistor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightLevel {
    Day,
    Night,
}

/// The IR driver. [`IrSensor::init`] must run before any reading.
#[derive(Debug)]
pub struct IrSensor {
    _private: (),
}

impl IrSensor {
    /// Initialises the driver and reads the IR-cut control mode.
    pub fn init() -> Result<Self> {
        // SAFETY: no arguments, no pointers.
        let ret = unsafe { anyka_sys::ak_drv_ir_init() };
        check("ak_drv_ir_init", ret)?;
        Ok(Self { _private: () })
    }

    /// Current photosensor reading, or `None` while the driver has no value.
    pub fn light_level(&self) -> Option<LightLevel> {
        // SAFETY: no arguments, no pointers. Returns 1 for day, 0 for night
        // and -1 when no reading is available yet.
        match unsafe { anyka_sys::ak_drv_ir_get_input_level() } {
            1 => Some(LightLevel::Day),
            0 => Some(LightLevel::Night),
            _ => None,
        }
    }

    /// Drives the IR-cut filter directly.
    pub fn set_ircut(&self, level: i32) -> Result<()> {
        // SAFETY: plain integer, no pointers.
        let ret = unsafe { anyka_sys::ak_drv_ir_set_ircut(level) };
        check("ak_drv_ir_set_ircut", ret)
    }
}
