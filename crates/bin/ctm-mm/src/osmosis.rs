//! Osmosis SQS price oracle — for assets not on Binance (like UM).
//!
//! Polls the Osmosis SQS router for UM/USDC quotes every few seconds.
//! This gives us a real price feed for Penumbra's native token.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::RwLock;

const SQS_BASE: &str = "https://sqs.osmosis.zone";

// IBC denoms on Osmosis
const UM_DENOM: &str = "ibc/0FA9232B262B89E77D1335D54FB1E1F506A92A7E4B51524B400DC69C68D28372";
const USDC_DENOM: &str = "ibc/498A0751C798A0D9A389AA3691123DADA57DAA4FE165D5C75894505B876BA6E4";

/// Amount of UM to quote (1 UM = 1_000_000 upenumbra)
const QUOTE_AMOUNT: u64 = 1_000_000;

#[derive(Debug, Clone)]
pub struct OsmosisPrice {
    /// UM price in USDC
    pub price: f64,
    /// Amount out for 1 UM
    pub amount_out: f64,
    /// Price impact for the quote
    pub price_impact: f64,
    /// Effective fee
    pub effective_fee: f64,
    /// Last update time
    pub last_update: Instant,
}

impl Default for OsmosisPrice {
    fn default() -> Self {
        Self {
            price: 0.0,
            amount_out: 0.0,
            price_impact: 0.0,
            effective_fee: 0.0,
            last_update: Instant::now(),
        }
    }
}

impl OsmosisPrice {
    pub fn is_valid(&self) -> bool {
        self.price > 0.0
    }

    pub fn is_stale(&self, max_age: Duration) -> bool {
        self.last_update.elapsed() > max_age
    }
}

pub type SharedOsmosisPrice = Arc<RwLock<OsmosisPrice>>;

/// Spawn background task polling Osmosis SQS for UM/USDC price.
pub fn spawn_osmosis_oracle(
    interval: Duration,
) -> (SharedOsmosisPrice, tokio::task::JoinHandle<()>) {
    let state = Arc::new(RwLock::new(OsmosisPrice::default()));
    let state_clone = state.clone();

    let handle = tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");

        loop {
            match fetch_um_price(&client).await {
                Ok((price, amount_out, impact, fee)) => {
                    let mut s = state_clone.write().await;
                    s.price = price;
                    s.amount_out = amount_out;
                    s.price_impact = impact;
                    s.effective_fee = fee;
                    s.last_update = Instant::now();
                    tracing::debug!("Osmosis UM/USDC: {:.6} (impact={:.4}%)", price, impact * 100.0);
                }
                Err(e) => {
                    tracing::debug!("Osmosis SQS error: {}", e);
                }
            }
            tokio::time::sleep(interval).await;
        }
    });

    (state, handle)
}

/// Fetch UM→USDC price from SQS router.
/// Returns (price, amount_out, price_impact, effective_fee).
async fn fetch_um_price(client: &reqwest::Client) -> Result<(f64, f64, f64, f64)> {
    let token_in = format!("{}{}", QUOTE_AMOUNT, UM_DENOM);
    let url = format!(
        "{}/router/quote?tokenIn={}&tokenOutDenom={}",
        SQS_BASE, token_in, USDC_DENOM,
    );

    let resp: serde_json::Value = client.get(&url).send().await?.json().await?;

    // SQS returns: { "amount_out": "...", "route": [...], "effective_fee": "...",
    //   "price_impact": "...", "in_base_out_quote_spot_price": "..." }
    let amount_out_raw: f64 = resp["amount_out"]
        .as_str()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);

    // USDC has 6 decimals on Noble/Osmosis
    let amount_out = amount_out_raw / 1_000_000.0;

    // Prefer spot price (more accurate than amount_out which includes slippage)
    let spot_price: f64 = resp["in_base_out_quote_spot_price"]
        .as_str()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);

    let price = if spot_price > 0.0 { spot_price } else { amount_out };

    let price_impact: f64 = resp["price_impact"]
        .as_str()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);

    let effective_fee: f64 = resp["effective_fee"]
        .as_str()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);

    if price <= 0.0 {
        anyhow::bail!("got zero price from SQS");
    }

    Ok((price, amount_out, price_impact, effective_fee))
}

/// Also fetch USDC→UM (reverse direction) for spread estimation.
pub async fn fetch_reverse_price(client: &reqwest::Client) -> Result<f64> {
    // Quote 1 USDC worth of UM
    let usdc_amount: u64 = 1_000_000; // 1 USDC
    let token_in = format!("{}{}", usdc_amount, USDC_DENOM);
    let url = format!(
        "{}/router/quote?tokenIn={}&tokenOutDenom={}",
        SQS_BASE, token_in, UM_DENOM,
    );

    let resp: serde_json::Value = client.get(&url).send().await?.json().await?;

    let amount_out_raw: f64 = resp["amount_out"]
        .as_str()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);

    // UM has 6 decimals
    let um_out = amount_out_raw / 1_000_000.0;
    if um_out <= 0.0 {
        anyhow::bail!("got zero reverse price from SQS");
    }

    // Price = USDC per UM = 1 / um_out
    Ok(1.0 / um_out)
}

/// Wait for valid Osmosis price data.
pub async fn wait_for_price(state: &SharedOsmosisPrice) -> OsmosisPrice {
    loop {
        let s = state.read().await.clone();
        if s.is_valid() {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
