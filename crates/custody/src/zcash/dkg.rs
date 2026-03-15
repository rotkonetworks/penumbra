//! Key generation for Zcash Orchard custody.
//!
//! Provides both trusted-dealer and (future) interactive DKG modes.
//!
//! # Trusted dealer mode
//!
//! One party generates all shares and distributes them. Simpler but
//! requires trusting the dealer to delete the secret after splitting.
//! Acceptable for initial deployment, validator-operated bridge, or
//! when the dealer is a secure enclave / ceremony.
//!
//! # Interactive DKG (TODO)
//!
//! reddsa 0.5.x only exposes the trusted dealer API. When the DKG
//! module is exposed (or via frost-rerandomized directly), this will
//! be upgraded to full interactive DKG with nullifier commitments,
//! matching Penumbra's threshold::dkg protocol.
//!
//! # Nullifier key
//!
//! The nullifier key (nk) is generated separately and shared with all
//! participants. In trusted dealer mode, the dealer generates nk. In
//! interactive DKG mode, each participant commits to a nullifier share
//! in round 1 and opens in round 2 (same as Penumbra's approach).

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use ed25519_consensus::{SigningKey, VerificationKey};
use rand_core::CryptoRngCore;

use reddsa::frost::redpallas::{self, keys, Identifier};

use super::config::{ZcashConfig, AccountabilityMode};

/// Generate configs for all participants using a trusted dealer.
///
/// This is the analog of `threshold::Config::deal`. The dealer knows the
/// full secret (ask + nk) and splits it. In production, use interactive DKG.
pub fn deal(
    rng: &mut impl CryptoRngCore,
    t: u16,
    n: u16,
    osst_allocations: &[(u32, bool)], // (osst_shares, frost_executor) per participant
) -> Result<Vec<ZcashConfig>> {
    if osst_allocations.len() != n as usize {
        anyhow::bail!("osst_allocations length must match n");
    }

    // Generate ed25519 identity keys for each participant
    let signing_keys: Vec<SigningKey> = (0..n)
        .map(|_| SigningKey::new(&mut *rng))
        .collect();

    let verification_keys: Vec<VerificationKey> = signing_keys
        .iter()
        .map(|sk| sk.verification_key())
        .collect();

    // Use default sequential identifiers (1, 2, 3, ...)
    // RedPallas ciphersuite doesn't support identifier derivation from bytes
    let id_list = keys::IdentifierList::Default;
    let (share_map, public_key_package) =
        keys::generate_with_dealer(n, t, id_list, &mut *rng)
            .map_err(|e| anyhow!("FROST dealer keygen failed: {}", e))?;

    // Collect identifiers in order from the share map
    let mut identifiers: Vec<Identifier> = share_map.keys().cloned().collect();
    identifiers.sort_by_key(|id| id.serialize());

    // Generate shared nullifier key (random, known to all participants)
    let mut nullifier_key = [0u8; 32];
    rng.fill_bytes(&mut nullifier_key);

    // Build verifying shares map
    let verifying_shares: HashMap<String, String> = public_key_package
        .signer_pubkeys()
        .iter()
        .map(|(id, share)| {
            (
                hex::encode(id.serialize()),
                hex::encode(share.serialize()),
            )
        })
        .collect();

    // Build config for each participant
    let configs: Vec<ZcashConfig> = identifiers
        .iter()
        .zip(signing_keys.iter())
        .zip(osst_allocations.iter())
        .map(|((id, sk), (osst_shares, frost_executor))| {
            let secret_share = &share_map[id];
            // Convert SecretShare to KeyPackage for the signing share
            let key_package: keys::KeyPackage = secret_share.clone().try_into()
                .expect("secret share to key package conversion should not fail");

            ZcashConfig {
                threshold: t,
                group_verifying_key: public_key_package.group_public().serialize(),
                spend_key_share: key_package.secret_share().serialize(),
                nullifier_key,
                signing_key: sk.as_bytes().to_owned(),
                verifying_shares: verifying_shares.clone(),
                osst_shares: *osst_shares,
                frost_executor: *frost_executor,
                accountability: AccountabilityMode::Private,
            }
        })
        .collect();

    Ok(configs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    #[test]
    fn test_deal_2_of_3() {
        let allocations = vec![
            (37, true),  // iqlusion-like
            (36, true),  // informal-like
            (19, false), // rotko-like
        ];

        let configs = deal(&mut OsRng, 2, 3, &allocations).unwrap();

        assert_eq!(configs.len(), 3);

        // All agree on group key
        assert_eq!(configs[0].group_verifying_key, configs[1].group_verifying_key);
        assert_eq!(configs[1].group_verifying_key, configs[2].group_verifying_key);

        // All agree on nullifier
        assert_eq!(configs[0].nullifier_key, configs[1].nullifier_key);
        assert_eq!(configs[1].nullifier_key, configs[2].nullifier_key);

        // Different signing shares
        assert_ne!(configs[0].spend_key_share, configs[1].spend_key_share);
        assert_ne!(configs[1].spend_key_share, configs[2].spend_key_share);

        // Threshold = 2
        assert_eq!(configs[0].threshold, 2);

        // OSST allocations correct
        assert_eq!(configs[0].osst_shares, 37);
        assert!(configs[0].frost_executor);
        assert!(!configs[2].frost_executor);

        // Group key is non-zero (valid Pallas point)
        assert_ne!(configs[0].group_verifying_key, [0u8; 32]);

        println!("deal complete:");
        println!("  group key: {}", hex::encode(&configs[0].group_verifying_key));
        println!("  nullifier: {}", hex::encode(&configs[0].nullifier_key));
        println!("  threshold: 2-of-3");
        println!("  shares: {} distinct", configs.len());
    }

    #[test]
    fn test_deal_4_of_5_penumbra_committee() {
        // Simulates the FROST committee allocation for Penumbra validators
        let allocations = vec![
            (47, true),  // top validator
            (31, true),  // second
            (17, true),  // third
            (17, true),  // fourth
            (11, true),  // fifth
        ];

        let configs = deal(&mut OsRng, 4, 5, &allocations).unwrap();

        assert_eq!(configs.len(), 5);
        assert_eq!(configs[0].threshold, 4);

        // All agree on group key
        for c in &configs[1..] {
            assert_eq!(configs[0].group_verifying_key, c.group_verifying_key);
            assert_eq!(configs[0].nullifier_key, c.nullifier_key);
        }

        // All are executors
        for c in &configs {
            assert!(c.frost_executor);
        }

        println!("4-of-5 committee deal complete");
        println!("  group key: {}", hex::encode(&configs[0].group_verifying_key));
    }

    #[test]
    fn test_config_serialization_after_deal() {
        let configs = deal(&mut OsRng, 2, 2, &[(10, true), (10, true)]).unwrap();

        let json = serde_json::to_string(&configs[0]).unwrap();
        let recovered: ZcashConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(configs[0].group_verifying_key, recovered.group_verifying_key);
        assert_eq!(configs[0].threshold, recovered.threshold);
        assert_eq!(configs[0].nullifier_key, recovered.nullifier_key);
    }
}
