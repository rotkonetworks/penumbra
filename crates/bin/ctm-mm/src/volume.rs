//! Volume generation bot — tiny swaps through LP positions to earn LQT rewards.
//!
//! Runs with a SEPARATE wallet from the market maker. Alternates buy/sell
//! with small amounts to generate execution volume on the DEX.
//!
//! Usage:
//!   ctm-mm volume --home ~/.penumbra-vol --swap-size 100000 --interval 30

use anyhow::{Context, Result};
use penumbra_sdk_asset::{asset, Value};
use penumbra_sdk_custody::soft_kms::SoftKms;
use penumbra_sdk_fee::{Fee, FeeTier};
use penumbra_sdk_keys::keys::AddressIndex;
use penumbra_sdk_num::Amount;
use penumbra_sdk_proto::{
    box_grpc_svc,
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

use crate::config::{CustodyConfig, PcliConfig};

/// Run the volume generation loop.
pub async fn run(
    config: &PcliConfig,
    grpc_url: url::Url,
    base_name: &str,
    quote_name: &str,
    swap_size: u128,
    interval_secs: u64,
    account: u32,
    dry_run: bool,
) -> Result<()> {
    // ─── Init view + custody (separate wallet) ─────────────────────
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

    let custody_svc = match &config.custody {
        CustodyConfig::SoftKms(c) => SoftKms::new(c.clone()),
        CustodyConfig::Other => anyhow::bail!("requires SoftKms custody"),
    };
    let custody_server = CustodyServiceServer::new(custody_svc);
    let mut custody = CustodyServiceClient::new(box_grpc_svc::local(custody_server));

    let fvk = config.full_viewing_key.clone();
    let source = AddressIndex::new(account);

    // Wait for sync
    tracing::info!("Volume bot: waiting for view sync...");
    loop {
        match ViewClient::status(&mut view).await {
            Ok(s) if s.full_sync_height > 0 && !s.catching_up => {
                tracing::info!("View synced at height {}", s.full_sync_height);
                break;
            }
            Ok(s) => {
                tracing::info!("Syncing: height={} catching_up={}", s.full_sync_height, s.catching_up);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_secs(5)).await,
        }
    }

    // Resolve assets
    let asset_cache = ViewClient::assets(&mut view).await?;
    let base_denom = asset_cache
        .get_unit(base_name)
        .ok_or_else(|| anyhow::anyhow!("unknown asset: {}", base_name))?;
    let quote_denom = asset_cache
        .get_unit(quote_name)
        .ok_or_else(|| anyhow::anyhow!("unknown quote: {}", quote_name))?;

    let base_id = base_denom.id();
    let quote_id = quote_denom.id();

    tracing::info!(
        "Volume bot: {} / {} | swap_size={} | interval={}s | {}",
        base_name, quote_name, swap_size, interval_secs,
        if dry_run { "DRY" } else { "LIVE" }
    );

    // ─── Main loop: alternate buy/sell ─────────────────────────────
    let mut buy_next = true;
    let mut cycle = 0u64;

    loop {
        cycle += 1;

        // Check balances
        let notes = ViewClient::unspent_notes_by_address_and_asset(&mut view).await?;
        let account_notes = notes.get(&source).cloned().unwrap_or_default();
        let base_bal: u128 = account_notes
            .get(&base_id)
            .map(|n| n.iter().map(|n| u128::from(n.note.amount())).sum())
            .unwrap_or(0);
        let quote_bal: u128 = account_notes
            .get(&quote_id)
            .map(|n| n.iter().map(|n| u128::from(n.note.amount())).sum())
            .unwrap_or(0);

        // Decide direction based on what we have
        let (input_id, output_id, amount, direction) = if buy_next && quote_bal >= swap_size {
            // Buy base with quote
            (quote_id, base_id, swap_size, "BUY")
        } else if !buy_next && base_bal >= swap_size {
            // Sell base for quote
            (base_id, quote_id, swap_size, "SELL")
        } else if quote_bal >= swap_size {
            (quote_id, base_id, swap_size, "BUY")
        } else if base_bal >= swap_size {
            (base_id, quote_id, swap_size, "SELL")
        } else {
            tracing::warn!(
                "Insufficient balance: base={} quote={} need={}",
                base_bal, quote_bal, swap_size
            );
            tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
            continue;
        };

        tracing::info!(
            "[cycle {}] {} {} (base={} quote={})",
            cycle, direction, amount, base_bal, quote_bal
        );

        if !dry_run {
            match do_swap(&mut view, &mut custody, &fvk, source, input_id, output_id, amount).await {
                Ok(()) => {
                    tracing::info!("[cycle {}] {} executed", cycle, direction);
                    buy_next = !buy_next; // alternate
                }
                Err(e) => {
                    tracing::error!("[cycle {}] swap failed: {}", cycle, e);
                }
            }
        } else {
            tracing::info!("[cycle {}] DRY: would {} {}", cycle, direction, amount);
            buy_next = !buy_next;
        }

        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
    }
}

/// Execute a swap on the Penumbra DEX.
async fn do_swap(
    view: &mut ViewServiceClient<box_grpc_svc::BoxGrpcService>,
    custody: &mut CustodyServiceClient<box_grpc_svc::BoxGrpcService>,
    fvk: &penumbra_sdk_keys::FullViewingKey,
    source: AddressIndex,
    input_id: asset::Id,
    output_id: asset::Id,
    amount: u128,
) -> Result<()> {
    let gas_prices = ViewClient::gas_prices(view).await?;

    let mut planner = Planner::new(OsRng);
    planner.set_gas_prices(gas_prices);
    planner.set_fee_tier(FeeTier::default().into());

    let input = Value {
        amount: Amount::from(amount),
        asset_id: input_id,
    };

    // Claim address for swap output
    let (claim_address, _) = fvk.incoming().payment_address(source.into());
    let swap_claim_fee = Fee::default();
    planner.swap(input, output_id, swap_claim_fee, claim_address)?;

    let plan = planner.plan(view, source).await?;
    let tx = penumbra_sdk_wallet::build_transaction(fvk, view, custody, plan).await?;
    ViewClient::broadcast_transaction(view, tx, true).await?;

    // Need to claim the swap output after it's processed
    // Wait a couple blocks for the batch swap to execute
    tokio::time::sleep(std::time::Duration::from_secs(12)).await;

    // Claim all pending swap outputs
    let gas_prices = ViewClient::gas_prices(view).await?;
    let mut claim_planner = Planner::new(OsRng);
    claim_planner.set_gas_prices(gas_prices);
    claim_planner.set_fee_tier(FeeTier::default().into());

    match claim_planner.plan(view, source).await {
        Ok(plan) => {
            if plan.actions.is_empty() {
                tracing::debug!("No swap claims pending");
            } else {
                let tx = penumbra_sdk_wallet::build_transaction(fvk, view, custody, plan).await?;
                ViewClient::broadcast_transaction(view, tx, true).await?;
                tracing::info!("Swap claimed");
            }
        }
        Err(e) => tracing::debug!("Claim plan: {}", e),
    }

    Ok(())
}
