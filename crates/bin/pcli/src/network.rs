use anyhow::Context;
use decaf377_rdsa::{Signature, SpendAuth};
use futures::{FutureExt, TryStreamExt};
use penumbra_sdk_governance::ValidatorVoteBody;
use penumbra_sdk_proto::{
    custody::v1::{AuthorizeValidatorDefinitionRequest, AuthorizeValidatorVoteRequest},
    util::tendermint_proxy::v1::tendermint_proxy_service_client::TendermintProxyServiceClient,
    view::v1::broadcast_transaction_response::Status as BroadcastStatus,
    DomainType, Message,
};
use penumbra_sdk_stake::validator::Validator;
use penumbra_sdk_transaction::{txhash::TransactionId, Transaction, TransactionPlan, WitnessData};
use penumbra_sdk_view::{ViewClient, ViewServer};
use serde::{Deserialize, Serialize};
use std::{fs, future::Future, path::PathBuf};
use tonic::transport::Channel;
use tracing::instrument;

use crate::App;

/// Bundle of data needed for offline signing.
/// Can be transferred via QR code (small transactions) or file (large transactions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfflineBundle {
    /// The transaction plan to sign
    pub plan: TransactionPlan,
    /// Witness data (merkle proofs for note positions)
    pub witness: WitnessData,
    /// Effect hash (what actually gets signed), hex-encoded
    pub effect_hash: String,
    /// Spend randomizers, hex-encoded
    pub spend_randomizers: Vec<String>,
    /// Vote randomizers, hex-encoded
    pub vote_randomizers: Vec<String>,
}

impl OfflineBundle {
    /// Save bundle to a JSON file for transfer via wormhole/USB/etc
    pub fn save(&self, path: &PathBuf) -> anyhow::Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        fs::write(path, json)?;
        Ok(())
    }

    /// Load bundle from JSON file
    pub fn load(path: &PathBuf) -> anyhow::Result<Self> {
        let bytes = fs::read(path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

impl App {
    pub async fn build_and_submit_transaction(
        &mut self,
        plan: TransactionPlan,
    ) -> anyhow::Result<TransactionId> {
        let asset_cache = self.view().assets().await?;
        println!(
            "including transaction fee of {}...",
            plan.transaction_parameters.fee.0.format(&asset_cache)
        );

        // If QR export mode, export the plan for airgap signing instead of signing locally
        if self.qr_export_mode {
            return self.export_plan_as_qr(plan).await;
        }

        let transaction = self.build_transaction(plan).await?;
        self.submit_transaction(transaction).await
    }

    /// Export a transaction plan for airgap signing.
    /// Creates an OfflineBundle with all data needed for signing.
    async fn export_plan_as_qr(&mut self, plan: TransactionPlan) -> anyhow::Result<TransactionId> {
        use crate::qr;
        use penumbra_sdk_transaction::ActionPlan;
        use penumbra_sdk_view::ViewClient;

        println!("preparing offline transaction...");

        // Get witness data from view service (requires network)
        let witness_data = self.view().witness(&plan).await?;

        // Compute effect hash (this is what gets signed)
        let effect_hash = plan.effect_hash(&self.config.full_viewing_key)?;
        let effect_hash_bytes: [u8; 64] = *effect_hash.as_bytes();

        // Extract randomizers from actions
        let mut spend_randomizers: Vec<String> = Vec::new();
        let mut vote_randomizers: Vec<String> = Vec::new();

        for action in &plan.actions {
            match action {
                ActionPlan::Spend(spend_plan) => {
                    spend_randomizers.push(hex::encode(spend_plan.randomizer.to_bytes()));
                }
                ActionPlan::DelegatorVote(vote_plan) => {
                    vote_randomizers.push(hex::encode(vote_plan.randomizer.to_bytes()));
                }
                _ => {}
            }
        }

        // Create the offline bundle
        let bundle = OfflineBundle {
            plan: plan.clone(),
            witness: witness_data,
            effect_hash: hex::encode(&effect_hash_bytes),
            spend_randomizers: spend_randomizers.clone(),
            vote_randomizers: vote_randomizers.clone(),
        };

        // Save bundle to JSON file
        let bundle_path = PathBuf::from("pending_tx.bundle.json");
        bundle.save(&bundle_path)?;

        // Display transaction summary
        self.display_transaction_plan(&plan).await?;

        // Build QR payload
        let plan_proto: penumbra_sdk_proto::core::transaction::v1::TransactionPlan = plan.clone().into();
        let plan_bytes = plan_proto.encode_to_vec();

        let prelude = [0x53u8, 0x03, 0x10];
        let metadata = Self::encode_asset_metadata(&plan);

        let mut payload = Vec::new();
        payload.extend_from_slice(&prelude);
        payload.extend_from_slice(&metadata);
        payload.extend_from_slice(&(plan_bytes.len() as u32).to_le_bytes());
        payload.extend_from_slice(&plan_bytes);
        payload.extend_from_slice(&effect_hash_bytes);

        payload.extend_from_slice(&(spend_randomizers.len() as u16).to_le_bytes());
        for r in &spend_randomizers {
            payload.extend_from_slice(&hex::decode(r)?);
        }
        payload.extend_from_slice(&(vote_randomizers.len() as u16).to_le_bytes());
        for r in &vote_randomizers {
            payload.extend_from_slice(&hex::decode(r)?);
        }

        // Show QR for small transactions, suggest file transfer for large ones
        const QR_SIZE_THRESHOLD: usize = 2048;

        println!();
        if payload.len() <= QR_SIZE_THRESHOLD {
            println!("scan this QR code with your offline signer:");
            println!();
            qr::display_qr_terminal(&payload)?;
        } else {
            println!("transaction too large for QR ({} bytes)", payload.len());
            println!("transfer via: wormhole send {}", bundle_path.display());
        }

        // Clear next steps
        println!();
        println!("next steps:");
        println!("  1. sign on your offline device");
        println!("  2. save authorization to: pending_tx.bundle.auth.bin");
        println!("  3. run: pcli tx --qr-complete {}", bundle_path.display());

        Ok(TransactionId::default())
    }

    /// Encode asset denomination metadata for QR display
    /// Format: [count: 1 byte][for each: length: 1 byte, utf8 string]
    fn encode_asset_metadata(plan: &TransactionPlan) -> Vec<u8> {
        use std::collections::HashSet;

        let mut denoms = HashSet::new();

        // Extract denominations from spend/output actions
        for action in &plan.actions {
            match action {
                penumbra_sdk_transaction::ActionPlan::Spend(_) => {
                    // Add "penumbra" as default since we can't resolve asset IDs here
                    denoms.insert("penumbra".to_string());
                }
                penumbra_sdk_transaction::ActionPlan::Output(_) => {
                    denoms.insert("penumbra".to_string());
                }
                _ => {}
            }
        }

        // Fallback
        if denoms.is_empty() {
            denoms.insert("penumbra".to_string());
        }

        // Encode: [count][len1][str1][len2][str2]...
        let denoms_vec: Vec<_> = denoms.into_iter().collect();
        let mut result = Vec::new();
        result.push(denoms_vec.len() as u8);

        for denom in denoms_vec {
            let bytes = denom.as_bytes();
            result.push(bytes.len() as u8);
            result.extend_from_slice(bytes);
        }

        result
    }

    /// Display human-readable transaction plan for verification before signing
    async fn display_transaction_plan(&mut self, plan: &TransactionPlan) -> anyhow::Result<()> {
        use penumbra_sdk_transaction::ActionPlan;

        // Safe string truncation helper
        fn truncate(s: &str, max_len: usize) -> String {
            if s.len() <= max_len {
                s.to_string()
            } else if max_len <= 6 {
                s.chars().take(max_len).collect()
            } else {
                let half = (max_len - 3) / 2;
                let start: String = s.chars().take(half).collect();
                let end: String = s.chars().skip(s.len().saturating_sub(half)).collect();
                format!("{}...{}", start, end)
            }
        }

        let asset_cache = self.view().assets().await?;
        let fee = &plan.transaction_parameters.fee;

        println!("fee: {}", fee.0.format(&asset_cache));
        println!("actions:");

        for (i, action) in plan.actions.iter().enumerate() {
            let desc = match action {
                ActionPlan::Spend(spend) => {
                    format!("spend {}", spend.note.value().format(&asset_cache))
                }
                ActionPlan::Output(output) => {
                    let addr = output.dest_address.to_string();
                    format!("send {} to {}", output.value.format(&asset_cache), truncate(&addr, 20))
                }
                ActionPlan::Swap(swap) => {
                    let input = penumbra_sdk_asset::Value {
                        amount: swap.swap_plaintext.delta_1_i,
                        asset_id: swap.swap_plaintext.trading_pair.asset_1(),
                    };
                    let target = asset_cache
                        .get(&swap.swap_plaintext.trading_pair.asset_2())
                        .map(|m| m.symbol().to_string())
                        .unwrap_or_else(|| "?".to_string());
                    format!("swap {} for {}", input.format(&asset_cache), target)
                }
                ActionPlan::SwapClaim(_) => "claim swap".to_string(),
                ActionPlan::Delegate(d) => {
                    let id = d.validator_identity.to_string();
                    format!("delegate {} to {}", d.unbonded_amount, truncate(&id, 16))
                }
                ActionPlan::Undelegate(u) => {
                    let id = u.validator_identity.to_string();
                    format!("undelegate {} from {}", u.delegation_amount, truncate(&id, 16))
                }
                ActionPlan::UndelegateClaim(c) => {
                    format!("claim undelegation of {}", c.unbonding_amount)
                }
                ActionPlan::ValidatorDefinition(vd) => {
                    format!("define validator {}", vd.validator.name)
                }
                ActionPlan::DelegatorVote(v) => {
                    let vote = match v.vote {
                        penumbra_sdk_governance::Vote::Yes => "yes",
                        penumbra_sdk_governance::Vote::No => "no",
                        penumbra_sdk_governance::Vote::Abstain => "abstain",
                    };
                    format!("vote {} on proposal {}", vote, v.proposal)
                }
                ActionPlan::ProposalSubmit(p) => {
                    format!("submit proposal: {}", p.proposal.title)
                }
                ActionPlan::ProposalWithdraw(w) => {
                    format!("withdraw proposal {}", w.proposal)
                }
                ActionPlan::Ics20Withdrawal(w) => {
                    let dest = truncate(&w.destination_chain_address, 16);
                    format!("ibc send {} {} to {}", w.amount, w.denom.symbol(), dest)
                }
                ActionPlan::PositionOpen(p) => {
                    let a1 = asset_cache.get(&p.position.phi.pair.asset_1())
                        .map(|m| m.symbol().to_string()).unwrap_or_else(|| "?".to_string());
                    let a2 = asset_cache.get(&p.position.phi.pair.asset_2())
                        .map(|m| m.symbol().to_string()).unwrap_or_else(|| "?".to_string());
                    format!("open lp {}/{}", a1, a2)
                }
                ActionPlan::PositionClose(p) => {
                    let id = p.position_id.to_string();
                    format!("close lp {}", truncate(&id, 16))
                }
                ActionPlan::PositionWithdraw(p) => {
                    let id = p.position_id.to_string();
                    format!("withdraw lp {}", truncate(&id, 16))
                }
                ActionPlan::ActionDutchAuctionSchedule(a) => {
                    let out = asset_cache.get(&a.description.output_id)
                        .map(|m| m.symbol()).unwrap_or("?");
                    format!("auction {} for {}", a.description.input.format(&asset_cache), out)
                }
                ActionPlan::ActionDutchAuctionEnd(_) => "end auction".to_string(),
                ActionPlan::ActionDutchAuctionWithdraw(_) => "withdraw auction".to_string(),
                ActionPlan::ActionLiquidityTournamentVote(l) => {
                    format!("lqt vote for {}", l.incentivized.denom)
                }
                ActionPlan::CommunityPoolSpend(_) => "community pool spend".to_string(),
                ActionPlan::CommunityPoolOutput(_) => "community pool output".to_string(),
                ActionPlan::CommunityPoolDeposit(d) => {
                    format!("deposit {} to community pool", d.value.format(&asset_cache))
                }
                _ => format!("{:?}", std::mem::discriminant(action)),
            };
            println!("  {}. {}", i + 1, desc);
        }

        Ok(())
    }

    pub fn build_transaction(
        &mut self,
        plan: TransactionPlan,
    ) -> impl Future<Output = anyhow::Result<Transaction>> + '_ {
        println!(
            "building transaction [{} actions, {} proofs]...",
            plan.actions.len(),
            plan.num_proofs(),
        );
        let start = std::time::Instant::now();
        let tx = penumbra_sdk_wallet::build_transaction(
            &self.config.full_viewing_key,
            self.view.as_mut().expect("view service initialized"),
            &mut self.custody,
            plan,
        );
        async move {
            let tx = tx.await?;
            let elapsed = start.elapsed();
            println!(
                "finished proving in {}.{:03} seconds [{} actions, {} proofs, {} bytes]",
                elapsed.as_secs(),
                elapsed.subsec_millis(),
                tx.actions().count(),
                tx.num_proofs(),
                tx.encode_to_vec().len()
            );
            Ok(tx)
        }
    }

    pub async fn sign_validator_definition(
        &mut self,
        validator_definition: Validator,
    ) -> anyhow::Result<Signature<SpendAuth>> {
        let request = AuthorizeValidatorDefinitionRequest {
            validator_definition: Some(validator_definition.into()),
            pre_authorizations: vec![],
        };
        self.custody
            .authorize_validator_definition(request)
            .await?
            .into_inner()
            .validator_definition_auth
            .ok_or_else(|| anyhow::anyhow!("missing validator definition auth"))?
            .try_into()
    }

    pub async fn sign_validator_vote(
        &mut self,
        validator_vote: ValidatorVoteBody,
    ) -> anyhow::Result<Signature<SpendAuth>> {
        let request = AuthorizeValidatorVoteRequest {
            validator_vote: Some(validator_vote.into()),
            pre_authorizations: vec![],
        };
        // Use the separate governance custody service, if one is configured, to sign the validator
        // vote. This allows the governance custody service to have a different key than the main
        // custody, which is useful for validators who want to have a separate key for voting.
        self.governance_custody // VERY IMPORTANT: use governance custody here!
            .authorize_validator_vote(request)
            .await?
            .into_inner()
            .validator_vote_auth
            .ok_or_else(|| anyhow::anyhow!("missing validator vote auth"))?
            .try_into()
    }

    /// Submits a transaction to the network.
    pub async fn submit_transaction(
        &mut self,
        transaction: Transaction,
    ) -> anyhow::Result<TransactionId> {
        if let Some(file) = &self.save_transaction_here_instead {
            println!(
                "saving transaction to disk, path: {}",
                file.to_string_lossy()
            );
            fs::write(file, &serde_json::to_vec(&transaction)?)?;
            return Ok(transaction.id());
        }

        println!("broadcasting transaction and awaiting confirmation...");
        let mut rsp = self.view().broadcast_transaction(transaction, true).await?;

        let id = async move {
            while let Some(rsp) = rsp.try_next().await? {
                match rsp.status {
                    Some(status) => match status {
                        BroadcastStatus::BroadcastSuccess(bs) => {
                            println!(
                                "transaction broadcast successfully: {}",
                                TransactionId::try_from(
                                    bs.id.expect("detected transaction missing id")
                                )?
                            );
                        }
                        BroadcastStatus::Confirmed(c) => {
                            let id = c.id.expect("detected transaction missing id").try_into()?;
                            if c.detection_height != 0 {
                                println!(
                                    "transaction confirmed and detected: {} @ height {}",
                                    id, c.detection_height
                                );
                            } else {
                                println!("transaction confirmed and detected: {}", id);
                            }
                            return Ok(id);
                        }
                    },
                    None => {
                        // No status is unexpected behavior
                        return Err(anyhow::anyhow!(
                            "empty BroadcastTransactionResponse message"
                        ));
                    }
                }
            }

            Err(anyhow::anyhow!(
                "should have received BroadcastTransaction status or error"
            ))
        }
        .boxed()
        .await
        .context("error broadcasting transaction")?;

        Ok(id)
    }

    /// Submits a transaction to the network, returning `Ok` as soon as the
    /// transaction has been submitted, rather than waiting for confirmation.
    #[instrument(skip(self, transaction))]
    pub async fn submit_transaction_unconfirmed(
        &mut self,
        transaction: Transaction,
    ) -> anyhow::Result<()> {
        println!("broadcasting transaction without confirmation...");
        self.view()
            .broadcast_transaction(transaction, false)
            .await?;

        Ok(())
    }

    /// Convenience method for obtaining a `tonic::Channel` for the remote
    /// `pd` endpoint, as configured for `pcli`.
    pub async fn pd_channel(&self) -> anyhow::Result<Channel> {
        ViewServer::get_pd_channel(self.config.grpc_url.clone())
            .await
            .context(format!("could not connect to {}", self.config.grpc_url))
    }

    pub async fn tendermint_proxy_client(
        &self,
    ) -> anyhow::Result<TendermintProxyServiceClient<Channel>> {
        let channel = self.pd_channel().await?;
        Ok(TendermintProxyServiceClient::new(channel))
    }
}
