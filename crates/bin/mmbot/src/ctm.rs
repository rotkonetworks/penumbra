use anyhow::Result;
use serde::Deserialize;
use tch::{CModule, Device, IValue, Tensor};

/// Market making decision (from CTM model or rule-based).
#[derive(Debug, Clone)]
pub struct MmDecision {
    /// Directional lean [-1, 1] (negative = want to sell, positive = want to buy)
    pub skew: f64,
    /// Overall aggressiveness [0, 1] (1 = tightest quotes)
    pub aggression: f64,
    /// Per-level bid offsets from fair value [0, 1] (fraction of max spread)
    pub bid_offsets: Vec<f64>,
    /// Per-level ask offsets from fair value [0, 1]
    pub ask_offsets: Vec<f64>,
    /// Per-level bid fees in bps
    pub bid_fees: Vec<u32>,
    /// Per-level ask fees in bps
    pub ask_fees: Vec<u32>,
    /// Per-level bid capital fractions (sums to 1)
    pub bid_sizes: Vec<f64>,
    /// Per-level ask capital fractions (sums to 1)
    pub ask_sizes: Vec<f64>,
}

/// CTM TorchScript model for market making inference.
///
/// Input: flat tensor [1, total_seq_len, max_features] (multi-timeframe, zero-padded)
///        + conviction scalar [1]
/// Output: [bias, spread, skew] in [-1,1], [0,1], [0,1]
///         + certainty [entropy, 1-entropy]
pub struct CtmModel {
    model: CModule,
    device: Device,
    total_seq_len: i64,
    max_features: i64,
}

impl CtmModel {
    pub fn load(path: &str) -> Result<Self> {
        let device = if tch::Cuda::is_available() {
            Device::Cuda(0)
        } else {
            Device::Cpu
        };
        let model = CModule::load_on_device(path, device)?;
        // Default for multi-tf: 1d(30) + 1h(48) + 1m(60) = 138, 30 features
        Ok(Self {
            model,
            device,
            total_seq_len: 138,
            max_features: 30,
        })
    }

    pub fn load_with_meta(model_path: &str, total_seq_len: i64, max_features: i64) -> Result<Self> {
        let device = if tch::Cuda::is_available() {
            Device::Cuda(0)
        } else {
            Device::Cpu
        };
        let model = CModule::load_on_device(model_path, device)?;
        Ok(Self {
            model,
            device,
            total_seq_len,
            max_features,
        })
    }

    /// Run inference. Returns (bias, spread, skew, certainty).
    /// `features` is a flat slice of [total_seq_len * max_features] f32 values,
    /// with each timeframe's data placed at the correct offset and zero-padded.
    /// Run inference. Returns (bias, spread, skew, certainty).
    /// `features` is a flat slice of [total_seq_len * max_features] f32 values,
    /// with each timeframe's data placed at the correct offset and zero-padded.
    pub fn infer(&self, features: &[f32], conviction: f32) -> Result<(f64, f64, f64, f64)> {
        let x = Tensor::from_slice(features)
            .reshape([1, self.total_seq_len, self.max_features])
            .to_device(self.device);
        let cv = Tensor::from_slice(&[conviction]).to_device(self.device);

        // Model returns (predictions [1,3], certainties [1,2]) as a tuple
        let output = self.model.forward_is(&[IValue::Tensor(x), IValue::Tensor(cv)])?;

        // Extract tuple elements
        let (pred_tensor, cert_tensor) = match output {
            IValue::Tuple(ref elems) if elems.len() == 2 => {
                let pred = match &elems[0] {
                    IValue::Tensor(t) => t.shallow_clone(),
                    _ => anyhow::bail!("Expected tensor for predictions"),
                };
                let cert = match &elems[1] {
                    IValue::Tensor(t) => t.shallow_clone(),
                    _ => anyhow::bail!("Expected tensor for certainties"),
                };
                (pred, cert)
            }
            IValue::Tensor(t) => {
                // Fallback: single tensor output (shouldn't happen with our model)
                (t.get(0), t.get(1))
            }
            _ => anyhow::bail!("Unexpected model output format"),
        };

        let pred = pred_tensor.squeeze_dim(0); // [3]
        let bias = f64::try_from(pred.get(0))?;
        let spread = f64::try_from(pred.get(1))?;
        let skew = f64::try_from(pred.get(2))?;

        // Certainty: [entropy, 1-entropy]. Use 1-entropy as confidence.
        let cert = cert_tensor.squeeze_dim(0); // [2]
        let certainty = f64::try_from(cert.get(1))?;

        Ok((bias, spread, skew, certainty))
    }
}

/// Convert CTM model output into a full MmDecision.
///
/// Maps the 3 model outputs (bias, spread_raw, skew_raw) to the
/// multi-level position structure needed by the Penumbra DEX planner.
pub fn ctm_to_decision(
    bias: f64,       // [-1, 1] directional lean
    spread: f64,     // [0, 1] spread tightness (0 = tight, 1 = wide)
    skew: f64,       // [0, 1] asymmetric sizing (0.5 = symmetric)
    inventory_skew: f64, // [-1, 1] current inventory imbalance
    n_levels: usize,
    base_fee_bps: u32,
) -> MmDecision {
    // Combine model bias with inventory management
    // Model says which direction, inventory forces rebalancing
    let effective_skew = (bias * 0.7 - inventory_skew * 0.3).clamp(-1.0, 1.0);

    // Spread: model controls tightness. Low spread_raw = aggressive (tight).
    let spread_factor = 0.2 + spread * 0.8; // [0.2, 1.0]

    // Graduated levels from tight to wide
    let bid_offsets: Vec<f64> = (0..n_levels)
        .map(|i| {
            let base = (i as f64 + 1.0) / (n_levels as f64 + 1.0);
            // Skew > 0.5 means model favors asks (bearish), tighten bids less
            let skew_adj = if effective_skew > 0.0 {
                base * (1.0 - effective_skew * 0.3) // bullish: tighter bids
            } else {
                base * (1.0 + effective_skew.abs() * 0.3) // bearish: wider bids
            };
            (skew_adj * spread_factor).clamp(0.01, 1.0)
        })
        .collect();

    let ask_offsets: Vec<f64> = (0..n_levels)
        .map(|i| {
            let base = (i as f64 + 1.0) / (n_levels as f64 + 1.0);
            let skew_adj = if effective_skew > 0.0 {
                base * (1.0 + effective_skew * 0.3) // bullish: wider asks
            } else {
                base * (1.0 - effective_skew.abs() * 0.3) // bearish: tighter asks
            };
            (skew_adj * spread_factor).clamp(0.01, 1.0)
        })
        .collect();

    // Fees: tighter levels get lower fees
    let bid_fees: Vec<u32> = (0..n_levels)
        .map(|i| {
            let level_fee = base_fee_bps / 2 + (i as u32) * (base_fee_bps / n_levels as u32);
            level_fee.max(1)
        })
        .collect();
    let ask_fees = bid_fees.clone();

    // Size allocation: skew biases capital toward the direction we want to accumulate
    // skew_raw > 0.5 means more capital on asks (selling), < 0.5 means more on bids (buying)
    let bid_weight = 1.0 - skew * 0.4; // [0.6, 1.0]
    let ask_weight = 0.6 + skew * 0.4; // [0.6, 1.0]
    let total_bid = bid_weight * n_levels as f64;
    let total_ask = ask_weight * n_levels as f64;

    let bid_sizes: Vec<f64> = (0..n_levels).map(|_| bid_weight / total_bid).collect();
    let ask_sizes: Vec<f64> = (0..n_levels).map(|_| ask_weight / total_ask).collect();

    MmDecision {
        skew: effective_skew,
        aggression: 1.0 - spread_factor,
        bid_offsets,
        ask_offsets,
        bid_fees,
        ask_fees,
        bid_sizes,
        ask_sizes,
    }
}

/// Rule-based market making strategy (fallback when no model loaded).
pub fn rule_based_decision(
    _fair_price: f64,
    inventory_skew: f64,
    conviction: f32,
    n_levels: usize,
    base_fee_bps: u32,
) -> MmDecision {
    let conviction_skew = (conviction as f64 / 50.0) - 1.0;
    ctm_to_decision(
        conviction_skew * 0.3,
        0.5, // moderate spread
        0.5, // symmetric
        inventory_skew,
        n_levels,
        base_fee_bps,
    )
}

/// CTM decision from the Python feature server (read from JSON file).
#[derive(Debug, Deserialize)]
pub struct CtmDecision {
    pub bias: f64,
    pub spread: f64,
    pub skew: f64,
    pub certainty: f64,
    pub fair_price: f64,
    pub timestamp: String,
    #[serde(default)]
    pub features_hash: String,
}

/// Read the latest CTM decision from the feature server's JSON output.
/// Returns error if file doesn't exist, is stale (>60s), or can't be parsed.
pub fn read_ctm_decision(path: &str) -> Result<CtmDecision> {
    let metadata = std::fs::metadata(path)?;
    let age = metadata
        .modified()?
        .elapsed()
        .unwrap_or(std::time::Duration::from_secs(999));

    if age.as_secs() > 60 {
        anyhow::bail!("CTM decision stale ({}s old)", age.as_secs());
    }

    let contents = std::fs::read_to_string(path)?;
    let decision: CtmDecision = serde_json::from_str(&contents)?;
    Ok(decision)
}
