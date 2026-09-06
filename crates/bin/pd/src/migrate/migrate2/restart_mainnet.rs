//! `restart-mainnet-1`: coordinated restart of `penumbra-1` after the
//! liveness halt of 2026-09-02 at height 12,598,600.
//!
//! The chain stopped because validators holding more than one third of the
//! voting power went offline, so no block could reach a two-thirds precommit.
//! Nothing about the application state is wrong. This migration:
//!
//! 1. marks the absent validators `Disabled`, so the remaining validators hold
//!    100% of the voting power;
//! 2. executes an **empty application block 12,598,601** itself, through the
//!    same `App::begin_block` / `end_block` / `commit` path `pd` uses for every
//!    block, with a fixed header;
//! 3. writes a checkpoint genesis whose `initial_height` is **12,598,602**.
//!
//! Step 2 exists because every validator that was online at the halt already
//! signed votes for height 12,598,601 (rounds 0 to 2) on the halted chain.
//! CometBFT's double-sign protection will not let them sign those rounds
//! again, and a round only advances on two-thirds of votes, so a chain
//! restarted at 12,598,601 can never produce a block. Restarting at 12,598,602
//! sidesteps the signed rounds entirely, and no vote from the halted chain
//! can ever be turned into evidence against a validator on the new chain,
//! because the halted chain never had a height 12,598,602. Executing the empty
//! block inside the migration keeps the application state and the compact
//! block stream contiguous, so wallets keep syncing. There is simply no
//! CometBFT block 12,598,601.
//!
//! It also bumps the app version to the one compiled into this binary, so the
//! restart doubles as the 2.1 upgrade.
//!
//! Every parameter that affects the resulting state or genesis is compiled in.
//! Operators run exactly one command and compare the printed root hash with
//! the release notes. The hidden `--unsafe-test-plan` flag exists only for
//! rehearsing the procedure on a private copy of the chain state with fresh
//! consensus keys and a different chain id.

use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use cnidarium::{StateDelta, Storage};
use jmt::RootHash;
use penumbra_sdk_app::app::App;
use penumbra_sdk_app::app::StateReadExt as _;
use penumbra_sdk_app::app::StateWriteExt as _;
use penumbra_sdk_app::app_version::migrate_app_version;
use penumbra_sdk_app::{APP_VERSION, SUBSTORE_PREFIXES};
use penumbra_sdk_governance::StateWriteExt as _;
use penumbra_sdk_sct::component::clock::{EpochManager, EpochRead};
use penumbra_sdk_stake::component::restart::RestartStateWrite;
use penumbra_sdk_stake::component::validator_handler::ValidatorDataRead;
use penumbra_sdk_stake::component::ConsensusIndexRead;
use penumbra_sdk_stake::validator::State as ValidatorState;
use penumbra_sdk_stake::CurrentConsensusKeys;
use serde::Deserialize;
use tendermint::abci::request;
use tendermint::abci::types::{BlockSignatureInfo, CommitInfo, Validator, VoteInfo};
use tendermint::block::BlockIdFlag;
use tendermint::PublicKey;

use super::framework::Migration;

/// Height of the last block committed on `penumbra-1` before the halt.
pub const EXPECTED_PRE_UPGRADE_HEIGHT: u64 = 12_598_600;

/// App-state root hash at [`EXPECTED_PRE_UPGRADE_HEIGHT`], as reported by
/// `last_block_app_hash` in `/abci_info` on every node that reached the halt
/// (confirmed on rotko's validator and on polkachu's RPC node). The migration
/// refuses to run on any other state.
pub const EXPECTED_PRE_UPGRADE_ROOT_HASH: Option<&str> =
    Some("6fd4f811f8e1fcc2c67d7ea2ccc75cef228acc2b9a738b8514c9e163c5cd859a");

/// Timestamp of the empty application block 12,598,601 executed by the
/// migration. Five seconds after the last real block (2026-09-02T19:37:07Z),
/// and before [`GENESIS_START`].
pub const SYNTHETIC_BLOCK_TIME: &str = "2026-09-02T19:37:12Z";

/// Genesis time of the restarted chain. CometBFT will not start proposing
/// before this instant, so it doubles as the coordinated start time. Every
/// operator must produce a genesis with this exact value, or their node is on
/// a different chain.
pub const GENESIS_START: &str = "2026-09-08T12:00:00Z";

/// Validators absent from consensus at height 12,598,601, identified by their
/// CometBFT ed25519 consensus public key (base64, as shown by `/validators`).
pub const ABSENT_VALIDATORS: &[(&str, &str)] = &[
    ("iqlusion", "HSuFV7cxLkwVQ4XVgzbIkE7aNkaTO/KF4Vlhex/r32A="),
    (
        "Tessellated",
        "lNC3joFWZ2m8QHOEUigOormsSUmGjS0ADGHjUacO7PM=",
    ),
];

/// A consensus-key replacement, used only when rehearsing on a private copy.
#[derive(Debug, Clone, Deserialize)]
pub struct Rekey {
    /// Existing consensus key (base64 ed25519).
    pub old: String,
    /// Replacement consensus key (base64 ed25519).
    pub new: String,
}

/// What the migration does to the validator set.
#[derive(Debug, Clone, Deserialize)]
pub struct RestartPlan {
    /// Refuse to run unless the local state is at exactly this height.
    pub expected_pre_upgrade_height: u64,
    /// Refuse to run unless the local state has exactly this root hash (hex).
    #[serde(default)]
    pub expected_pre_upgrade_root_hash: Option<String>,
    /// Consensus keys (base64 ed25519) of validators to disable.
    pub disable: Vec<String>,
    /// Testing only: disable every validator in the consensus set that is not
    /// being rekeyed, so the rehearsal set consists of the rekeyed nodes alone.
    #[serde(default)]
    pub disable_all_others: bool,
    /// Testing only: consensus-key replacements.
    #[serde(default)]
    pub rekey: Vec<Rekey>,
    /// Testing only: chain id of the rehearsal network.
    #[serde(default)]
    pub chain_id: Option<String>,
    /// Testing only: genesis time override (RFC 3339).
    #[serde(default)]
    pub genesis_time: Option<String>,
}

/// A validator that keeps voting power after the restart, as CometBFT sees it.
struct ActiveValidator {
    name: String,
    consensus_key: PublicKey,
    power: u64,
}

/// Build the deterministic header of the empty block executed by the migration.
///
/// `app_hash` is the application root hash after the validator changes, which
/// is what a real block at this height would carry. `validators` is the
/// post-restart active set: both the current and next validator set hash are
/// derived from it, the way CometBFT would compute them.
fn synthetic_begin_block(
    chain_id: &str,
    height: u64,
    time: tendermint::Time,
    app_hash: RootHash,
    validators: &[ActiveValidator],
) -> Result<request::BeginBlock> {
    let infos: Vec<tendermint::validator::Info> = validators
        .iter()
        .map(|v| {
            Ok(tendermint::validator::Info::new(
                v.consensus_key,
                tendermint::vote::Power::try_from(v.power).context("voting power")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let validator_set = tendermint::validator::Set::without_proposer(infos);
    let validators_hash = validator_set.hash();

    // sha256 of the empty string: the hash CometBFT uses for empty commits,
    // empty transaction lists and empty evidence lists.
    let empty_hash = tendermint::Hash::Sha256(
        hex::decode("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")?
            .try_into()
            .expect("32 bytes"),
    );

    let header = tendermint::block::Header {
        version: tendermint::block::header::Version {
            block: 11,
            app: APP_VERSION,
        },
        chain_id: chain_id.parse().context("chain id")?,
        height: height.try_into().context("height")?,
        time,
        last_block_id: None,
        last_commit_hash: Some(empty_hash),
        data_hash: Some(empty_hash),
        validators_hash,
        next_validators_hash: validators_hash,
        consensus_hash: empty_hash,
        app_hash: tendermint::AppHash::try_from(app_hash.0.to_vec()).context("app hash")?,
        last_results_hash: Some(empty_hash),
        evidence_hash: Some(empty_hash),
        // Nobody proposed this block.
        proposer_address: tendermint::account::Id::new([0u8; 20]),
    };

    // Count every remaining validator as having signed the previous block, so
    // the empty block neither rewards nor penalises anyone's uptime.
    let votes = validators
        .iter()
        .map(|v| {
            Ok(VoteInfo {
                validator: Validator {
                    address: tendermint::account::Id::from(v.consensus_key)
                        .as_bytes()
                        .try_into()
                        .expect("20-byte address"),
                    power: tendermint::vote::Power::try_from(v.power).context("voting power")?,
                },
                sig_info: BlockSignatureInfo::Flag(BlockIdFlag::Commit),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(request::BeginBlock {
        hash: tendermint::Hash::None,
        header,
        last_commit_info: CommitInfo {
            round: 0u8.into(),
            votes,
        },
        byzantine_validators: vec![],
    })
}

impl RestartPlan {
    /// The compiled-in plan for `penumbra-1`.
    pub fn mainnet_1() -> Self {
        Self {
            expected_pre_upgrade_height: EXPECTED_PRE_UPGRADE_HEIGHT,
            expected_pre_upgrade_root_hash: EXPECTED_PRE_UPGRADE_ROOT_HASH.map(str::to_owned),
            disable: ABSENT_VALIDATORS
                .iter()
                .map(|(_, key)| (*key).to_owned())
                .collect(),
            disable_all_others: false,
            rekey: vec![],
            chain_id: None,
            genesis_time: None,
        }
    }

    /// Load a rehearsal plan from a JSON file.
    pub fn from_test_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading test plan {}", path.display()))?;
        let plan: Self = serde_json::from_str(&raw).context("parsing test plan JSON")?;
        Ok(plan)
    }

    /// The genesis time this plan produces.
    pub fn genesis_start(&self) -> Result<tendermint::time::Time> {
        let raw = self.genesis_time.as_deref().unwrap_or(GENESIS_START);
        raw.parse::<tendermint::time::Time>()
            .with_context(|| format!("invalid genesis time {raw:?}"))
    }
}

fn parse_consensus_key(b64: &str) -> Result<PublicKey> {
    serde_json::from_value(serde_json::json!({
        "type": "tendermint/PubKeyEd25519",
        "value": b64,
    }))
    .with_context(|| format!("invalid ed25519 consensus key {b64:?}"))
}

pub struct RestartMainnet1Migration {
    plan: RestartPlan,
    unsafe_test: bool,
}

impl RestartMainnet1Migration {
    pub fn new(plan: RestartPlan, unsafe_test: bool) -> Self {
        Self { plan, unsafe_test }
    }

    /// The validators that keep voting power after `migrate_inner`, in the
    /// order CometBFT will learn them at `InitChain`.
    async fn active_validators(
        &self,
        state: &StateDelta<cnidarium::Snapshot>,
    ) -> Result<Vec<ActiveValidator>> {
        let mut active = Vec::new();
        for identity_key in state.get_consensus_set().await? {
            let validator = state
                .get_validator_definition(&identity_key)
                .await?
                .with_context(|| format!("no definition for {identity_key}"))?;
            let validator_state = state
                .get_validator_state(&identity_key)
                .await?
                .with_context(|| format!("no state for {identity_key}"))?;
            if !matches!(validator_state, ValidatorState::Active) {
                continue;
            }
            let power = state
                .get_validator_power(&identity_key)
                .await?
                .unwrap_or_default()
                .value();
            if power == 0 {
                continue;
            }
            active.push(ActiveValidator {
                name: validator.name,
                consensus_key: validator.consensus_key,
                power: u64::try_from(power).context("voting power fits in u64")?,
            });
        }
        ensure!(!active.is_empty(), "no active validators after the changes");
        Ok(active)
    }
}

impl Migration for RestartMainnet1Migration {
    fn name(&self) -> &'static str {
        "restart-mainnet-1"
    }

    fn target_app_version(&self) -> Option<u64> {
        Some(APP_VERSION)
    }

    /// Same as the default, plus a hard check that the local state is the
    /// state this migration was written and tested against.
    async fn prepare(
        &self,
        pd_home: &std::path::PathBuf,
        _comet_home: Option<&std::path::PathBuf>,
    ) -> Result<(RootHash, u64)> {
        let storage = Storage::load(pd_home.join("rocksdb"), SUBSTORE_PREFIXES.to_vec()).await?;
        let state = storage.latest_snapshot();
        let root_hash: RootHash = state
            .root_hash()
            .await
            .expect("chain state has a root hash")
            .into();
        let height = state
            .get_block_height()
            .await
            .expect("chain state has a block height");
        storage.release().await;

        ensure!(
            height == self.plan.expected_pre_upgrade_height,
            "local state is at height {height}, but this restart is defined at height {}; \
             do not run it on any other state",
            self.plan.expected_pre_upgrade_height
        );
        if let Some(expected) = &self.plan.expected_pre_upgrade_root_hash {
            let actual = hex::encode(root_hash.0);
            ensure!(
                actual.eq_ignore_ascii_case(expected),
                "local state root hash {actual} does not match the expected {expected}; \
                 your node's state differs from the network's at height {height}"
            );
        }
        Ok((root_hash, height))
    }

    /// Unlike the default, this runs in three commits:
    ///
    /// 1. the validator changes, committed in place at version 12,598,600;
    /// 2. an empty application block 12,598,601 (version 12,598,601);
    /// 3. the app version bump, the ready-to-start bit and the height reset
    ///    that lets CometBFT issue `InitChain`, committed in place.
    ///
    /// The returned height is `12,598,602`: the genesis `initial_height`.
    async fn migrate(
        &self,
        pd_home: &std::path::PathBuf,
        _comet_home: Option<&std::path::PathBuf>,
    ) -> Result<(RootHash, u64)> {
        let storage = Storage::load(pd_home.join("rocksdb"), SUBSTORE_PREFIXES.to_vec()).await?;
        let initial_state = storage.latest_snapshot();
        let pre_upgrade_height = initial_state
            .get_block_height()
            .await
            .expect("chain state has a block height");
        let synthetic_height = pre_upgrade_height + 1;
        let post_upgrade_height = pre_upgrade_height + 2;
        ensure!(
            storage.latest_version() == pre_upgrade_height,
            "state version {} does not match block height {pre_upgrade_height}",
            storage.latest_version()
        );

        // 1. Validator set changes and the app version bump.
        let mut delta = StateDelta::new(initial_state);
        migrate_app_version(&mut delta, APP_VERSION).await?;
        self.migrate_inner(&mut delta).await?;
        let active = self.active_validators(&delta).await?;
        let chain_id = delta.get_chain_id().await?;
        let root_after_changes = storage.commit_in_place(delta).await?;
        tracing::info!(
            ?root_after_changes,
            "committed validator changes at height {pre_upgrade_height}"
        );

        // 2. The empty block.
        let time: tendermint::Time = SYNTHETIC_BLOCK_TIME
            .parse()
            .context("synthetic block time")?;
        let begin_block = synthetic_begin_block(
            &chain_id,
            synthetic_height,
            time,
            root_after_changes,
            &active,
        )?;
        tracing::info!(
            height = synthetic_height,
            %time,
            validators_hash = %begin_block.header.validators_hash,
            "executing the empty application block"
        );
        let mut app = App::new(storage.latest_snapshot());
        let begin_events = app.begin_block(&begin_block).await;
        let end_events = app
            .end_block(&request::EndBlock {
                height: synthetic_height as i64,
            })
            .await;
        let root_after_block = app.commit(storage.clone()).await;
        ensure!(
            storage.latest_version() == synthetic_height,
            "state version {} after the empty block, expected {synthetic_height}",
            storage.latest_version()
        );
        tracing::info!(
            ?root_after_block,
            begin_block_events = begin_events.len(),
            end_block_events = end_events.len(),
            "empty block {synthetic_height} committed"
        );

        // 3. Let the node start, and let CometBFT issue InitChain.
        let mut delta = StateDelta::new(storage.latest_snapshot());
        let committed_height = delta.get_block_height().await?;
        ensure!(
            committed_height == synthetic_height,
            "block height {committed_height} after the empty block, expected {synthetic_height}"
        );
        delta.ready_to_start();
        delta.put_block_height(0u64);
        let post_upgrade_root_hash = storage.commit_in_place(delta).await?;
        ensure!(
            storage.latest_version() == synthetic_height,
            "state version {} after the final commit, expected {synthetic_height}",
            storage.latest_version()
        );
        tracing::info!(
            ?post_upgrade_root_hash,
            post_upgrade_height,
            "post-migration root hash; CometBFT starts at initial_height {post_upgrade_height}"
        );
        storage.release().await;

        Ok((post_upgrade_root_hash, post_upgrade_height))
    }

    async fn migrate_inner(&self, delta: &mut StateDelta<cnidarium::Snapshot>) -> Result<()> {
        let plan = &self.plan;
        let testing_fields_used = plan.disable_all_others
            || !plan.rekey.is_empty()
            || plan.chain_id.is_some()
            || plan.genesis_time.is_some();
        ensure!(
            self.unsafe_test || !testing_fields_used,
            "rehearsal-only plan fields used outside of --unsafe-test-plan"
        );

        if let Some(chain_id) = &plan.chain_id {
            let old = delta.get_chain_id().await?;
            tracing::warn!(%old, new = %chain_id, "REHEARSAL: overriding chain id");
            delta.put_chain_id(chain_id.clone());
        }

        let to_disable = plan
            .disable
            .iter()
            .map(|k| parse_consensus_key(k))
            .collect::<Result<Vec<_>>>()?;
        let rekeys = plan
            .rekey
            .iter()
            .map(|r| Ok((parse_consensus_key(&r.old)?, parse_consensus_key(&r.new)?)))
            .collect::<Result<Vec<(PublicKey, PublicKey)>>>()?;

        // Walk the consensus set: everything CometBFT could be told about.
        let consensus_set = delta.get_consensus_set().await?;
        ensure!(!consensus_set.is_empty(), "consensus set index is empty");

        let mut disabled_keys: Vec<PublicKey> = Vec::new();
        let mut found_listed = 0usize;
        let mut remaining_power: u128 = 0;
        let mut remaining: Vec<(String, u128)> = Vec::new();

        for identity_key in &consensus_set {
            let validator = delta
                .get_validator_definition(identity_key)
                .await?
                .with_context(|| format!("no definition for {identity_key}"))?;
            let state = delta
                .get_validator_state(identity_key)
                .await?
                .with_context(|| format!("no state for {identity_key}"))?;
            let power: u128 = delta
                .get_validator_power(identity_key)
                .await?
                .unwrap_or_default()
                .value();
            let consensus_key = validator.consensus_key;

            let listed = to_disable.contains(&consensus_key);
            let rekeyed = rekeys.iter().any(|(old, _)| *old == consensus_key);
            if listed {
                found_listed += 1;
            }
            let should_disable = listed || (plan.disable_all_others && !rekeyed);

            if should_disable {
                match state {
                    ValidatorState::Disabled => {
                        tracing::info!(name = %validator.name, %identity_key, "already disabled");
                    }
                    ValidatorState::Tombstoned => {
                        tracing::info!(name = %validator.name, %identity_key, "tombstoned; nothing to do");
                    }
                    _ => {
                        let (old, new) = delta.restart_disable_validator(identity_key).await?;
                        tracing::info!(
                            name = %validator.name,
                            %identity_key,
                            ?old,
                            ?new,
                            power,
                            "validator removed from consensus"
                        );
                        disabled_keys.push(consensus_key);
                    }
                }
                if matches!(state, ValidatorState::Active) {
                    // It had real voting power; make sure InitChain never sees it.
                    if !disabled_keys.contains(&consensus_key) {
                        disabled_keys.push(consensus_key);
                    }
                }
            } else if matches!(state, ValidatorState::Active) {
                remaining_power += power;
                remaining.push((validator.name.clone(), power));
            }
        }

        ensure!(
            found_listed == to_disable.len(),
            "only {found_listed} of the {} listed consensus keys were found in the consensus set; \
             this is not the state this migration was written for",
            to_disable.len()
        );

        for (old, new) in &rekeys {
            let identity_key = delta
                .lookup_identity_key_by_consensus_key(old)
                .await
                .with_context(|| format!("no validator with consensus key {old:?}"))?;
            tracing::warn!(%identity_key, "REHEARSAL: replacing consensus key");
            delta.restart_rekey_validator(&identity_key, *new).await?;
        }

        // The staking component reports every key in this record to CometBFT at
        // InitChain, including zero-power ones, and CometBFT panics on a genesis
        // validator with zero power. Rewrite it to the validators that keep power.
        let current = delta.restart_get_consensus_keys().await?;
        let before = current.consensus_keys.len();
        let consensus_keys: Vec<PublicKey> = current
            .consensus_keys
            .into_iter()
            .filter(|k| !disabled_keys.contains(k))
            .map(|k| {
                rekeys
                    .iter()
                    .find(|(old, _)| *old == k)
                    .map(|(_, new)| *new)
                    .unwrap_or(k)
            })
            .collect();
        ensure!(
            !consensus_keys.is_empty(),
            "no validators would remain in the consensus set"
        );
        tracing::info!(
            before,
            after = consensus_keys.len(),
            "rewrote the consensus keys reported to CometBFT"
        );
        delta.restart_put_consensus_keys(CurrentConsensusKeys { consensus_keys });

        remaining.sort_by(|a, b| b.1.cmp(&a.1));
        for (name, power) in &remaining {
            tracing::info!(
                name = %name,
                power,
                share = format!("{:.2}%", 100.0 * *power as f64 / remaining_power as f64),
                "validator keeps voting power"
            );
        }
        tracing::info!(
            validators = remaining.len(),
            total_power = remaining_power,
            "post-restart active set"
        );
        if remaining.is_empty() {
            bail!("no active validators would remain");
        }
        Ok(())
    }
}
