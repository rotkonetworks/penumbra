//! Binance price oracle — WebSocket BBO feed for real-time fair price.
//!
//! We don't trade on Binance. It's purely the price reference for
//! placing tight LP positions on Penumbra.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::sync::RwLock;

const BINANCE_WS: &str = "wss://stream.binance.com:9443/ws";

#[derive(Debug, Clone)]
pub struct BboState {
    pub bid: f64,
    pub ask: f64,
    pub bid_qty: f64,
    pub ask_qty: f64,
    pub last_update: Instant,
}

impl Default for BboState {
    fn default() -> Self {
        Self { bid: 0.0, ask: 0.0, bid_qty: 0.0, ask_qty: 0.0, last_update: Instant::now() }
    }
}

impl BboState {
    pub fn mid(&self) -> f64 { (self.bid + self.ask) / 2.0 }

    pub fn spread_bps(&self) -> f64 {
        let m = self.mid();
        if m > 0.0 { (self.ask - self.bid) / m * 10000.0 } else { 0.0 }
    }

    pub fn is_valid(&self) -> bool {
        self.bid > 0.0 && self.ask > 0.0 && self.bid < self.ask
    }

    pub fn is_stale(&self, max_age: Duration) -> bool {
        self.last_update.elapsed() > max_age
    }
}

/// Shared Binance BBO state.
pub type SharedBbo = Arc<RwLock<BboState>>;

/// Create an empty shared BBO (for manual price mode).
pub fn create_empty_bbo() -> SharedBbo {
    Arc::new(RwLock::new(BboState::default()))
}

/// Spawn WebSocket task for real-time Binance BBO.
pub fn spawn_bbo_feed(symbol: &str) -> (SharedBbo, tokio::task::JoinHandle<()>) {
    let bbo = Arc::new(RwLock::new(BboState::default()));
    let bbo_clone = bbo.clone();
    let sym = symbol.to_lowercase();

    let handle = tokio::spawn(async move {
        loop {
            if let Err(e) = run_ws(&sym, &bbo_clone).await {
                tracing::warn!("Binance WS error: {}, reconnecting in 2s", e);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    (bbo, handle)
}

async fn run_ws(symbol: &str, bbo: &SharedBbo) -> Result<()> {
    use futures::StreamExt;
    use tokio_tungstenite::connect_async;

    let url = format!("{}/{}@bookTicker", BINANCE_WS, symbol);
    tracing::info!("Connecting to {}", url);

    let (ws, _) = connect_async(&url).await?;
    let (_, mut rx) = ws.split();

    while let Some(msg) = rx.next().await {
        let msg = msg?;
        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                let bid = data["b"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                let ask = data["a"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                let bid_qty = data["B"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                let ask_qty = data["A"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);

                if bid > 0.0 && ask > 0.0 {
                    let mut state = bbo.write().await;
                    state.bid = bid;
                    state.ask = ask;
                    state.bid_qty = bid_qty;
                    state.ask_qty = ask_qty;
                    state.last_update = Instant::now();
                }
            }
        }
    }

    Err(anyhow!("WebSocket ended"))
}

/// Wait for valid BBO data.
pub async fn wait_for_bbo(bbo: &SharedBbo) -> BboState {
    loop {
        let state = bbo.read().await.clone();
        if state.is_valid() {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
