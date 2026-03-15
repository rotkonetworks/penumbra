//! Shared state between mmbot and arb-daemon
//!
//! Both bots write their state to JSON files that the other reads.
//! State directory: /tmp/penumbra-bots/ (same as arb-daemon)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_STATE_DIR: &str = "/tmp/penumbra-bots";

/// mmbot publishes this state
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MmBotState {
    pub updated_at: u64,
    pub open_positions: u32,
    pub base_balance: u128,
    pub quote_balance: u128,
    pub fair_price: f64,
    pub pair: String,
    pub inventory_skew: f64,
    pub alive: bool,
}

/// arb-daemon publishes this state
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArbState {
    pub updated_at: u64,
    pub reference_prices: ReferencePriceState,
    pub pending_ibc_transfers: u32,
    pub penumbra_usdc: u128,
    pub penumbra_um: u128,
    pub osmosis_usdc: u128,
    pub osmosis_um: u128,
    pub best_arb_spread_pct: f64,
    pub best_arb_pair: String,
    pub alive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReferencePriceState {
    pub btc_usdc_binance: f64,
    pub um_usdc_osmosis: f64,
    pub um_btc_osmosis: f64,
}

pub struct StateManager {
    state_dir: PathBuf,
}

impl StateManager {
    pub fn new(state_dir: &str) -> Result<Self> {
        let state_dir = PathBuf::from(state_dir);
        std::fs::create_dir_all(&state_dir)
            .context("Failed to create state directory")?;
        Ok(Self { state_dir })
    }

    pub fn default_dir() -> Result<Self> {
        Self::new(DEFAULT_STATE_DIR)
    }

    pub fn write_mmbot_state(&self, state: &MmBotState) -> Result<()> {
        let path = self.state_dir.join("mmbot.json");
        let tmp = self.state_dir.join("mmbot.json.tmp");
        let json = serde_json::to_string_pretty(state)?;
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn read_arb_state(&self) -> Option<ArbState> {
        let path = self.state_dir.join("arb.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

impl MmBotState {
    pub fn new(pair: &str) -> Self {
        Self {
            updated_at: now_secs(),
            pair: pair.to_string(),
            alive: true,
            ..Default::default()
        }
    }

    pub fn update(&mut self) {
        self.updated_at = now_secs();
    }
}
