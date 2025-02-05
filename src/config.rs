use std::{error::Error, fs};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub ethercat: EthercatConfig,
    pub slaves: Vec<SlaveConfig>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct EthercatConfig {
    pub master_id: u32,
    pub esi: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum SlaveConfig {
    Epos(EposKind),
    Unknown,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct EposKind {
    pub id: u16,
    pub encoder_resolution: u32,
    pub reduction: f32,
}

impl Config {
    pub fn from_yaml(path: &str) -> Result<Self, Box<dyn Error>> {
        let yaml = fs::read_to_string(path)?;
        Ok(serde_yaml::from_str(&yaml)?)
    }
}
