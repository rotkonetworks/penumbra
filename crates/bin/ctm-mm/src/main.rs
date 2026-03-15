//! CTM Unified Market Maker — tight liquidity on Penumbra DEX.
//!
//! Architecture:
//!   1. Binance WebSocket → real-time fair price (BBO oracle)
//!   2. CTM model → optimal spread/skew/size (TorchScript inference)
//!   3. Penumbra SDK → native LP positions (Planner + ViewClient)
//!
//! The goal is to provide tight, accurate liquidity on Penumbra DEX
//! using minimal capital. Binance gives us the true price; the CTM model
//! decides how aggressive to be; Penumbra SDK places LP positions natively.
//!
//! Usage:
//!   ctm-mm --binance-symbol UMUSDT --asset penumbra --quote transfer/channel-2/uusdc
//!   ctm-mm --binance-symbol BTCUSDT --asset btc --quote transfer/channel-2/uusdc --levels 3

mod binance;
mod config;
mod ibc_flow;
mod model;
mod osmosis;
mod penumbra;
mod volume;

use anyhow::Result;
use clap::Parser;
use std::time::Duration;

#[derive(Debug, Parser)]
#[clap(name = "ctm-mm", about = "CTM-powered tight liquidity on Penumbra DEX")]
struct Args {
    /// pcli home directory (reuses wallet/keys)
    #[clap(long, env = "PENUMBRA_PCLI_HOME")]
    home: Option<String>,

    /// gRPC URL for pd node
    #[clap(long)]
    grpc_url: Option<url::Url>,

    /// Penumbra asset to market-make (e.g., "penumbra", "btc")
    #[clap(long, default_value = "penumbra")]
    asset: String,

    /// Quote asset on Penumbra (e.g., "transfer/channel-2/uusdc")
    #[clap(long, default_value = "transfer/channel-2/uusdc")]
    quote: String,

    /// Binance symbol for price oracle (e.g., "UMUSDT", "BTCUSDT")
    #[clap(long, default_value = "UMUSDT")]
    binance_symbol: String,

    /// Path to UnifiedCTM TorchScript model (optional)
    #[clap(long)]
    model: Option<String>,

    /// Number of LP levels per side (beyond the tight anchor)
    #[clap(long, default_value = "2")]
    levels: usize,

    /// Max spread in basis points for deepest level
    #[clap(long, default_value = "250")]
    max_spread_bps: u32,

    /// Base position fee in basis points (escalates per level)
    #[clap(long, default_value = "30")]
    fee_bps: u32,

    /// Fraction of capital to deploy (0.0-1.0)
    #[clap(long, default_value = "0.8")]
    risk_fraction: f64,

    /// Blocks between cycles (~6s per block). 3 = ~18s check cycle.
    #[clap(long, default_value = "3")]
    rebalance_blocks: u64,

    /// Min price move (%) to trigger rebalance. Saves proof cost when price is flat.
    /// 0 = always refresh. 0.1 = refresh on any meaningful move.
    #[clap(long, default_value = "0.1")]
    rebalance_threshold_pct: f64,

    /// Account index
    #[clap(long, default_value = "0")]
    source: u32,

    /// Dry run
    #[clap(long)]
    dry_run: bool,

    /// Max price deviation (%) between Binance and Penumbra DEX before warning
    #[clap(long, default_value = "5.0")]
    max_price_deviation: f64,

    /// Max per-position size in quote units (50 USDC = 50_000_000 with 6 decimals)
    #[clap(long, default_value = "50000000")]
    max_position_quote: u128,

    /// Cumulative loss threshold before circuit breaker trips (quote units, 20 USDC)
    #[clap(long, default_value = "20000000")]
    circuit_breaker_loss: u128,

    /// Warmup cycles (gradual capital scaling on startup)
    #[clap(long, default_value = "3")]
    warmup_cycles: u32,

    /// Max Binance oracle spread (bps) — skip cycle if oracle is unreliable
    #[clap(long, default_value = "50.0")]
    max_oracle_spread_bps: f64,

    /// Manual fair price (skip Binance oracle — for assets not on Binance)
    #[clap(long)]
    manual_price: Option<f64>,

    /// Stagger position opening: one position per block instead of batching all
    #[clap(long)]
    stagger: bool,

    /// Hermes telemetry URL for IBC flow monitoring (e.g., http://127.0.0.1:3001)
    #[clap(long)]
    hermes_url: Option<String>,

    /// Volume bot mode: do tiny swaps instead of LP. Use with separate --home wallet.
    #[clap(long)]
    volume: bool,

    /// Swap size for volume bot (raw units, e.g., 100000 = 0.1 USDC)
    #[clap(long, default_value = "100000")]
    swap_size: u128,

    /// Interval between volume swaps (seconds)
    #[clap(long, default_value = "30")]
    swap_interval: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // ─── Volume bot mode (separate wallet, tiny swaps) ─────────────
    if args.volume {
        tracing::info!("Volume bot mode: swapping {} every {}s", args.swap_size, args.swap_interval);
        let pcli_home = args.home.clone().unwrap_or_else(|| {
            let dirs = directories::ProjectDirs::from("zone", "penumbra", "pcli")
                .expect("platform data dir");
            dirs.data_dir().to_string_lossy().to_string()
        });
        let pcli_config = config::load_pcli_config(&pcli_home)?;
        let grpc_url = args.grpc_url.unwrap_or(pcli_config.grpc_url.clone());

        return volume::run(
            &pcli_config,
            grpc_url,
            &args.asset,
            &args.quote,
            args.swap_size,
            args.swap_interval,
            args.source,
            args.dry_run,
        ).await;
    }

    tracing::info!("CTM Market Maker starting");
    tracing::info!("  Penumbra: {} / {}", args.asset, args.quote);
    tracing::info!("  Binance oracle: {}", args.binance_symbol);
    tracing::info!("  levels={}, spread={}bps, fee={}bps", args.levels, args.max_spread_bps, args.fee_bps);
    tracing::info!("  risk_fraction={}, dry_run={}", args.risk_fraction, args.dry_run);

    // ─── 1. Load Penumbra wallet ─────────────────────────────────────
    let pcli_home = args.home.clone().unwrap_or_else(|| {
        let dirs = directories::ProjectDirs::from("zone", "penumbra", "pcli")
            .expect("platform data dir");
        dirs.data_dir().to_string_lossy().to_string()
    });
    let pcli_config = config::load_pcli_config(&pcli_home)?;
    tracing::info!("Loaded wallet from {}", pcli_home);

    let grpc_url = args.grpc_url.unwrap_or(pcli_config.grpc_url.clone());
    tracing::info!("Connecting to {}", grpc_url);

    // ─── 2. Start price oracle ──────────────────────────────────────
    // For assets not on Binance (like UM), use Osmosis SQS as the oracle.
    // Auto-detect: if asset is "penumbra" or manual_price is set, skip Binance WS.
    let use_osmosis = args.asset == "penumbra" && args.manual_price.is_none();
    let use_manual = args.manual_price.is_some();

    let (bbo, _ws_handle) = if use_manual || use_osmosis {
        (binance::create_empty_bbo(), None)
    } else {
        let (bbo, handle) = binance::spawn_bbo_feed(&args.binance_symbol);
        tracing::info!("Waiting for Binance BBO...");
        let first_bbo = binance::wait_for_bbo(&bbo).await;
        tracing::info!(
            "Binance connected: bid={:.4} ask={:.4} spread={:.1}bps",
            first_bbo.bid, first_bbo.ask, first_bbo.spread_bps()
        );
        (bbo, Some(handle))
    };

    // Osmosis oracle for UM
    let osmosis_oracle = if use_osmosis {
        let (price, _handle) = osmosis::spawn_osmosis_oracle(Duration::from_secs(5));
        tracing::info!("Waiting for Osmosis UM/USDC price...");
        let first = osmosis::wait_for_price(&price).await;
        tracing::info!("Osmosis connected: UM/USDC = {:.6}", first.price);
        Some(price)
    } else {
        if use_manual {
            tracing::info!("Using manual price: {:.6}", args.manual_price.unwrap());
        }
        None
    };

    // ─── 2b. Start IBC flow monitor (optional) ─────────────────────
    let ibc_flow = if let Some(hermes_url) = &args.hermes_url {
        let (flow, _handle) = ibc_flow::spawn_ibc_flow_monitor(
            hermes_url,
            Duration::from_secs(5),
        );
        tracing::info!("IBC flow monitor: {}", hermes_url);
        Some(flow)
    } else {
        None
    };

    // ─── 3. Initialize Penumbra venue ────────────────────────────────
    let mut pen = penumbra::PenumbraVenue::init(
        &pcli_config, grpc_url, &args.asset, &args.quote, args.source,
    ).await?;

    // Configure safety
    pen.safety.max_position_quote = args.max_position_quote;
    pen.safety.circuit_breaker_loss = args.circuit_breaker_loss;
    pen.safety.warmup_cycles = args.warmup_cycles;
    pen.safety.max_oracle_spread_bps = args.max_oracle_spread_bps;
    pen.safety.max_deviation_pct = args.max_price_deviation;
    tracing::info!(
        "Safety: max_pos={} circuit_breaker={} warmup={} max_oracle_spread={}bps max_dev={}%",
        args.max_position_quote, args.circuit_breaker_loss, args.warmup_cycles,
        args.max_oracle_spread_bps, args.max_price_deviation
    );

    let (base_bal, quote_bal) = pen.balances().await?;
    tracing::info!("Penumbra balances: {} base, {} quote", base_bal, quote_bal);

    // ─── 4. Load CTM model (optional) ────────────────────────────────
    let ctm_model: Option<model::CtmModel> = if let Some(model_path) = &args.model {
        match model::CtmModel::load(model_path) {
            Ok(m) => {
                tracing::info!("Loaded CTM model: {}", model_path);
                Some(m)
            }
            Err(e) => {
                tracing::warn!("CTM model load failed: {}, using rule-based", e);
                None
            }
        }
    } else {
        tracing::info!("No CTM model specified, using rule-based strategy");
        None
    };

    let coin_id = model::coin_id(&args.binance_symbol);
    tracing::info!("Coin ID: {} → {}", args.binance_symbol, coin_id);

    // ─── 5. Main loop ────────────────────────────────────────────────
    let sleep_secs = args.rebalance_blocks * 6; // ~6s per block
    let mut last_position_price: f64 = 0.0;
    let mut prev_fair_price: f64 = 0.0;

    loop {
        // Get fair price from oracle (Osmosis, Binance, or manual)
        let (fair_price, oracle_spread_bps) = if let Some(ref osmo) = osmosis_oracle {
            let p = osmo.read().await.clone();
            if !p.is_valid() || p.is_stale(Duration::from_secs(30)) {
                tracing::warn!("Osmosis price stale, waiting...");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            // Osmosis doesn't give us a bid/ask spread directly,
            // estimate from price impact (typically 0.1-1%)
            let spread_bps = (p.price_impact * 10000.0).max(5.0);
            (p.price, spread_bps)
        } else if use_manual {
            (args.manual_price.unwrap(), 0.0)
        } else {
            let bbo_state = bbo.read().await.clone();
            if !bbo_state.is_valid() || bbo_state.is_stale(Duration::from_secs(10)) {
                tracing::warn!("Binance data stale, waiting...");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            (bbo_state.mid(), bbo_state.spread_bps())
        };

        // ─── Safety: check circuit breaker ─────────────────────────────
        if pen.safety_state.circuit_breaker_tripped {
            tracing::error!("Circuit breaker tripped — bot halted. Restart to reset.");
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }

        // Check DEX price and validate oracle safety
        let dex_price = match pen.estimate_price().await {
            Ok(p) if p > 0.0 => {
                tracing::info!("DEX price: {:.4} (fair: {:.4})", p, fair_price);
                Some(p)
            }
            Ok(_) => {
                tracing::debug!("DEX price zero (no liquidity yet)");
                None
            }
            Err(e) => {
                tracing::debug!("DEX price unavailable: {}", e);
                None
            }
        };

        // Validate oracle quality + price deviation (skip in manual mode only)
        if !use_manual {
            if let Err(e) = pen.validate_oracle(oracle_spread_bps, dex_price, fair_price) {
                tracing::warn!("Safety: {} — skipping cycle", e);
                tokio::time::sleep(Duration::from_secs(6)).await;
                continue;
            }
        }

        // Query DEX order book (all public positions)
        match pen.order_book(20).await {
            Ok((bids, asks)) => {
                let bid_depth: f64 = bids.iter().map(|l| l.quote_reserves).sum();
                let ask_depth: f64 = asks.iter().map(|l| l.base_reserves).sum();
                let best_bid = bids.first().map(|l| l.price).unwrap_or(0.0);
                let best_ask = asks.first().map(|l| l.price).unwrap_or(0.0);
                let dex_spread = if best_bid > 0.0 && best_ask > 0.0 {
                    (best_ask - best_bid) / ((best_ask + best_bid) / 2.0) * 10000.0
                } else { 0.0 };
                tracing::info!(
                    "DEX book: bid={:.6} ask={:.6} spread={:.0}bps | depth: {:.2} quote bid, {:.2} base ask | levels: {} bid {} ask",
                    best_bid, best_ask, dex_spread, bid_depth, ask_depth, bids.len(), asks.len()
                );
            }
            Err(e) => tracing::debug!("Order book query failed: {}", e),
        }

        // Check IBC flow state
        if let Some(ref flow) = ibc_flow {
            let f = flow.read().await;
            let features = f.to_features();
            if f.inflow_rate > 0.0 || f.pending_inflow > 0 {
                tracing::info!(
                    "IBC flow: inflow={:.2}/s outflow={:.2}/s pending={} [features: {:.2},{:.2},{:.2},{:.2},{:.2},{:.2}]",
                    f.inflow_rate, f.outflow_rate, f.pending_inflow,
                    features[0], features[1], features[2], features[3], features[4], features[5]
                );
            }
        }

        // ─── Build market state from all data feeds ──────────────────
        let mut mkt = model::MarketState {
            fair_price,
            prev_fair_price,
            ..Default::default()
        };

        // DEX book data (already queried above, grab it again cheaply)
        if let Ok((bids, asks)) = pen.order_book(20).await {
            mkt.dex_best_bid = bids.first().map(|l| l.price).unwrap_or(0.0);
            mkt.dex_best_ask = asks.first().map(|l| l.price).unwrap_or(0.0);
            mkt.dex_bid_depth = bids.iter().map(|l| l.quote_reserves).sum();
            mkt.dex_ask_depth = asks.iter().map(|l| l.base_reserves).sum();
            mkt.dex_bid_levels = bids.len();
            mkt.dex_ask_levels = asks.len();
        }

        // IBC flow
        if let Some(ref flow) = ibc_flow {
            let f = flow.read().await;
            mkt.ibc_inflow_rate = f.inflow_rate;
            mkt.ibc_outflow_rate = f.outflow_rate;
            mkt.ibc_pending = f.pending_inflow;
        }

        // Osmosis spread
        if let Some(ref osmo) = osmosis_oracle {
            let p = osmo.read().await;
            mkt.osmosis_price_impact = p.price_impact;
        }

        // Cross-asset returns (BTC, ETH from Binance)
        let (btc_ret, eth_ret) = model::fetch_cross_asset_returns().await;
        mkt.btc_ret_1h = btc_ret;
        mkt.eth_ret_1h = eth_ret;

        // Get signal: CTM model for Binance assets, rule-based for UM
        let signal = if use_osmosis {
            // UM/Penumbra: use all market data, no CTM model needed
            let sig = model::compute_signal(&mkt);
            tracing::info!(
                "Signal [rule]: dir={:.2} spread={:.2} skew={:.2} size={:.2} | btc={:+.2}% eth={:+.2}% book_imbal={:.2}",
                sig.direction, sig.spread, sig.skew, sig.size,
                btc_ret * 100.0, eth_ret * 100.0, mkt.book_imbalance()
            );
            sig
        } else {
            match &ctm_model {
                Some(m) => {
                    match fetch_candles_for_model(&args.binance_symbol).await {
                        Ok((c1m, c1h, c1d)) => {
                            match m.infer(&c1m, &c1h, &c1d, coin_id) {
                                Ok(sig) => {
                                    tracing::info!(
                                        "Signal [CTM]: dir={:.2} spread={:.2} skew={:.2} size={:.2} cert={:.2}",
                                        sig.direction, sig.spread, sig.skew, sig.size, sig.certainty
                                    );
                                    sig
                                }
                                Err(e) => {
                                    tracing::warn!("CTM inference failed: {}", e);
                                    model::compute_signal(&mkt)
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Candle fetch failed: {}", e);
                            model::compute_signal(&mkt)
                        }
                    }
                }
                None => model::compute_signal(&mkt),
            }
        };

        prev_fair_price = fair_price;

        // ─── Always refresh: close old, open new every cycle ─────────
        // Penumbra fees are negligible — generate activity, be the market.
        // Optional threshold to skip if price barely moved (default 0 = always refresh).
        if args.rebalance_threshold_pct > 0.0 && last_position_price > 0.0 && !pen.positions.is_empty() {
            let price_move_pct = ((fair_price - last_position_price) / last_position_price * 100.0).abs();
            if price_move_pct < args.rebalance_threshold_pct {
                tracing::debug!("Price moved {:.3}% < {:.1}% threshold — holding", price_move_pct, args.rebalance_threshold_pct);
                tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
                continue;
            }
        }

        // Close old positions
        if let Err(e) = pen.close_positions().await {
            tracing::error!("Failed to close positions: {}", e);
            tokio::time::sleep(Duration::from_secs(6)).await;
            continue;
        }

        // Open new positions at fair price
        if let Err(e) = pen.open_positions(
            fair_price,
            signal.direction,
            signal.spread,
            signal.skew,
            signal.size,
            args.levels,
            args.max_spread_bps,
            args.fee_bps,
            args.risk_fraction,
            args.dry_run,
            args.stagger,
        ).await {
            tracing::error!("Failed to open positions: {}", e);
        } else {
            last_position_price = fair_price;
        }

        // Log status
        let (base_bal, quote_bal) = pen.balances().await.unwrap_or((0, 0));
        println!(
            "[{}] {} ${:.4} | dir={:.2} spread={:.2} oracle_bps={:.1} | positions={} | base={} quote={} | {}",
            chrono::Utc::now().format("%H:%M:%S"),
            if use_osmosis { "UM/USDC(osmo)" } else { &args.binance_symbol },
            fair_price,
            signal.direction,
            signal.spread,
            oracle_spread_bps,
            pen.positions.len(),
            base_bal,
            quote_bal,
            if args.dry_run { "DRY" } else { "LIVE" },
        );

        // Sleep until next rebalance
        tracing::info!("sleeping {}s", sleep_secs);
        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
    }
}

/// Default rule-based signal: neutral direction, moderate spread.
/// Fetch candles from Binance REST API and build feature vectors.
/// Returns (candles_1m, candles_1h, candles_1d) as flat f32 arrays.
async fn fetch_candles_for_model(symbol: &str) -> Result<(Vec<f32>, Vec<f32>, Vec<f32>)> {
    let client = reqwest::Client::new();
    let base = "https://api.binance.com/api/v3/klines";

    // Fetch 3 timeframes in parallel
    let (r1m, r1h, r1d) = tokio::join!(
        client.get(base)
            .query(&[("symbol", symbol), ("interval", "1m"), ("limit", "61")])
            .send(),
        client.get(base)
            .query(&[("symbol", symbol), ("interval", "1h"), ("limit", "49")])
            .send(),
        client.get(base)
            .query(&[("symbol", symbol), ("interval", "1d"), ("limit", "31")])
            .send(),
    );

    let c1m = parse_klines_to_features(r1m?.json::<serde_json::Value>().await?, 60)?;
    let c1h = parse_klines_to_features(r1h?.json::<serde_json::Value>().await?, 48)?;
    let c1d = parse_klines_to_features(r1d?.json::<serde_json::Value>().await?, 30)?;

    Ok((c1m, c1h, c1d))
}

/// Parse Binance klines JSON into flat feature vector.
/// Each candle → 21 features (matching Python build_features).
/// Returns flat [seq_len * 21] for model input.
fn parse_klines_to_features(data: serde_json::Value, seq_len: usize) -> Result<Vec<f32>> {
    let klines = data.as_array()
        .ok_or_else(|| anyhow::anyhow!("expected array of klines"))?;

    // Parse OHLCV from Binance format: [time, open, high, low, close, volume, ...]
    let mut candles: Vec<(f64, f64, f64, f64, f64, f64, f64)> = Vec::new();
    for k in klines {
        let arr = k.as_array().ok_or_else(|| anyhow::anyhow!("invalid kline"))?;
        let o: f64 = arr[1].as_str().unwrap_or("0").parse()?;
        let h: f64 = arr[2].as_str().unwrap_or("0").parse()?;
        let l: f64 = arr[3].as_str().unwrap_or("0").parse()?;
        let c: f64 = arr[4].as_str().unwrap_or("0").parse()?;
        let v: f64 = arr[5].as_str().unwrap_or("0").parse()?;
        let trades: f64 = arr[8].as_f64().unwrap_or(arr[8].as_str().unwrap_or("0").parse().unwrap_or(0.0));
        let taker_buy_vol: f64 = arr[9].as_str().unwrap_or("0").parse()?;
        candles.push((o, h, l, c, v, trades, taker_buy_vol));
    }

    // Take last seq_len+1 candles (need +1 for returns)
    let n = candles.len();
    let start = if n > seq_len + 1 { n - seq_len - 1 } else { 0 };
    let candles = &candles[start..];

    // Build features per bar (simplified — matches core features from Python pipeline)
    let n_features = 21;
    let mut features = Vec::with_capacity(seq_len * n_features);

    for i in 1..candles.len().min(seq_len + 1) {
        let (o, h, l, c, v, trades, tbv) = candles[i];
        let (po, _ph, _pl, pc, pv, pt, _ptbv) = candles[i - 1];

        let range = h - l;
        let body = (c - o).abs();
        let wick = if range > 0.0 { (range - body) / range } else { 0.0 };

        // Returns
        let ret_1 = if pc > 0.0 { (c - pc) / pc } else { 0.0 };

        // Simplified features (full Python pipeline has RSI, MACD, BB etc.
        // but for initial deployment these core features capture the signal)
        let log_vol = if v > 0.0 { v.ln() } else { 0.0 };
        let vol_ratio = if pv > 0.0 { v / pv } else { 1.0 };
        let log_trades = if trades > 0.0 { trades.ln() } else { 0.0 };
        let trades_ratio = if pt > 0.0 { trades / pt } else { 1.0 };
        let taker_buy_ratio = if v > 0.0 { tbv / v } else { 0.5 };
        let body_ratio = if range > 0.0 { body / range } else { 0.0 };
        let range_pct = if pc > 0.0 { range / pc } else { 0.0 };

        // 21 features (pad with zeros for RSI/MACD/BB which need lookback)
        let row = [
            ret_1 as f32,         // ret_1
            0.0f32,               // ret_5 (need lookback)
            0.0,                  // ret_15
            0.0,                  // ret_60
            body_ratio as f32,    // body_ratio
            wick as f32,          // wick_ratio
            range_pct as f32,     // range_pct
            0.5,                  // rsi_14 (need lookback, default neutral)
            0.5,                  // rsi_7
            0.0,                  // macd
            0.0,                  // macd_signal
            0.0,                  // macd_hist
            0.5,                  // bb_pos (default mid)
            0.02,                 // bb_width (default)
            range_pct as f32,     // atr_14 (approximate)
            log_vol as f32,       // log_vol
            vol_ratio as f32,     // vol_ratio
            log_trades as f32,    // log_trades
            trades_ratio as f32,  // trades_ratio
            taker_buy_ratio as f32, // taker_buy_ratio
            0.0,                  // funding_rate (not available from klines)
        ];
        features.extend_from_slice(&row);
    }

    // Pad if not enough candles
    while features.len() < seq_len * n_features {
        features.push(0.0);
    }

    // Truncate to exact size
    features.truncate(seq_len * n_features);

    Ok(features)
}
