use anyhow::Result;
use penumbra_sdk_asset::{asset, Value};
use penumbra_sdk_dex::lp::{
    position::Position,
    SellOrder,
};
use penumbra_sdk_proto::core::component::dex::v1::{
    simulation_service_client::SimulationServiceClient,
    SimulateTradeRequest,
    simulate_trade_request::Routing,
    simulate_trade_request::routing::Setting,
};
use tonic::transport::Channel;

/// Simulate a swap to estimate price.
/// Returns the output Value if successful.
pub async fn simulate_swap(
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

    // Extract the output value from SwapExecution proto
    if let Some(swap_exec) = resp.output {
        if let Some(output) = swap_exec.output {
            Ok(Some(output.try_into()?))
        } else {
            Ok(None)
        }
    } else {
        Ok(None)
    }
}

/// Create a sell order position.
pub fn make_sell_position(
    offered: Value,
    desired: Value,
    fee_bps: u32,
    close_on_fill: bool,
) -> Position {
    let mut pos = SellOrder {
        offered,
        desired,
        fee: fee_bps,
    }
    .into_position(rand::thread_rng());

    pos.close_on_fill = close_on_fill;
    pos
}

/// Create a buy order position (offer quote, want base).
pub fn make_buy_position(
    offered_quote: Value,
    desired_base: Value,
    fee_bps: u32,
    close_on_fill: bool,
) -> Position {
    let mut pos = SellOrder {
        offered: offered_quote,
        desired: desired_base,
        fee: fee_bps,
    }
    .into_position(rand::thread_rng());

    pos.close_on_fill = close_on_fill;
    pos
}
