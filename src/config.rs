use crate::Result;
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoraeMode {
    Charging,
    #[default]
    Smart,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    Loop,
    #[default]
    Event,
}
fn yes() -> bool {
    true
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    pub revision: u64,
    pub enabled: bool,
    #[serde(default)]
    pub write_mode: WriteMode,
    #[serde(default)]
    pub detailed_logging: bool,
    #[serde(default)]
    pub global_enabled: bool,
    pub horae_stop: bool,
    pub charge_trigger: bool,
    #[serde(default = "yes")]
    pub charge_horae_enabled: bool,
    #[serde(default)]
    pub charge_pps_stability: bool,
    #[serde(default)]
    pub charge_horae_mode: HoraeMode,
    pub batt_temp_mc: i32,
    pub cpu_temp_mc: i32,
    pub gpu_temp_mc: i32,
    pub ddr_temp_mc: i32,
    pub charge_batt_temp_mc: i32,
    pub charge_cpu_temp_mc: i32,
    pub charge_gpu_temp_mc: i32,
    pub charge_ddr_temp_mc: i32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Temperatures {
    pub battery: i32,
    pub cpu: i32,
    pub gpu: i32,
    pub ddr: i32,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            schema: 3,
            revision: 0,
            enabled: true,
            write_mode: WriteMode::Event,
            detailed_logging: false,
            global_enabled: false,
            horae_stop: true,
            charge_trigger: true,
            charge_horae_enabled: true,
            charge_pps_stability: false,
            charge_horae_mode: HoraeMode::Smart,
            batt_temp_mc: 34000,
            cpu_temp_mc: 40000,
            gpu_temp_mc: 40000,
            ddr_temp_mc: 40000,
            charge_batt_temp_mc: 30000,
            charge_cpu_temp_mc: 35000,
            charge_gpu_temp_mc: 35000,
            charge_ddr_temp_mc: 35000,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.schema != 3 || self.revision > 9_000_000_000_000 {
            return Err("config_schema_or_revision_invalid".into());
        }
        for (v, max) in [
            (self.batt_temp_mc, 40000),
            (self.cpu_temp_mc, 60000),
            (self.gpu_temp_mc, 60000),
            (self.ddr_temp_mc, 55000),
            (self.charge_batt_temp_mc, 40000),
            (self.charge_cpu_temp_mc, 60000),
            (self.charge_gpu_temp_mc, 60000),
            (self.charge_ddr_temp_mc, 55000),
        ] {
            if !(20000..=max).contains(&v) || v % 1000 != 0 {
                return Err("temperature_range_or_step_invalid".into());
            }
        }
        Ok(())
    }
    pub fn select(&self, status: &str) -> (&'static str, Temperatures) {
        if self.charge_trigger && matches!(status.trim(), "Charging" | "Full") {
            (
                "charging",
                Temperatures {
                    battery: self.charge_batt_temp_mc,
                    cpu: self.charge_cpu_temp_mc,
                    gpu: self.charge_gpu_temp_mc,
                    ddr: self.charge_ddr_temp_mc,
                },
            )
        } else {
            (
                if self.global_enabled {
                    "global"
                } else {
                    "idle"
                },
                Temperatures {
                    battery: self.batt_temp_mc,
                    cpu: self.cpu_temp_mc,
                    gpu: self.gpu_temp_mc,
                    ddr: self.ddr_temp_mc,
                },
            )
        }
    }
    pub fn effective(&self, status: &str, smart_permits_horae: bool) -> Self {
        let mut c = self.clone();
        c.charge_pps_stability = self.charge_pps_stability
            && self.select(status).0 == "charging"
            && status.trim() == "Charging";
        match self.select(status).0 {
            "charging" => {
                c.horae_stop = self.charge_horae_enabled
                    && (self.charge_horae_mode == HoraeMode::Charging || smart_permits_horae)
            }
            "global" => (),
            _ => c.enabled = false,
        }
        c
    }
    pub fn migrate(&mut self) -> bool {
        if self.schema != 2 {
            return false;
        }
        self.schema = 3;
        self.revision += 1;
        self.global_enabled = false;
        self.charge_trigger = true;
        self.charge_horae_enabled = self.horae_stop;
        self.charge_horae_mode = HoraeMode::Smart;
        true
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub expected_revision: u64,
    pub config: Config,
}
pub fn decode_hex(s: &str) -> Result<Vec<u8>> {
    if s.is_empty()
        || s.len() > 8192
        || s.len() % 2 != 0
        || !s.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("invalid_hex_payload".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "invalid_hex".into()))
        .collect()
}
