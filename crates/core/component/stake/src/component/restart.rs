//! State surgery helpers for coordinated chain restarts.
//!
//! These are **not** used during normal consensus. They exist so that an
//! out-of-band migration (`pd migrate restart-mainnet-1`) can remove
//! validators that are not participating in consensus from the CometBFT
//! validator set, using the same state transitions that the staking component
//! itself performs, and so that a private rehearsal of such a restart can
//! swap consensus keys without touching the real operators' key material.

use anyhow::{Context, Result};
use async_trait::async_trait;
use cnidarium::StateWrite;
use penumbra_sdk_proto::{StateReadProto, StateWriteProto};
use tendermint::PublicKey;

use crate::{
    component::{
        validator_handler::{ValidatorDataRead, ValidatorManager},
        StateWriteExt as _,
    },
    state_key,
    validator::State,
    CurrentConsensusKeys, IdentityKey,
};

#[async_trait]
pub trait RestartStateWrite: StateWrite {
    /// Force a validator out of the consensus set, exactly as if its operator
    /// had uploaded a definition with `enabled = false`.
    ///
    /// A `Disabled` validator is never re-selected at an epoch boundary: the
    /// operator must upload a fresh definition with `enabled = true` to come
    /// back. An `Active` validator's delegation pool starts unbonding.
    ///
    /// Returns the `(old_state, new_state)` pair.
    async fn restart_disable_validator(
        &mut self,
        identity_key: &IdentityKey,
    ) -> Result<(State, State)> {
        self.set_validator_state(identity_key, State::Disabled)
            .await
            .with_context(|| format!("disabling validator {identity_key}"))
    }

    /// Replace a validator's consensus key. **Testing only**: this is used to
    /// rehearse a restart on a private copy of the chain state with freshly
    /// generated keys, so that the rehearsal can never sign for mainnet.
    async fn restart_rekey_validator(
        &mut self,
        identity_key: &IdentityKey,
        new_consensus_key: PublicKey,
    ) -> Result<()> {
        let mut validator = self
            .get_validator_definition(identity_key)
            .await?
            .with_context(|| format!("validator {identity_key} has no definition"))?;
        validator.consensus_key = new_consensus_key;
        self.register_consensus_key(identity_key, &new_consensus_key);
        self.put(
            state_key::validators::definitions::by_id(identity_key),
            validator,
        );
        Ok(())
    }

    /// The set of consensus keys the staking component believes CometBFT
    /// currently knows about. At `InitChain` after a migration, every key in
    /// this set is reported back to CometBFT, including zero-power ones, which
    /// CometBFT rejects in a genesis validator set.
    async fn restart_get_consensus_keys(&self) -> Result<CurrentConsensusKeys> {
        self.get(state_key::consensus_update::consensus_keys())
            .await?
            .context("current consensus keys must be present")
    }

    /// Overwrite the recorded set of consensus keys known to CometBFT.
    fn restart_put_consensus_keys(&mut self, keys: CurrentConsensusKeys) {
        self.put(
            state_key::consensus_update::consensus_keys().to_owned(),
            keys,
        );
    }
}

impl<T: StateWrite + ?Sized> RestartStateWrite for T {}
