//! Native Penumbra DEX venue — LP positions via SDK (Planner + ViewClient).
//!
//! This is the core of the bot: provide tight liquidity on Penumbra DEX
//! using Binance as the price oracle. Uses the Penumbra SDK directly
//! (same as pcli) for transaction building and signing.
//!
//! Safety: Penumbra uses constant-sum AMM positions with batch auctions.
//! On illiquid pairs, YOUR positions define the clearing price. If mispriced,
//! arbitrageurs drain your reserves instantly. We protect against this with:
//!   - Mandatory Binance oracle (never place LP without reference price)
//!   - Max position size cap (don't deploy more than can be safely filled)
//!   - Post-fill P&L tracking with circuit breaker
//!   - Price sanity checks (Binance vs DEX deviation guard)
//!   - Gradual capital deployment (start tiny, scale up)

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use ibc_types::core::client::Height as IbcHeight;
use ibc_types::core::channel::ChannelId;
use penumbra_sdk_asset::{asset, Value};
use penumbra_sdk_custody::soft_kms::SoftKms;
use penumbra_sdk_dex::{
    DirectedTradingPair,
    lp::{
        position::{self, Position},
        SellOrder,
    },
};
use penumbra_sdk_fee::FeeTier;
use penumbra_sdk_keys::keys::AddressIndex;
use penumbra_sdk_num::Amount;
use penumbra_sdk_proto::{
    box_grpc_svc,
    core::component::dex::v1::{
        query_service_client::QueryServiceClient as DexQueryClient,
        simulation_service_client::SimulationServiceClient,
        LiquidityPositionsByPriceRequest,
        SimulateTradeRequest,
        simulate_trade_request::Routing,
        simulate_trade_request::routing::Setting,
    },
    custody::v1::{
        custody_service_client::CustodyServiceClient,
        custody_service_server::CustodyServiceServer,
    },
    view::v1::{
        view_service_client::ViewServiceClient,
        view_service_server::ViewServiceServer,
    },
};
use penumbra_sdk_shielded_pool::Ics20Withdrawal;
use penumbra_sdk_view::{Planner, ViewClient, ViewServer};
use rand_core::OsRng;
use tonic::transport::Channel;

use crate::config::{CustodyConfig, PcliConfig};

/// A single level in the DEX order book.
#[derive(Debug, Clone)]
pub struct OrderBookLevel {
    /// Effective price in quote per base unit.
    pub price: f64,
    /// Available base reserves (human units).
    pub base_reserves: f64,
    /// Available quote reserves (human units).
    pub quote_reserves: f64,
    /// Fee tier in basis points.
    pub fee_bps: u32,
}

/// Safety configuration for LP placement on illiquid Penumbra pairs.
#[derive(Debug, Clone)]
pub struct SafetyConfig {
    /// Max % of total capital to deploy per cycle (prevents full drain).
    /// Start small on illiquid pairs. Default: 0.10 (10%)
    pub max_deploy_fraction: f64,
    /// Max position size in quote units per single position.
    /// Prevents one large position from being entirely drained.
    pub max_position_quote: u128,
    /// Max cumulative loss (in quote units) before circuit breaker trips.
    pub circuit_breaker_loss: u128,
    /// Max allowed deviation (%) between Binance and DEX price.
    /// If exceeded, skip this cycle entirely.
    pub max_deviation_pct: f64,
    /// Minimum Binance spread (bps) — if Binance spread is too wide,
    /// the oracle is unreliable. Skip cycle.
    pub max_oracle_spread_bps: f64,
    /// Number of successful cycles before scaling up capital.
    pub warmup_cycles: u32,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            max_deploy_fraction: 0.10,
            max_position_quote: 1_000_000_000, // 1000 USDC (6 decimals)
            circuit_breaker_loss: 5_000_000_000, // 5000 USDC
            max_deviation_pct: 3.0,
            max_oracle_spread_bps: 50.0,
            warmup_cycles: 5,
        }
    }
}

/// Tracks P&L and safety state across cycles.
#[derive(Debug, Default)]
pub struct SafetyState {
    pub total_base_deployed: u128,
    pub total_quote_deployed: u128,
    pub initial_base: u128,
    pub initial_quote: u128,
    pub cycle_count: u32,
    pub consecutive_errors: u32,
    pub circuit_breaker_tripped: bool,
}

impl SafetyState {
    /// Check if we should halt due to excessive losses.
    pub fn check_pnl(
        &mut self,
        current_base: u128,
        current_quote: u128,
        fair_price: f64,
        config: &SafetyConfig,
    ) -> bool {
        if self.initial_base == 0 && self.initial_quote == 0 {
            // First cycle — record starting balances
            self.initial_base = current_base;
            self.initial_quote = current_quote;
            return true;
        }

        // Compute total value in quote terms
        let initial_value = self.initial_quote as f64
            + self.initial_base as f64 * fair_price;
        let current_value = current_quote as f64
            + current_base as f64 * fair_price;
        let loss = initial_value - current_value;

        if loss > 0.0 && loss as u128 > config.circuit_breaker_loss {
            tracing::error!(
                "CIRCUIT BREAKER: loss {:.0} exceeds limit {} — halting LP placement",
                loss, config.circuit_breaker_loss
            );
            self.circuit_breaker_tripped = true;
            return false;
        }

        if loss > 0.0 {
            tracing::warn!(
                "P&L: loss={:.0} / limit={} ({:.1}%)",
                loss, config.circuit_breaker_loss,
                loss / config.circuit_breaker_loss as f64 * 100.0
            );
        } else {
            tracing::info!("P&L: profit={:.0}", -loss);
        }

        true
    }

    /// Compute effective risk fraction based on warmup.
    pub fn effective_risk_fraction(&self, base_fraction: f64, config: &SafetyConfig) -> f64 {
        if self.cycle_count < config.warmup_cycles {
            // Gradually scale up: 20% → 40% → 60% → 80% → 100% of base fraction
            let warmup_scale = (self.cycle_count as f64 + 1.0) / config.warmup_cycles as f64;
            let fraction = base_fraction * warmup_scale * config.max_deploy_fraction;
            tracing::info!(
                "Warmup cycle {}/{}: deploying {:.1}% of capital",
                self.cycle_count + 1, config.warmup_cycles,
                fraction * 100.0
            );
            fraction
        } else {
            base_fraction * config.max_deploy_fraction
        }
    }
}

/// Penumbra DEX venue — native SDK integration.
pub struct PenumbraVenue {
    pub view: ViewServiceClient<box_grpc_svc::BoxGrpcService>,
    pub custody: CustodyServiceClient<box_grpc_svc::BoxGrpcService>,
    pub dex_client: DexQueryClient<Channel>,
    pub sim_client: SimulationServiceClient<Channel>,
    pub fvk: penumbra_sdk_keys::FullViewingKey,
    pub base_id: asset::Id,
    pub quote_id: asset::Id,
    pub base_denom: asset::Unit,
    pub quote_denom: asset::Unit,
    pub source: AddressIndex,
    /// Currently open position IDs.
    pub positions: Vec<position::Id>,
    /// Safety configuration.
    pub safety: SafetyConfig,
    /// Safety state (P&L tracking, warmup).
    pub safety_state: SafetyState,
}

impl PenumbraVenue {
    /// Initialize from pcli config (same wallet/keys as pcli).
    pub async fn init(
        config: &PcliConfig,
        grpc_url: url::Url,
        asset_name: &str,
        quote_name: &str,
        account: u32,
    ) -> Result<Self> {
        // View service (in-memory)
        let view_svc = ViewServer::load_or_initialize(
            None::<&camino::Utf8Path>,
            None::<&camino::Utf8Path>,
            &config.full_viewing_key,
            grpc_url.clone(),
        )
        .await
        .context("failed to initialize view service")?;

        let view_server = ViewServiceServer::new(view_svc);
        let mut view = ViewServiceClient::new(box_grpc_svc::local(view_server));

        // Custody (signing)
        let custody_svc = match &config.custody {
            CustodyConfig::SoftKms(soft_kms_config) => SoftKms::new(soft_kms_config.clone()),
            CustodyConfig::Other => anyhow::bail!("requires SoftKms custody"),
        };
        let custody_server = CustodyServiceServer::new(custody_svc);
        let custody = CustodyServiceClient::new(box_grpc_svc::local(custody_server));

        // DEX clients
        let channel = ViewServer::get_pd_channel(grpc_url).await?;
        let dex_client = DexQueryClient::new(channel.clone());
        let sim_client = SimulationServiceClient::new(channel);

        // Wait for view service to sync (must reach current chain tip)
        tracing::info!("Waiting for view sync (this may take a minute on first run)...");
        loop {
            match ViewClient::status(&mut view).await {
                Ok(status) => {
                    if status.full_sync_height > 0 && !status.catching_up {
                        tracing::info!("View synced at height {}", status.full_sync_height);
                        break;
                    }
                    tracing::info!(
                        "View syncing: height={} catching_up={}",
                        status.full_sync_height, status.catching_up
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                Err(e) => {
                    tracing::debug!("View sync in progress: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }

        // Resolve assets
        let asset_cache = ViewClient::assets(&mut view).await?;
        let base_denom = asset_cache
            .get_unit(asset_name)
            .ok_or_else(|| anyhow::anyhow!("unknown asset: {}", asset_name))?;
        let quote_denom = asset_cache
            .get_unit(quote_name)
            .ok_or_else(|| anyhow::anyhow!("unknown quote: {}", quote_name))?;

        let base_id = base_denom.id();
        let quote_id = quote_denom.id();

        tracing::info!(
            "Penumbra venue: {} / {} (account {})",
            asset_name, quote_name, account
        );

        Ok(Self {
            view,
            custody,
            dex_client,
            sim_client,
            fvk: config.full_viewing_key.clone(),
            base_id,
            quote_id,
            base_denom,
            quote_denom,
            source: AddressIndex::new(account),
            positions: Vec::new(),
            safety: SafetyConfig::default(),
            safety_state: SafetyState::default(),
        })
    }

    /// Get balances (base, quote) in raw units.
    pub async fn balances(&mut self) -> Result<(u128, u128)> {
        let notes = ViewClient::unspent_notes_by_address_and_asset(&mut self.view).await?;
        let account_notes = notes.get(&self.source).cloned().unwrap_or_default();

        let base: u128 = account_notes
            .get(&self.base_id)
            .map(|n| n.iter().map(|n| u128::from(n.note.amount())).sum())
            .unwrap_or(0);
        let quote: u128 = account_notes
            .get(&self.quote_id)
            .map(|n| n.iter().map(|n| u128::from(n.note.amount())).sum())
            .unwrap_or(0);

        Ok((base, quote))
    }

    /// Estimate fair price via DEX swap simulation.
    pub async fn estimate_price(&mut self) -> Result<f64> {
        let base_unit = u128::from(self.base_denom.unit_amount());
        let quote_unit = u128::from(self.quote_denom.unit_amount());

        // Sell 1 base → quote
        let sell_input = Value {
            amount: Amount::from(base_unit),
            asset_id: self.base_id,
        };
        let sell_out = simulate_swap(&mut self.sim_client, sell_input, self.quote_id).await?;

        // Buy base with 1 quote
        let buy_input = Value {
            amount: Amount::from(quote_unit),
            asset_id: self.quote_id,
        };
        let buy_out = simulate_swap(&mut self.sim_client, buy_input, self.base_id).await?;

        match (sell_out, buy_out) {
            (Some(s), Some(b)) => {
                let sell_price = u128::from(s.amount) as f64 / quote_unit as f64;
                let buy_base = u128::from(b.amount);
                let buy_price = if buy_base > 0 {
                    quote_unit as f64 / (buy_base as f64 / base_unit as f64)
                } else {
                    sell_price
                };
                Ok((sell_price + buy_price) / 2.0)
            }
            (Some(s), None) => Ok(u128::from(s.amount) as f64 / quote_unit as f64),
            _ => anyhow::bail!("no liquidity"),
        }
    }

    /// Query the full DEX order book for our trading pair.
    /// Returns (bids, asks) where each is a vec of (effective_price, reserves_base, reserves_quote, fee_bps).
    /// Bids = positions selling quote for base (someone wants to buy base).
    /// Asks = positions selling base for quote (someone wants to sell base).
    pub async fn order_book(&mut self, limit: u64) -> Result<(Vec<OrderBookLevel>, Vec<OrderBookLevel>)> {
        use futures::TryStreamExt;

        let base_unit = u128::from(self.base_denom.unit_amount()) as f64;
        let quote_unit = u128::from(self.quote_denom.unit_amount()) as f64;

        // Ask side: base → quote (positions offering to sell base for quote)
        let ask_pair = DirectedTradingPair::new(self.base_id, self.quote_id);
        let ask_stream = self.dex_client
            .liquidity_positions_by_price(LiquidityPositionsByPriceRequest {
                trading_pair: Some(ask_pair.into()),
                limit,
                ..Default::default()
            })
            .await?
            .into_inner();

        let ask_positions: Vec<Position> = ask_stream
            .map_err(|e| anyhow::anyhow!("ask stream: {}", e))
            .and_then(|msg| async move {
                msg.data
                    .ok_or_else(|| anyhow::anyhow!("missing position"))
                    .map(Position::try_from)?
            })
            .try_collect()
            .await?;

        // Bid side: quote → base (positions offering to sell quote for base)
        let bid_pair = DirectedTradingPair::new(self.quote_id, self.base_id);
        let bid_stream = self.dex_client
            .liquidity_positions_by_price(LiquidityPositionsByPriceRequest {
                trading_pair: Some(bid_pair.into()),
                limit,
                ..Default::default()
            })
            .await?
            .into_inner();

        let bid_positions: Vec<Position> = bid_stream
            .map_err(|e| anyhow::anyhow!("bid stream: {}", e))
            .and_then(|msg| async move {
                msg.data
                    .ok_or_else(|| anyhow::anyhow!("missing position"))
                    .map(Position::try_from)?
            })
            .try_collect()
            .await?;

        // Convert positions to price levels
        let mut asks = Vec::new();
        for pos in &ask_positions {
            let p = pos.phi.component.p.value() as f64;
            let q = pos.phi.component.q.value() as f64;
            if q > 0.0 {
                // Effective price: how much quote you get per base unit
                let eff_price = (p / q) * (base_unit / quote_unit);
                let r1 = u128::from(pos.reserves.r1) as f64 / base_unit;
                let r2 = u128::from(pos.reserves.r2) as f64 / quote_unit;
                asks.push(OrderBookLevel {
                    price: eff_price,
                    base_reserves: r1,
                    quote_reserves: r2,
                    fee_bps: pos.phi.component.fee,
                });
            }
        }

        let mut bids = Vec::new();
        for pos in &bid_positions {
            let p = pos.phi.component.p.value() as f64;
            let q = pos.phi.component.q.value() as f64;
            if p > 0.0 {
                // For bid direction (quote→base), effective price in quote per base
                let eff_price = (q / p) * (quote_unit / base_unit);
                let r1 = u128::from(pos.reserves.r1) as f64 / quote_unit;
                let r2 = u128::from(pos.reserves.r2) as f64 / base_unit;
                bids.push(OrderBookLevel {
                    price: eff_price,
                    base_reserves: r2,
                    quote_reserves: r1,
                    fee_bps: pos.phi.component.fee,
                });
            }
        }

        // Sort: bids descending, asks ascending
        bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
        asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

        Ok((bids, asks))
    }

    /// Close all our open positions.
    pub async fn close_positions(&mut self) -> Result<()> {
        if self.positions.is_empty() {
            return Ok(());
        }

        tracing::info!("closing {} positions", self.positions.len());
        let gas_prices = ViewClient::gas_prices(&mut self.view).await?;

        let mut planner = Planner::new(OsRng);
        planner.set_gas_prices(gas_prices);
        planner.set_fee_tier(FeeTier::default().into());

        for id in &self.positions {
            planner.position_close(*id);
        }

        let plan = planner.plan(&mut self.view, self.source).await?;
        let tx = penumbra_sdk_wallet::build_transaction(
            &self.fvk, &mut self.view, &mut self.custody, plan,
        ).await?;

        ViewClient::broadcast_transaction(&mut self.view, tx, true).await?;
        self.positions.clear();

        // Wait for confirmation
        tokio::time::sleep(std::time::Duration::from_secs(7)).await;
        Ok(())
    }

    /// Validate that it's safe to place LP positions this cycle.
    /// Returns Ok(()) if safe, Err with reason if not.
    pub fn validate_oracle(
        &self,
        binance_spread_bps: f64,
        dex_price: Option<f64>,
        binance_mid: f64,
    ) -> Result<()> {
        // Check Binance oracle quality
        if binance_spread_bps > self.safety.max_oracle_spread_bps {
            anyhow::bail!(
                "Binance spread {:.1}bps exceeds safety limit {:.1}bps — oracle unreliable",
                binance_spread_bps, self.safety.max_oracle_spread_bps
            );
        }

        // Check DEX vs Binance deviation (only if DEX has liquidity)
        if let Some(dex_p) = dex_price {
            if dex_p > 0.0 {
                let deviation = ((binance_mid - dex_p) / dex_p * 100.0).abs();
                if deviation > self.safety.max_deviation_pct {
                    anyhow::bail!(
                        "Price deviation {:.1}% (Binance={:.4} vs DEX={:.4}) exceeds {:.1}% limit — \
                         possible manipulation or stale DEX state",
                        deviation, binance_mid, dex_p, self.safety.max_deviation_pct
                    );
                }
            }
        }

        Ok(())
    }

    /// Open LP positions for tightest liquidity with convex pricing.
    ///
    /// Strategy: "Tight top, expensive depth"
    ///   - Level 0: Tiny size at fair price, lowest fee (0bps, close-on-fill)
    ///   - Level 1..N: Exponentially wider spread + higher fees + more size
    ///   - Result: small swaps get great price, large swaps pay progressively more
    ///
    /// This is optimal for illiquid pairs where we want to attract flow
    /// without risking too much capital at any single price point.
    pub async fn open_positions(
        &mut self,
        fair_price: f64,
        direction: f64,
        spread: f64,
        skew: f64,
        size: f64,
        levels: usize,
        max_spread_bps: u32,
        fee_bps: u32,
        risk_fraction: f64,
        dry_run: bool,
        stagger: bool,
    ) -> Result<()> {
        // ─── Safety: circuit breaker check ─────────────────────────────
        if self.safety_state.circuit_breaker_tripped {
            tracing::error!("Circuit breaker is tripped — refusing to place new positions");
            return Ok(());
        }

        let (base_balance, quote_balance) = self.balances().await?;
        if base_balance == 0 && quote_balance == 0 {
            tracing::warn!("Penumbra: no funds available");
            return Ok(());
        }

        // ─── Safety: P&L check ─────────────────────────────────────────
        if !self.safety_state.check_pnl(base_balance, quote_balance, fair_price, &self.safety) {
            return Ok(());
        }

        // ─── Safety: warmup-adjusted risk fraction ─────────────────────
        let effective_fraction = self.safety_state.effective_risk_fraction(risk_fraction, &self.safety);
        self.safety_state.cycle_count += 1;

        let base_to_deploy = (base_balance as f64 * effective_fraction) as u128;
        let quote_to_deploy = (quote_balance as f64 * effective_fraction) as u128;

        let max_pos_quote = self.safety.max_position_quote;
        let mut new_positions: Vec<Position> = Vec::new();

        // Model-driven skew: positive direction = bullish → tighter bids, wider asks
        let effective_skew = skew * 0.5 + direction * 0.3;

        // ─── Convex liquidity ladder ───────────────────────────────────
        // Level 0: tight anchor (5% of capital, 0 fee, at fair price)
        // Level 1: close (10%, low fee, ~10bps from fair)
        // Level 2: mid (20%, base fee, ~30bps)
        // Level 3+: deep (grows exponentially, higher fees, wider spread)
        //
        // Capital allocation: exponential growth — most capital far from fair.
        // Spread: exponential widening — cheap at top, expensive at depth.
        // Fees: step up per level — tight=0bps, close=10bps, mid=fee_bps, deep=2*fee_bps

        let total_levels = levels + 1; // +1 for the anchor level
        let fee_tiers: Vec<u32> = (0..total_levels).map(|i| {
            if i == 0 { 0 }
            else if i == 1 { fee_bps / 5 }    // e.g. 10bps if fee_bps=50
            else if i == 2 { fee_bps / 2 }    // e.g. 25bps
            else { fee_bps + (i as u32 - 2) * (fee_bps / 3) } // escalating
        }).collect();

        // Capital weights: exponential — level 0 gets least, deeper levels get most
        // e.g. 5 levels: [1, 2, 4, 8, 16] → normalized
        let raw_weights: Vec<f64> = (0..total_levels).map(|i| {
            if i == 0 { 1.0 } else { (2.0_f64).powi(i as i32) }
        }).collect();
        let weight_sum: f64 = raw_weights.iter().sum();
        let cap_fracs: Vec<f64> = raw_weights.iter().map(|w| w / weight_sum).collect();

        // Spread per level: exponential from 0 to max_spread_bps
        // Level 0: 0 bps (at fair price)
        // Level N: max_spread_bps
        let spread_bps_per_level: Vec<f64> = (0..total_levels).map(|i| {
            if i == 0 { 0.0 }
            else {
                let t = i as f64 / levels as f64; // 0..1
                // Exponential curve: most of the spread is at deeper levels
                let exp_t = (t * t) * (1.0 + spread * 0.5); // model spread modulates curve
                exp_t * max_spread_bps as f64
            }
        }).collect();

        for i in 0..total_levels {
            let level_fee = fee_tiers[i].min(max_spread_bps); // fee can't exceed spread
            let close_on_fill = i == 0; // only anchor is close-on-fill
            let offset_bps = spread_bps_per_level[i];
            let cap_frac = cap_fracs[i];

            // Ask side (sell base for quote)
            if base_to_deploy > 0 {
                let level_base = (base_to_deploy as f64 * cap_frac) as u128;
                if level_base > 0 {
                    // Skew: positive → wider asks (we're bullish, less eager to sell)
                    let skewed_offset = offset_bps * (1.0 + effective_skew * 0.5);
                    let ask_price = fair_price * (1.0 + skewed_offset / 10000.0);
                    let desired_quote = (level_base as f64 * ask_price) as u128;
                    let capped_quote = desired_quote.min(max_pos_quote);
                    let capped_base = if desired_quote > max_pos_quote {
                        (max_pos_quote as f64 / ask_price) as u128
                    } else {
                        level_base
                    };

                    if capped_base > 0 {
                        let pos = make_position(
                            capped_base, self.base_id,
                            capped_quote.max(1), self.quote_id,
                            level_fee, close_on_fill,
                        );
                        tracing::info!(
                            "  ask[{}]: {} base @ {:.6} ({:+.0}bps, {}bps fee{})",
                            i, capped_base, ask_price, skewed_offset, level_fee,
                            if close_on_fill { ", close-on-fill" } else { "" }
                        );
                        new_positions.push(pos);
                    }
                }
            }

            // Bid side (buy base with quote)
            if quote_to_deploy > 0 && fair_price > 0.0 {
                let level_quote = (quote_to_deploy as f64 * cap_frac) as u128;
                let capped_quote = level_quote.min(max_pos_quote);
                if capped_quote > 0 {
                    // Skew: positive → tighter bids (we're bullish, more eager to buy)
                    let skewed_offset = offset_bps * (1.0 - effective_skew * 0.5);
                    let bid_price = fair_price * (1.0 - skewed_offset.max(0.0) / 10000.0);
                    if bid_price > 0.0 {
                        let desired_base = (capped_quote as f64 / bid_price) as u128;
                        let pos = make_position(
                            capped_quote, self.quote_id,
                            desired_base.max(1), self.base_id,
                            level_fee, close_on_fill,
                        );
                        tracing::info!(
                            "  bid[{}]: {} quote @ {:.6} ({:+.0}bps, {}bps fee{})",
                            i, capped_quote, bid_price, skewed_offset, level_fee,
                            if close_on_fill { ", close-on-fill" } else { "" }
                        );
                        new_positions.push(pos);
                    }
                }
            }
        }

        // Submit
        if new_positions.is_empty() {
            tracing::warn!("Penumbra: no positions to open");
            return Ok(());
        }

        if dry_run {
            tracing::info!("[DRY] would open {} positions", new_positions.len());
            return Ok(());
        }

        if stagger {
            // ─── Staggered: one position per transaction per block ───
            tracing::info!("staggering {} positions (1 per block)", new_positions.len());
            for (i, pos) in new_positions.iter().enumerate() {
                let t0 = std::time::Instant::now();
                tracing::info!("  submitting position {}/{}", i + 1, new_positions.len());

                let gas_prices = ViewClient::gas_prices(&mut self.view).await?;
                let mut planner = Planner::new(OsRng);
                planner.set_gas_prices(gas_prices);
                planner.set_fee_tier(FeeTier::default().into());
                planner.position_open(pos.clone());

                let plan = planner.plan(&mut self.view, self.source).await?;
                let tx = penumbra_sdk_wallet::build_transaction(
                    &self.fvk, &mut self.view, &mut self.custody, plan,
                ).await?;

                ViewClient::broadcast_transaction(&mut self.view, tx, true).await?;
                self.positions.push(pos.id());

                let elapsed = t0.elapsed();
                tracing::info!(
                    "  position {}/{} submitted ({:.1}s prove+broadcast)",
                    i + 1, new_positions.len(), elapsed.as_secs_f64()
                );

                // Wait for next block before submitting the next one
                if i + 1 < new_positions.len() {
                    // Check if anchor got instantly filled (early exit)
                    if i == 0 && pos.close_on_fill {
                        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                        // Re-check balances — if anchor was arbed, we might want to bail
                        let (b, q) = self.balances().await?;
                        let prev_total = base_balance as f64 * fair_price + quote_balance as f64;
                        let curr_total = b as f64 * fair_price + q as f64;
                        if curr_total < prev_total * 0.95 {
                            tracing::warn!(
                                "Anchor may have been arbed (value dropped {:.1}%) — halting stagger",
                                (1.0 - curr_total / prev_total) * 100.0
                            );
                            return Ok(());
                        }
                    } else {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
            tracing::info!("staggered {} positions across {} blocks", self.positions.len(), new_positions.len());
        } else {
            // ─── Batch: all positions in one transaction ─────────────
            tracing::info!("opening {} positions (batch)", new_positions.len());
            let gas_prices = ViewClient::gas_prices(&mut self.view).await?;

            let mut planner = Planner::new(OsRng);
            planner.set_gas_prices(gas_prices);
            planner.set_fee_tier(FeeTier::default().into());

            for pos in &new_positions {
                planner.position_open(pos.clone());
            }

            let plan = planner.plan(&mut self.view, self.source).await?;
            let tx = penumbra_sdk_wallet::build_transaction(
                &self.fvk, &mut self.view, &mut self.custody, plan,
            ).await?;

            ViewClient::broadcast_transaction(&mut self.view, tx, true).await?;
            self.positions = new_positions.iter().map(|p| p.id()).collect();
            tracing::info!("opened {} positions", self.positions.len());
        }

        Ok(())
    }

    /// IBC withdraw from Penumbra to another chain (Osmosis, Noble, etc).
    ///
    /// Channels:
    ///   - channel-4: Penumbra → Osmosis
    ///   - channel-2: Penumbra → Noble (USDC)
    pub async fn ibc_withdraw(
        &mut self,
        denom_metadata: asset::Metadata,
        amount: u128,
        dest_address: &str,
        channel: u64,
    ) -> Result<()> {
        tracing::info!(
            "IBC withdraw: {} {} via channel-{} to {}",
            amount, denom_metadata, channel,
            &dest_address[..20.min(dest_address.len())]
        );

        // Ephemeral return address (for refunds if IBC times out)
        let (return_address, _dtk) = self.fvk
            .incoming()
            .ephemeral_address(OsRng, self.source);

        // Timeout: 2 days from now, rounded to nearest minute for privacy
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_nanos() as u64;
        let two_days_ns = 2 * 24 * 60 * 60 * 1_000_000_000u64;
        let timeout_time = {
            let raw = now_ns + two_days_ns;
            // Round to nearest minute (60 billion ns)
            let minute_ns = 60_000_000_000u64;
            ((raw + minute_ns - 1) / minute_ns) * minute_ns
        };

        // Far-future height timeout (we rely on timestamp timeout)
        let timeout_height = IbcHeight::new(0, 99_999_999)
            .map_err(|e| anyhow::anyhow!("invalid IBC height: {}", e))?;

        let source_channel = ChannelId::from_str(
            &format!("channel-{}", channel)
        )?;

        let withdrawal = Ics20Withdrawal {
            amount: Amount::from(amount),
            denom: denom_metadata,
            destination_chain_address: dest_address.to_string(),
            return_address,
            timeout_height,
            timeout_time,
            source_channel,
            use_compat_address: false,
            use_transparent_address: false,
            ics20_memo: String::new(),
        };

        let gas_prices = ViewClient::gas_prices(&mut self.view).await?;

        let mut planner = Planner::new(OsRng);
        planner.set_gas_prices(gas_prices);
        planner.set_fee_tier(FeeTier::default().into());
        planner.ics20_withdrawal(withdrawal);

        let plan = planner.plan(&mut self.view, self.source).await?;
        let tx = penumbra_sdk_wallet::build_transaction(
            &self.fvk, &mut self.view, &mut self.custody, plan,
        ).await?;

        ViewClient::broadcast_transaction(&mut self.view, tx, true).await?;

        tracing::info!(
            "IBC withdrawal submitted: {} via channel-{} to {}",
            amount, channel, &dest_address[..20.min(dest_address.len())]
        );

        Ok(())
    }
}

/// Create an LP position (sell order).
fn make_position(
    offered_amount: u128,
    offered_asset: asset::Id,
    desired_amount: u128,
    desired_asset: asset::Id,
    fee_bps: u32,
    close_on_fill: bool,
) -> Position {
    let offered = Value {
        amount: Amount::from(offered_amount),
        asset_id: offered_asset,
    };
    let desired = Value {
        amount: Amount::from(desired_amount),
        asset_id: desired_asset,
    };
    let mut pos = SellOrder {
        offered,
        desired,
        fee: fee_bps,
    }
    .into_position(rand::thread_rng());
    pos.close_on_fill = close_on_fill;
    pos
}

/// Simulate a swap to estimate price.
async fn simulate_swap(
    client: &mut SimulationServiceClient<Channel>,
    input: Value,
    output_asset: asset::Id,
) -> Result<Option<Value>> {
    let req = SimulateTradeRequest {
        input: Some(input.into()),
        output: Some(output_asset.into()),
        routing: Some(Routing {
            setting: Some(Setting::Default(Default::default())),
        }),
    };
    let resp = client.simulate_trade(req).await?.into_inner();
    if let Some(swap_exec) = resp.output {
        if let Some(output) = swap_exec.output {
            return Ok(Some(output.try_into()?));
        }
    }
    Ok(None)
}
