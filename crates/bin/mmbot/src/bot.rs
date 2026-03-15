use anyhow::{Context, Result};
use penumbra_sdk_asset::{asset, Value};
use penumbra_sdk_custody::soft_kms::SoftKms;
use penumbra_sdk_dex::{
    lp::position::{self, Position},
    DirectedTradingPair, TradingPair,
};
use penumbra_sdk_keys::keys::AddressIndex;
use penumbra_sdk_num::Amount;
use penumbra_sdk_proto::{
    box_grpc_svc,
    core::component::dex::v1::{
        query_service_client::QueryServiceClient as DexQueryClient,
        simulation_service_client::SimulationServiceClient,
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
use penumbra_sdk_view::{Planner, ViewClient, ViewServer};
use rand_core::OsRng;
use tonic::transport::Channel;
use url::Url;

use crate::bot_state::{MmBotState, StateManager};
use crate::config::{CustodyConfig, PcliConfig};
use crate::ctm::{self, CtmModel, MmDecision};
use crate::dex;

pub struct BotConfig {
    pub config: PcliConfig,
    pub grpc_url: Url,
    pub ctm_model: Option<CtmModel>,
    pub levels: usize,
    pub max_spread_bps: u32,
    pub fee_bps: u32,
    pub risk_fraction: f64,
    pub rebalance_blocks: u64,
    pub conviction: f32,
    pub dry_run: bool,
    pub source: u32,
    pub asset: String,
    pub quote: String,
}

pub async fn run(cfg: BotConfig) -> Result<()> {
    // Set up view service (in-memory, same as pcli)
    let view_svc = ViewServer::load_or_initialize(
        None::<&camino::Utf8Path>,
        None::<&camino::Utf8Path>,
        &cfg.config.full_viewing_key,
        cfg.grpc_url.clone(),
    )
    .await
    .context("failed to initialize view service")?;

    let view_server = ViewServiceServer::new(view_svc);
    let mut view = ViewServiceClient::new(box_grpc_svc::local(view_server));

    // Set up custody (signing) — wrap SoftKms like pcli does
    let custody_svc = match &cfg.config.custody {
        CustodyConfig::SoftKms(soft_kms_config) => SoftKms::new(soft_kms_config.clone()),
        CustodyConfig::Other => {
            anyhow::bail!("mmbot requires SoftKms custody backend");
        }
    };
    let custody_server = CustodyServiceServer::new(custody_svc);
    let mut custody = CustodyServiceClient::new(box_grpc_svc::local(custody_server));

    // DEX query clients via the pd gRPC channel
    let channel = ViewServer::get_pd_channel(cfg.grpc_url.clone()).await?;
    let mut dex_client = DexQueryClient::new(channel.clone());
    let mut sim_client = SimulationServiceClient::new(channel.clone());

    // Resolve asset IDs
    let asset_cache = ViewClient::assets(&mut view).await?;
    let base_denom = asset_cache
        .get_unit(&cfg.asset)
        .ok_or_else(|| anyhow::anyhow!("unknown asset: {}", cfg.asset))?;
    let quote_denom = asset_cache
        .get_unit(&cfg.quote)
        .ok_or_else(|| anyhow::anyhow!("unknown quote asset: {}", cfg.quote))?;

    let base_id = base_denom.id();
    let quote_id = quote_denom.id();
    let source = AddressIndex::new(cfg.source);

    tracing::info!("market making {} / {}", cfg.asset, cfg.quote);

    // Track our open position IDs
    let mut our_positions: Vec<position::Id> = Vec::new();

    // Initialize shared state for arb-daemon coordination
    let state_mgr = StateManager::default_dir().ok();
    let mut mm_state = MmBotState::new(&format!("{}/{}", cfg.asset, cfg.quote));

    loop {
        tracing::info!("--- rebalance cycle ---");

        // 1. Sync view
        if let Err(e) = ViewClient::status(&mut view).await {
            tracing::warn!("view sync error: {}, retrying...", e);
            tokio::time::sleep(std::time::Duration::from_secs(6)).await;
            continue;
        }

        // 2. Get fair price via DEX simulation
        let fair_price = estimate_fair_price(
            &mut sim_client,
            base_id,
            quote_id,
            &base_denom,
            &quote_denom,
        )
        .await;

        let fair_price = match fair_price {
            Ok(p) if p > 0.0 => p,
            Ok(_) => {
                tracing::warn!("fair price is zero, skipping cycle");
                tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                continue;
            }
            Err(e) => {
                tracing::warn!("failed to estimate fair price: {}", e);
                tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                continue;
            }
        };

        tracing::info!("fair price: {:.6} quote per base", fair_price);

        // 3. Get balances
        let notes = ViewClient::unspent_notes_by_address_and_asset(&mut view).await?;
        let account_notes = notes.get(&source).cloned().unwrap_or_default();

        let base_balance: u128 = account_notes
            .get(&base_id)
            .map(|notes| notes.iter().map(|n| u128::from(n.note.amount())).sum())
            .unwrap_or(0);
        let quote_balance: u128 = account_notes
            .get(&quote_id)
            .map(|notes| notes.iter().map(|n| u128::from(n.note.amount())).sum())
            .unwrap_or(0);

        tracing::info!("balances: {} base, {} quote", base_balance, quote_balance);

        // 4. Compute inventory skew
        let base_value = base_balance as f64 * fair_price;
        let total_value = base_value + quote_balance as f64;
        let inventory_skew = if total_value > 0.0 {
            (base_value / total_value - 0.5) * 2.0
        } else {
            0.0
        };

        // 4b. Use arb-daemon's reference price if available (more accurate than DEX sim)
        if let Some(ref mgr) = state_mgr {
            if let Some(arb) = mgr.read_arb_state() {
                if arb.alive && arb.reference_prices.um_usdc_osmosis > 0.0 {
                    let ref_price = arb.reference_prices.um_usdc_osmosis;
                    let deviation = (fair_price - ref_price).abs() / ref_price;
                    if deviation > 0.02 {
                        tracing::info!(
                            "arb-daemon ref price: {:.6} (deviation {:.1}% from DEX sim {:.6})",
                            ref_price, deviation * 100.0, fair_price
                        );
                    }
                }
            }

            // Publish state for arb-daemon
            mm_state.base_balance = base_balance;
            mm_state.quote_balance = quote_balance;
            mm_state.fair_price = fair_price;
            mm_state.inventory_skew = inventory_skew;
            mm_state.open_positions = our_positions.len() as u32;
            mm_state.update();
            if let Err(e) = mgr.write_mmbot_state(&mm_state) {
                tracing::debug!("Failed to write mmbot state: {}", e);
            }
        }

        // 5. Get MM decision from CTM feature server or rule-based fallback
        let decision = match ctm::read_ctm_decision("/tmp/ctm-decision.json") {
            Ok(ctm_dec) => {
                tracing::info!(
                    "CTM signal: bias={:.3}, spread={:.3}, skew={:.3}, cert={:.3}",
                    ctm_dec.bias, ctm_dec.spread, ctm_dec.skew, ctm_dec.certainty,
                );
                ctm::ctm_to_decision(
                    ctm_dec.bias,
                    ctm_dec.spread,
                    ctm_dec.skew,
                    inventory_skew,
                    cfg.levels,
                    cfg.fee_bps,
                )
            }
            Err(e) => {
                tracing::debug!("CTM decision unavailable ({}), using rule-based", e);
                ctm::rule_based_decision(
                    fair_price,
                    inventory_skew,
                    cfg.conviction,
                    cfg.levels,
                    cfg.fee_bps,
                )
            }
        };

        tracing::info!(
            "decision: skew={:.3}, aggression={:.3}, inv_skew={:.3}",
            decision.skew,
            decision.aggression,
            inventory_skew,
        );

        // 6. Close old positions
        if !our_positions.is_empty() && !cfg.dry_run {
            tracing::info!("closing {} old positions", our_positions.len());
            let gas_prices = ViewClient::gas_prices(&mut view).await?;

            let mut planner = Planner::new(OsRng);
            planner.set_gas_prices(gas_prices);
            planner.set_fee_tier(penumbra_sdk_fee::FeeTier::default().into());

            for id in &our_positions {
                planner.position_close(*id);
            }

            match planner.plan(&mut view, source).await {
                Ok(plan) => {
                    let tx = penumbra_sdk_wallet::build_transaction(
                        &cfg.config.full_viewing_key,
                        &mut view,
                        &mut custody,
                        plan,
                    )
                    .await?;

                    ViewClient::broadcast_transaction(&mut view, tx, true).await?;
                    tracing::info!("closed positions");
                }
                Err(e) => {
                    tracing::error!("failed to close positions: {}", e);
                }
            }

            // Wait for close to be confirmed
            tokio::time::sleep(std::time::Duration::from_secs(7)).await;
            our_positions.clear();
        }

        // 7. Phase 1: Price-anchoring limit orders (close_on_fill=true)
        //
        // These small orders define where we think fair value is.
        // In Penumbra's batch auction, all LPs contribute to the clearing
        // price. By placing aggressive limit orders at our reference price,
        // we anchor the batch clearing price to reality.
        //
        // Without this, someone could skew the price by being the only
        // liquidity at a manipulated level.
        let spread_range = fair_price * (cfg.max_spread_bps as f64 / 10000.0);
        let mut new_positions: Vec<Position> = Vec::new();

        let base_to_sell = (base_balance as f64 * cfg.risk_fraction) as u128;
        let quote_to_spend = (quote_balance as f64 * cfg.risk_fraction) as u128;

        // Price anchor: small tight orders at fair value (5% of capital, 0 fee)
        let anchor_frac = 0.05;
        if base_to_sell > 0 {
            let anchor_base = (base_to_sell as f64 * anchor_frac) as u128;
            if anchor_base > 0 {
                let offered = Value {
                    amount: Amount::from(anchor_base),
                    asset_id: base_id,
                };
                let desired_amount = (anchor_base as f64 * fair_price) as u128;
                let desired = Value {
                    amount: Amount::from(desired_amount.max(1)),
                    asset_id: quote_id,
                };
                let pos = dex::make_sell_position(offered, desired, 0, true);
                tracing::info!(
                    "  anchor ask: {} base @ {:.6} (0bps, close-on-fill)",
                    anchor_base, fair_price,
                );
                new_positions.push(pos);
            }
        }
        if quote_to_spend > 0 {
            let anchor_quote = (quote_to_spend as f64 * anchor_frac) as u128;
            if anchor_quote > 0 && fair_price > 0.0 {
                let offered = Value {
                    amount: Amount::from(anchor_quote),
                    asset_id: quote_id,
                };
                let desired_amount = (anchor_quote as f64 / fair_price) as u128;
                let desired = Value {
                    amount: Amount::from(desired_amount.max(1)),
                    asset_id: base_id,
                };
                let pos = dex::make_buy_position(offered, desired, 0, true);
                tracing::info!(
                    "  anchor bid: {} quote @ {:.6} (0bps, close-on-fill)",
                    anchor_quote, fair_price,
                );
                new_positions.push(pos);
            }
        }

        // 8. Phase 2: LP positions at multiple levels (earning fees)
        let remaining_base = base_to_sell - (base_to_sell as f64 * anchor_frac) as u128;
        let remaining_quote = quote_to_spend - (quote_to_spend as f64 * anchor_frac) as u128;

        for i in 0..cfg.levels {
            // Ask: sell base for quote
            if remaining_base > 0 {
                let offset = decision.ask_offsets[i] * spread_range
                    * (1.0 - decision.aggression * 0.5)
                    * (1.0 + decision.skew * 0.3);

                let ask_price = fair_price + offset;
                let level_base = (remaining_base as f64 * decision.ask_sizes[i]) as u128;

                if level_base > 0 {
                    let offered = Value {
                        amount: Amount::from(level_base),
                        asset_id: base_id,
                    };
                    let desired_amount = (level_base as f64 * ask_price) as u128;
                    let desired = Value {
                        amount: Amount::from(desired_amount.max(1)),
                        asset_id: quote_id,
                    };

                    let pos = dex::make_sell_position(
                        offered, desired, decision.ask_fees[i], false,
                    );

                    tracing::info!(
                        "  ask[{}]: {} base @ {:.6} ({}bps fee)",
                        i, level_base, ask_price, decision.ask_fees[i],
                    );
                    new_positions.push(pos);
                }
            }

            // Bid: buy base with quote
            if remaining_quote > 0 {
                let offset = decision.bid_offsets[i] * spread_range
                    * (1.0 - decision.aggression * 0.5)
                    * (1.0 - decision.skew * 0.3);

                let bid_price = fair_price - offset;
                let level_quote = (remaining_quote as f64 * decision.bid_sizes[i]) as u128;

                if level_quote > 0 && bid_price > 0.0 {
                    let offered = Value {
                        amount: Amount::from(level_quote),
                        asset_id: quote_id,
                    };
                    let desired_amount = (level_quote as f64 / bid_price) as u128;
                    let desired = Value {
                        amount: Amount::from(desired_amount.max(1)),
                        asset_id: base_id,
                    };

                    let pos = dex::make_buy_position(
                        offered, desired, decision.bid_fees[i], false,
                    );

                    tracing::info!(
                        "  bid[{}]: {} quote @ {:.6} ({}bps fee)",
                        i, level_quote, bid_price, decision.bid_fees[i],
                    );
                    new_positions.push(pos);
                }
            }
        }

        // 8. Submit positions
        if !new_positions.is_empty() {
            if cfg.dry_run {
                tracing::info!("[DRY RUN] would open {} positions", new_positions.len());
            } else {
                tracing::info!("opening {} new positions", new_positions.len());
                let gas_prices = ViewClient::gas_prices(&mut view).await?;

                let mut planner = Planner::new(OsRng);
                planner.set_gas_prices(gas_prices);
                planner.set_fee_tier(penumbra_sdk_fee::FeeTier::default().into());

                for pos in &new_positions {
                    planner.position_open(pos.clone());
                }

                match planner.plan(&mut view, source).await {
                    Ok(plan) => {
                        let tx = penumbra_sdk_wallet::build_transaction(
                            &cfg.config.full_viewing_key,
                            &mut view,
                            &mut custody,
                            plan,
                        )
                        .await?;

                        ViewClient::broadcast_transaction(&mut view, tx, true).await?;
                        our_positions = new_positions.iter().map(|p| p.id()).collect();
                        tracing::info!("opened {} positions", our_positions.len());
                    }
                    Err(e) => {
                        tracing::error!("failed to open positions: {}", e);
                    }
                }
            }
        }

        // 9. Sleep
        let sleep_secs = cfg.rebalance_blocks * 6;
        tracing::info!("sleeping {}s until next rebalance", sleep_secs);
        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
    }
}

/// Estimate fair price by simulating small trades in both directions.
async fn estimate_fair_price(
    sim_client: &mut SimulationServiceClient<Channel>,
    base_id: asset::Id,
    quote_id: asset::Id,
    base_denom: &asset::Unit,
    quote_denom: &asset::Unit,
) -> Result<f64> {
    let base_unit_amount = u128::from(base_denom.unit_amount());
    let quote_unit_amount = u128::from(quote_denom.unit_amount());

    // Sell 1 base → how much quote?
    let sell_input = Value {
        amount: Amount::from(base_unit_amount),
        asset_id: base_id,
    };
    let sell_output = dex::simulate_swap(sim_client, sell_input, quote_id).await?;

    // Buy base with 1 quote
    let buy_input = Value {
        amount: Amount::from(quote_unit_amount),
        asset_id: quote_id,
    };
    let buy_output = dex::simulate_swap(sim_client, buy_input, base_id).await?;

    match (sell_output, buy_output) {
        (Some(sell_out), Some(buy_out)) => {
            let sell_quote = u128::from(sell_out.amount);
            let buy_base = u128::from(buy_out.amount);

            // sell_price = quote received / base sold (in display units)
            let sell_price = sell_quote as f64 / quote_unit_amount as f64;

            // buy_price = quote spent / base received (in display units)
            let buy_price = if buy_base > 0 {
                quote_unit_amount as f64 / (buy_base as f64 / base_unit_amount as f64)
            } else {
                sell_price
            };

            let mid = (sell_price + buy_price) / 2.0;
            let spread_pct = if mid > 0.0 {
                (buy_price - sell_price) / mid * 100.0
            } else {
                0.0
            };
            tracing::info!(
                "price: sell={:.6}, buy={:.6}, mid={:.6}, spread={:.1}%",
                sell_price, buy_price, mid, spread_pct,
            );
            Ok(mid)
        }
        (Some(sell_out), None) => {
            let sell_quote = u128::from(sell_out.amount);
            Ok(sell_quote as f64 / quote_unit_amount as f64)
        }
        _ => anyhow::bail!("no liquidity for price estimation"),
    }
}
