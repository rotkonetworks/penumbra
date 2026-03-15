//! Config loading — reuses pcli wallet config.

use anyhow::{Context, Result};
use penumbra_sdk_custody::soft_kms::Config as SoftKmsConfig;
use penumbra_sdk_keys::FullViewingKey;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::path::Path;
use url::Url;

#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PcliConfig {
    pub grpc_url: Url,
    pub view_url: Option<Url>,
    #[serde_as(as = "DisplayFromStr")]
    pub full_viewing_key: FullViewingKey,
    pub custody: CustodyConfig,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "backend")]
pub enum CustodyConfig {
    SoftKms(SoftKmsConfig),
    #[serde(other)]
    Other,
}

pub fn load_pcli_config(home: &str) -> Result<PcliConfig> {
    let config_path = Path::new(home).join("config.toml");
    let contents = std::fs::read_to_string(&config_path)
        .context(format!("could not read pcli config at {}", config_path.display()))?;
    toml::from_str(&contents).context("failed to parse pcli config")
}
