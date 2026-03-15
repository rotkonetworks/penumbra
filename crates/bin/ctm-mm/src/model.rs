//! Trading signal computation for Penumbra LP decisions.
//!
//! Two modes:
//! 1. CTM TorchScript model — for assets with Binance candle data (BTC, ETH, etc.)
//! 2. Rule-based signal — for UM/Penumbra using DEX book + Osmosis + cross-asset data
//!
//! Both produce the same Signal struct that drives position placement.

use anyhow::{anyhow, Result};
use tch::{CModule, Device, IValue, Tensor};

/// Trading signal that drives LP position placement.
#[derive(Debug, Clone)]
pub struct Signal {
    /// [-1, 1]: negative = bearish, positive = bullish
    pub direction: f64,
    /// [0, 1]: how tight to quote (1 = tightest)
    pub spread: f64,
    /// [-1, 1]: order book skew (positive = lean bid)
    pub skew: f64,
    /// [0, 1]: position size confidence
    pub size: f64,
    /// [0, 1]: probability of fill
    pub fill_prob: f64,
    /// [0, 1]: adverse selection risk
    pub adverse_selection: f64,
    /// Model certainty
    pub certainty: f64,
}

impl Default for Signal {
    fn default() -> Self {
        Self {
            direction: 0.0,
            spread: 0.5,
            skew: 0.0,
            size: 0.5,
            fill_prob: 0.5,
            adverse_selection: 0.3,
            certainty: 0.0,
        }
    }
}

// ─── Market state snapshot (all available data feeds) ─────────────

/// Everything the bot knows this cycle — fed to signal computation.
#[derive(Debug, Clone, Default)]
pub struct MarketState {
    // Oracle price
    pub fair_price: f64,
    pub prev_fair_price: f64,

    // DEX book state
    pub dex_best_bid: f64,
    pub dex_best_ask: f64,
    pub dex_bid_depth: f64,  // total quote on bids
    pub dex_ask_depth: f64,  // total base on asks
    pub dex_bid_levels: usize,
    pub dex_ask_levels: usize,

    // Cross-asset (from Binance REST, not WS)
    pub btc_ret_1h: f64,
    pub eth_ret_1h: f64,

    // IBC flow
    pub ibc_inflow_rate: f64,
    pub ibc_outflow_rate: f64,
    pub ibc_pending: u64,

    // Osmosis-specific
    pub osmosis_price_impact: f64,
    pub osmosis_spread_pct: f64,
}

impl MarketState {
    /// Price return since last cycle.
    pub fn price_ret(&self) -> f64 {
        if self.prev_fair_price > 0.0 {
            (self.fair_price - self.prev_fair_price) / self.prev_fair_price
        } else {
            0.0
        }
    }

    /// DEX spread in bps.
    pub fn dex_spread_bps(&self) -> f64 {
        if self.dex_best_bid > 0.0 && self.dex_best_ask > 0.0 {
            let mid = (self.dex_best_bid + self.dex_best_ask) / 2.0;
            (self.dex_best_ask - self.dex_best_bid) / mid * 10000.0
        } else {
            0.0
        }
    }

    /// Book imbalance: positive = more bid depth (bullish), [-1, 1].
    pub fn book_imbalance(&self) -> f64 {
        let total = self.dex_bid_depth + self.dex_ask_depth * self.fair_price;
        if total > 0.0 {
            (self.dex_bid_depth - self.dex_ask_depth * self.fair_price) / total
        } else {
            0.0
        }
    }

    /// IBC net flow: positive = inflow to Penumbra (bullish for UM).
    pub fn ibc_net_flow(&self) -> f64 {
        self.ibc_inflow_rate - self.ibc_outflow_rate
    }
}

// ─── Rule-based signal (for UM and assets without CTM model) ──────

/// Compute trading signal from all available market data.
/// This replaces the CTM model when we don't have Binance candle history.
pub fn compute_signal(state: &MarketState) -> Signal {
    let mut sig = Signal::default();

    // ─── Direction: aggregate signals ───────────────────────────────
    let mut dir_score = 0.0;
    let mut dir_weight = 0.0;

    // 1. Price momentum (short-term return)
    let ret = state.price_ret();
    dir_score += ret.clamp(-0.05, 0.05) * 10.0; // normalize: 5% move → ±0.5
    dir_weight += 1.0;

    // 2. Cross-asset momentum (BTC leads altcoins)
    if state.btc_ret_1h != 0.0 {
        dir_score += state.btc_ret_1h.clamp(-0.05, 0.05) * 6.0;
        dir_weight += 0.8;
    }
    if state.eth_ret_1h != 0.0 {
        dir_score += state.eth_ret_1h.clamp(-0.05, 0.05) * 4.0;
        dir_weight += 0.5;
    }

    // 3. DEX book imbalance
    let imbalance = state.book_imbalance();
    dir_score += imbalance * 0.3;
    dir_weight += 0.5;

    // 4. IBC flow (inflow = people bridging TO Penumbra = bullish demand)
    let net_flow = state.ibc_net_flow();
    if net_flow.abs() > 0.1 {
        dir_score += net_flow.clamp(-2.0, 2.0) * 0.15;
        dir_weight += 0.3;
    }

    sig.direction = if dir_weight > 0.0 {
        (dir_score / dir_weight).clamp(-1.0, 1.0)
    } else {
        0.0
    };

    // ─── Spread: tighter when conditions are calm ───────────────────
    // High volatility / wide DEX spread → widen our quotes
    let vol_factor = ret.abs() * 20.0; // 1% move → 0.2 spread widening
    let dex_spread = state.dex_spread_bps();
    let dex_factor = if dex_spread > 200.0 { 0.3 } else { 0.0 }; // DEX already wide → we stay wide too

    // IBC pending = incoming swap pressure → tighten to capture it
    let flow_factor = if state.ibc_pending > 3 { -0.2 } else { 0.0 };

    sig.spread = (0.6 + vol_factor + dex_factor + flow_factor).clamp(0.1, 1.0);

    // ─── Skew: lean towards the direction ───────────────────────────
    sig.skew = sig.direction * 0.5 + imbalance * 0.3;
    sig.skew = sig.skew.clamp(-1.0, 1.0);

    // ─── Size: more capital when we're confident ────────────────────
    // High cross-asset alignment → more confidence → deploy more
    let alignment = if state.btc_ret_1h != 0.0 && ret != 0.0 {
        (state.btc_ret_1h.signum() == ret.signum()) as u8 as f64
    } else {
        0.5
    };
    sig.size = (0.4 + alignment * 0.3).clamp(0.2, 1.0);

    // ─── Fill prob: higher when DEX has activity ────────────────────
    sig.fill_prob = if state.dex_bid_levels + state.dex_ask_levels > 4 {
        0.7
    } else {
        0.3
    };

    // ─── Adverse selection: higher when volatile ────────────────────
    sig.adverse_selection = (vol_factor + 0.2).clamp(0.1, 0.9);

    // ─── Certainty: based on data availability ──────────────────────
    let mut data_score = 0.0;
    if state.fair_price > 0.0 { data_score += 0.3; }
    if state.dex_best_bid > 0.0 { data_score += 0.2; }
    if state.btc_ret_1h != 0.0 { data_score += 0.2; }
    if state.ibc_inflow_rate > 0.0 || state.ibc_outflow_rate > 0.0 { data_score += 0.1; }
    sig.certainty = data_score;

    sig
}

// ─── CTM TorchScript model (for Binance-listed assets) ────────────

pub struct CtmModel {
    model: CModule,
    device: Device,
    book_features: i64,
    candle_features: i64,
}

impl CtmModel {
    pub fn load(path: &str) -> Result<Self> {
        let device = if tch::Cuda::is_available() {
            tracing::info!("CTM model: using CUDA");
            Device::Cuda(0)
        } else {
            tracing::info!("CTM model: using CPU");
            Device::Cpu
        };
        let model = CModule::load_on_device(path, device)?;

        let meta_path = path.replace(".pt", "_meta.pt");
        let (book_features, candle_features) = if let Ok(_meta) = CModule::load(&meta_path) {
            tracing::info!("Loaded model metadata from {}", meta_path);
            (30i64, 21i64)
        } else {
            tracing::info!("No metadata found, using defaults (book=30, candle=21)");
            (30, 21)
        };

        Ok(Self { model, device, book_features, candle_features })
    }

    /// Run inference with candle data (for Binance-listed assets).
    pub fn infer(
        &self,
        candles_1m: &[f32],
        candles_1h: &[f32],
        candles_1d: &[f32],
        coin_id: i64,
    ) -> Result<Signal> {
        let book_t = Tensor::zeros(
            [1, 60, self.book_features],
            (tch::Kind::Float, self.device),
        );

        let c1m_t = Tensor::from_slice(candles_1m)
            .reshape([1, 60, self.candle_features])
            .to_device(self.device);
        let c1h_t = Tensor::from_slice(candles_1h)
            .reshape([1, 48, self.candle_features])
            .to_device(self.device);
        let c1d_t = Tensor::from_slice(candles_1d)
            .reshape([1, 30, self.candle_features])
            .to_device(self.device);
        let coin_t = Tensor::from_slice(&[coin_id])
            .to_device(self.device);

        let output = self.model.forward_is(&[
            IValue::Tensor(book_t),
            IValue::Tensor(c1m_t),
            IValue::Tensor(c1h_t),
            IValue::Tensor(c1d_t),
            IValue::Tensor(coin_t),
        ])?;

        let (pred_t, cert_t) = match output {
            IValue::Tuple(ref elems) if elems.len() >= 2 => {
                let pred = match &elems[0] {
                    IValue::Tensor(t) => t.shallow_clone(),
                    _ => return Err(anyhow!("expected tensor for predictions")),
                };
                let cert = match &elems[1] {
                    IValue::Tensor(t) => t.shallow_clone(),
                    _ => return Err(anyhow!("expected tensor for certainties")),
                };
                (pred, cert)
            }
            _ => return Err(anyhow!("unexpected model output format")),
        };

        let pred = pred_t.squeeze_dim(0).to_device(Device::Cpu);
        let direction = f64::try_from(pred.get(0))?.tanh();
        let spread = sigmoid(f64::try_from(pred.get(1))?);
        let skew = f64::try_from(pred.get(2))?.tanh();
        let size = sigmoid(f64::try_from(pred.get(3))?);
        let fill_prob = sigmoid(f64::try_from(pred.get(4))?);
        let adverse_selection = sigmoid(f64::try_from(pred.get(5))?);

        let cert = cert_t.squeeze_dim(0).to_device(Device::Cpu);
        let certainty = sigmoid(f64::try_from(cert.get(1))?);

        Ok(Signal { direction, spread, skew, size, fill_prob, adverse_selection, certainty })
    }
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// Coin ID mapping — must match training COIN_IDS.
pub fn coin_id(symbol: &str) -> i64 {
    match symbol {
        "BTC" | "BTCUSDT" => 0,
        "ETH" | "ETHUSDT" => 1,
        "SOL" | "SOLUSDT" => 2,
        "ZEC" | "ZECUSDT" => 3,
        "DOGE" | "DOGEUSDT" => 4,
        "AVAX" | "AVAXUSDT" => 5,
        "LINK" | "LINKUSDT" => 6,
        "ARB" | "ARBUSDT" => 7,
        _ => 0,
    }
}

/// Fetch BTC and ETH 1h returns from Binance (for cross-asset context).
pub async fn fetch_cross_asset_returns() -> (f64, f64) {
    let client = reqwest::Client::new();
    let base = "https://api.binance.com/api/v3/klines";

    let fetch_ret = |symbol: &'static str| {
        let c = client.clone();
        async move {
            let resp = c.get(base)
                .query(&[("symbol", symbol), ("interval", "1h"), ("limit", "2")])
                .send().await.ok()?;
            let data: serde_json::Value = resp.json().await.ok()?;
            let klines = data.as_array()?;
            if klines.len() >= 2 {
                let prev_close: f64 = klines[0][4].as_str()?.parse().ok()?;
                let curr_close: f64 = klines[1][4].as_str()?.parse().ok()?;
                if prev_close > 0.0 {
                    return Some((curr_close - prev_close) / prev_close);
                }
            }
            None
        }
    };

    let (btc, eth) = tokio::join!(
        fetch_ret("BTCUSDT"),
        fetch_ret("ETHUSDT"),
    );

    (btc.unwrap_or(0.0), eth.unwrap_or(0.0))
}
