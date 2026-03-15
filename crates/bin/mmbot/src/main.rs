mod bot;
mod bot_state;
mod config;
mod ctm;
mod dex;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[clap(name = "mmbot", about = "CTM-powered market maker for Penumbra DEX")]
struct Opt {
    /// Path to pcli config (reuses pcli wallet/keys)
    #[clap(long, env = "PENUMBRA_PCLI_HOME")]
    home: Option<String>,

    /// Path to trained CTM TorchScript model
    #[clap(long, default_value = "trading_ctm.pt")]
    model: String,

    /// gRPC URL for pd node
    #[clap(long)]
    grpc_url: Option<url::Url>,

    /// Trading pair: asset to market-make (e.g., "penumbra")
    #[clap(long, default_value = "penumbra")]
    asset: String,

    /// Quote asset (e.g., "transfer/channel-2/uusdc")
    #[clap(long, default_value = "transfer/channel-2/uusdc")]
    quote: String,

    /// Number of price levels per side
    #[clap(long, default_value = "3")]
    levels: usize,

    /// Maximum spread in basis points
    #[clap(long, default_value = "200")]
    max_spread_bps: u32,

    /// Position fee in basis points
    #[clap(long, default_value = "100")]
    fee_bps: u32,

    /// Fraction of capital to deploy per side (0.0 - 1.0)
    #[clap(long, default_value = "0.3")]
    risk_fraction: f64,

    /// How many blocks between rebalances
    #[clap(long, default_value = "10")]
    rebalance_blocks: u64,

    /// Human conviction input (0=bearish, 50=neutral, 100=bullish)
    #[clap(long, default_value = "50")]
    conviction: f32,

    /// Dry run mode — log positions but don't submit
    #[clap(long)]
    dry_run: bool,

    /// Account index to use
    #[clap(long, default_value = "0")]
    source: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let opt = Opt::parse();

    tracing::info!("mmbot starting");
    tracing::info!("  asset: {}", opt.asset);
    tracing::info!("  quote: {}", opt.quote);
    tracing::info!("  levels: {}", opt.levels);
    tracing::info!("  max spread: {} bps", opt.max_spread_bps);
    tracing::info!("  fee: {} bps", opt.fee_bps);
    tracing::info!("  conviction: {}", opt.conviction);
    tracing::info!("  dry run: {}", opt.dry_run);

    // Load pcli config for wallet access
    let pcli_home = opt.home.clone().unwrap_or_else(|| {
        let dirs = directories::ProjectDirs::from("zone", "penumbra", "pcli")
            .expect("platform data dir");
        dirs.data_dir().to_string_lossy().to_string()
    });
    let config = config::load_pcli_config(&pcli_home)?;
    tracing::info!("loaded wallet from {}", pcli_home);

    // Override gRPC URL if provided
    let grpc_url = opt.grpc_url.unwrap_or(config.grpc_url.clone());
    tracing::info!("connecting to {}", grpc_url);

    // Load CTM model (optional — fall back to rule-based if not available)
    let ctm_model = match ctm::CtmModel::load(&opt.model) {
        Ok(model) => {
            tracing::info!("loaded CTM model from {}", opt.model);
            Some(model)
        }
        Err(e) => {
            tracing::warn!("CTM model not loaded ({}), using rule-based strategy", e);
            None
        }
    };

    // Run the bot
    bot::run(bot::BotConfig {
        config,
        grpc_url,
        ctm_model,
        levels: opt.levels,
        max_spread_bps: opt.max_spread_bps,
        fee_bps: opt.fee_bps,
        risk_fraction: opt.risk_fraction,
        rebalance_blocks: opt.rebalance_blocks,
        conviction: opt.conviction,
        dry_run: opt.dry_run,
        source: opt.source,
        asset: opt.asset,
        quote: opt.quote,
    })
    .await
}
