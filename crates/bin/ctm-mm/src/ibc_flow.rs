//! IBC flow monitoring — scrape Hermes metrics for real-time cross-chain flow data.
//!
//! We run Hermes on the same machine (bkk07 container 1102). By watching
//! send_packet and acknowledgement counters, we detect when someone is
//! bridging funds TO Penumbra — meaning they're about to swap on the DEX.
//!
//! This gives us a ~5-15 second heads-up to adjust LP positioning.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::RwLock;

/// Tracked IBC channels relevant for Penumbra LP.
const CHANNELS: &[(&str, &str)] = &[
    ("osmosis-1", "channel-79703"),      // Osmosis → Penumbra
    ("noble-1", "channel-89"),           // Noble → Penumbra (USDC)
    ("celestia", "channel-35"),          // Celestia → Penumbra (TIA)
    ("cosmoshub-4", "channel-940"),      // Cosmos Hub → Penumbra
    ("penumbra-1", "channel-4"),         // Penumbra → Osmosis (outflow)
    ("penumbra-1", "channel-2"),         // Penumbra → Noble (outflow)
    ("penumbra-1", "channel-3"),         // Penumbra → Celestia (outflow)
];

/// Snapshot of IBC flow metrics at a point in time.
#[derive(Debug, Clone, Default)]
pub struct IbcFlowSnapshot {
    /// send_packet counts per (chain, channel) — inbound to Penumbra
    pub send_packets: HashMap<String, u64>,
    /// ack counts per (chain, channel)
    pub ack_packets: HashMap<String, u64>,
    /// Timestamp of scrape
    pub timestamp: Option<Instant>,
}

/// Real-time IBC flow state with delta computation.
#[derive(Debug, Clone)]
pub struct IbcFlowState {
    pub current: IbcFlowSnapshot,
    pub previous: IbcFlowSnapshot,
    /// Delta per second for send packets (inflow rate)
    pub inflow_rate: f64,
    /// Delta per second for outflow packets
    pub outflow_rate: f64,
    /// Pending (unreceived) packets — potential incoming swaps
    pub pending_inflow: u64,
    /// Last update
    pub last_update: Instant,
}

impl Default for IbcFlowState {
    fn default() -> Self {
        Self {
            current: IbcFlowSnapshot::default(),
            previous: IbcFlowSnapshot::default(),
            inflow_rate: 0.0,
            outflow_rate: 0.0,
            pending_inflow: 0,
            last_update: Instant::now(),
        }
    }
}

impl IbcFlowState {
    /// Features for the model (normalized).
    /// Returns 6 features: [inflow_rate, outflow_rate, net_flow, pending, osmosis_delta, noble_delta]
    pub fn to_features(&self) -> [f32; 6] {
        let net = self.inflow_rate - self.outflow_rate;

        // Per-channel deltas
        let osmo_delta = self.channel_delta("osmosis-1", "channel-79703");
        let noble_delta = self.channel_delta("noble-1", "channel-89");

        [
            (self.inflow_rate as f32).min(10.0) / 10.0,   // normalize to [0, 1]
            (self.outflow_rate as f32).min(10.0) / 10.0,
            (net as f32).clamp(-5.0, 5.0) / 5.0,          // normalize to [-1, 1]
            (self.pending_inflow as f32).min(20.0) / 20.0,
            (osmo_delta as f32).min(10.0) / 10.0,
            (noble_delta as f32).min(10.0) / 10.0,
        ]
    }

    fn channel_delta(&self, chain: &str, channel: &str) -> f64 {
        let key = format!("{}:{}", chain, channel);
        let curr = self.current.send_packets.get(&key).copied().unwrap_or(0);
        let prev = self.previous.send_packets.get(&key).copied().unwrap_or(0);
        curr.saturating_sub(prev) as f64
    }
}

pub type SharedIbcFlow = Arc<RwLock<IbcFlowState>>;

/// Spawn a background task that scrapes Hermes metrics every `interval`.
pub fn spawn_ibc_flow_monitor(
    hermes_url: &str,
    interval: Duration,
) -> (SharedIbcFlow, tokio::task::JoinHandle<()>) {
    let state = Arc::new(RwLock::new(IbcFlowState::default()));
    let state_clone = state.clone();
    let url = format!("{}/metrics", hermes_url.trim_end_matches('/'));

    let handle = tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");

        loop {
            match scrape_hermes(&client, &url).await {
                Ok(snapshot) => {
                    let mut s = state_clone.write().await;
                    let now = Instant::now();

                    // Compute rates from previous snapshot
                    if let Some(prev_ts) = s.current.timestamp {
                        let dt = now.duration_since(prev_ts).as_secs_f64().max(1.0);

                        let mut inflow_delta = 0u64;
                        let mut outflow_delta = 0u64;

                        for (chain, channel) in CHANNELS {
                            let key = format!("{}:{}", chain, channel);
                            let curr = snapshot.send_packets.get(&key).copied().unwrap_or(0);
                            let prev = s.current.send_packets.get(&key).copied().unwrap_or(0);
                            let delta = curr.saturating_sub(prev);

                            if chain.starts_with("penumbra") {
                                outflow_delta += delta;
                            } else {
                                inflow_delta += delta;
                            }
                        }

                        s.inflow_rate = inflow_delta as f64 / dt;
                        s.outflow_rate = outflow_delta as f64 / dt;
                    }

                    // Pending = send_packets - ack_packets for inbound channels
                    let mut pending = 0u64;
                    for (chain, channel) in CHANNELS {
                        if chain.starts_with("penumbra") {
                            continue; // skip outflow channels
                        }
                        let key = format!("{}:{}", chain, channel);
                        let sent = snapshot.send_packets.get(&key).copied().unwrap_or(0);
                        let acked = snapshot.ack_packets.get(&key).copied().unwrap_or(0);
                        pending += sent.saturating_sub(acked);
                    }
                    s.pending_inflow = pending;

                    s.previous = s.current.clone();
                    s.current = snapshot;
                    s.last_update = now;
                }
                Err(e) => {
                    tracing::debug!("Hermes scrape failed: {}", e);
                }
            }

            tokio::time::sleep(interval).await;
        }
    });

    (state, handle)
}

/// Parse Hermes Prometheus metrics for IBC packet counters.
async fn scrape_hermes(
    client: &reqwest::Client,
    url: &str,
) -> Result<IbcFlowSnapshot> {
    let body = client.get(url).send().await?.text().await?;
    let mut snapshot = IbcFlowSnapshot {
        send_packets: HashMap::new(),
        ack_packets: HashMap::new(),
        timestamp: Some(Instant::now()),
    };

    for line in body.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        // Parse: metric_name{labels} value
        if line.starts_with("send_packet_events_total{")
            || line.starts_with("cleared_send_packet_events_total{")
        {
            if let Some((chain, channel, value)) = parse_metric_line(line) {
                let key = format!("{}:{}", chain, channel);
                *snapshot.send_packets.entry(key).or_default() += value;
            }
        } else if line.starts_with("acknowledgement_events_total{")
            || line.starts_with("cleared_acknowledgment_events_total{")
        {
            if let Some((chain, channel, value)) = parse_metric_line(line) {
                let key = format!("{}:{}", chain, channel);
                *snapshot.ack_packets.entry(key).or_default() += value;
            }
        }
    }

    Ok(snapshot)
}

/// Parse a Prometheus metric line like:
/// send_packet_events_total{chain="osmosis-1",channel="channel-79703",...} 7
fn parse_metric_line(line: &str) -> Option<(String, String, u64)> {
    let brace_start = line.find('{')?;
    let brace_end = line.find('}')?;
    let labels = &line[brace_start + 1..brace_end];
    let value_str = line[brace_end + 1..].trim();
    let value: u64 = value_str.parse().ok()?;

    let mut chain = None;
    let mut channel = None;

    for part in labels.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("chain=\"") {
            chain = Some(v.trim_end_matches('"').to_string());
        } else if let Some(v) = part.strip_prefix("channel=\"") {
            channel = Some(v.trim_end_matches('"').to_string());
        }
    }

    Some((chain?, channel?, value))
}
