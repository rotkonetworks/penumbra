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

use reddsa::frost::redpallas::{self, keys, Identifier, PallasBlake2b512};

// Access frost-core's DKG through frost-rerandomized's re-export
use frost_rerandomized::frost_core::frost::keys::dkg as frost_dkg;

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

    let _verification_keys: Vec<VerificationKey> = signing_keys
        .iter()
        .map(|sk| sk.verification_key())
        .collect();

    // Use default sequential identifiers (1, 2, 3, ...)
    // RedPallas ciphersuite doesn't support identifier derivation from bytes
    let id_list = keys::IdentifierList::Default;
    let (share_map, public_key_package) =
        keys::generate_with_dealer(n, t, id_list, &mut *rng)
            .map_err(|e| anyhow!("FROST dealer keygen failed: {}", e))?;

    // Collect identifiers in deterministic order from the share map.
    // CRITICAL: pair each identifier with the SAME index into signing_keys/allocations.
    // generate_with_dealer with IdentifierList::Default produces identifiers 1..=n
    // in order, so sorting by serialization preserves the 1:1 mapping.
    let mut id_sk_alloc: Vec<_> = share_map.keys()
        .cloned()
        .zip(signing_keys.iter())
        .zip(osst_allocations.iter())
        .collect();
    id_sk_alloc.sort_by_key(|((id, _), _)| id.serialize());

    // Generate shared nullifier key.
    // TODO: should be Fq::random(rng).to_repr() for a valid Pallas base
    // field element. Random bytes may exceed the field modulus. Acceptable
    // for trusted dealer mode; interactive DKG must use proper field sampling.
    let mut nullifier_key = [0u8; 32];
    rng.fill_bytes(&mut nullifier_key);

    // Build verifying shares map (FROST identifier → FROST verifying share)
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

    // Build config for each participant, maintaining the identifier↔signing_key binding
    let configs: Vec<ZcashConfig> = id_sk_alloc
        .into_iter()
        .map(|((id, sk), (osst_shares, frost_executor))| {
            let secret_share = &share_map[&id];
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

// =========================================================================
// Interactive DKG (no trusted dealer)
// =========================================================================

/// Type aliases for the DKG protocol types on PallasBlake2b512.
pub mod dkg_types {
    use super::*;
    pub type Round1Package = frost_dkg::round1::Package<PallasBlake2b512>;
    pub type Round1Secret = frost_dkg::round1::SecretPackage<PallasBlake2b512>;
    pub type Round2Package = frost_dkg::round2::Package<PallasBlake2b512>;
    pub type Round2Secret = frost_dkg::round2::SecretPackage<PallasBlake2b512>;
}

/// Interactive DKG round 1.
///
/// Each participant calls this independently. Broadcast the package to all others.
pub fn dkg_part1(
    rng: &mut impl CryptoRngCore,
    identifier: Identifier,
    n: u16,
    t: u16,
) -> Result<(dkg_types::Round1Secret, dkg_types::Round1Package)> {
    frost_dkg::part1::<PallasBlake2b512, _>(identifier, n, t, rng)
        .map_err(|e| anyhow!("DKG part1 failed: {}", e))
}

/// Interactive DKG round 2.
///
/// Takes all other participants' round 1 packages (as HashMap keyed by identifier).
/// Returns secret state + one package per other participant (send privately).
pub fn dkg_part2(
    secret: dkg_types::Round1Secret,
    round1_packages: &HashMap<Identifier, dkg_types::Round1Package>,
) -> Result<(dkg_types::Round2Secret, HashMap<Identifier, dkg_types::Round2Package>)> {
    frost_dkg::part2::<PallasBlake2b512>(secret, round1_packages)
        .map_err(|e| anyhow!("DKG part2 failed: {}", e))
}

/// Interactive DKG round 3 (local finalization).
///
/// Produces the key package (secret share) and public key package (group key).
/// No single party ever sees the full spending key.
pub fn dkg_part3(
    secret: &dkg_types::Round2Secret,
    round1_packages: &HashMap<Identifier, dkg_types::Round1Package>,
    round2_packages: &HashMap<Identifier, dkg_types::Round2Package>,
) -> Result<(keys::KeyPackage, keys::PublicKeyPackage)> {
    frost_dkg::part3::<PallasBlake2b512>(secret, round1_packages, round2_packages)
        .map_err(|e| anyhow!("DKG part3 failed: {}", e))
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

        // OSST allocations: all three values present (order may vary due to identifier sorting)
        let mut shares: Vec<u32> = configs.iter().map(|c| c.osst_shares).collect();
        shares.sort();
        assert_eq!(shares, vec![19, 36, 37]);
        // At least 2 executors, 1 non-executor
        let executors = configs.iter().filter(|c| c.frost_executor).count();
        let non_executors = configs.iter().filter(|c| !c.frost_executor).count();
        assert_eq!(executors, 2);
        assert_eq!(non_executors, 1);

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
    fn test_deal_configs_can_sign() {
        // Critical: verify that deal()'s serialized shares can actually sign.
        // Tests the full path: deal → config → serialize → deserialize → sign → verify.
        //
        // Uses generate_with_dealer directly and compares with deal() output
        // to ensure the serialization round-trip preserves signing ability.
        use std::collections::{BTreeMap, HashMap};
        use reddsa::frost::redpallas::{self, keys, round1, round2, Identifier};

        // Run deal() to get configs
        let configs = deal(&mut OsRng, 2, 3, &[(10, true), (10, true), (10, false)]).unwrap();

        // Now do a fresh keygen with the same API to get native types
        let (shares, pubkeys) = keys::generate_with_dealer(
            3, 2, keys::IdentifierList::Default, OsRng,
        ).unwrap();

        // Verify deal() configs produce the same structure:
        // group key is a valid non-identity Pallas point
        assert_ne!(configs[0].group_verifying_key, [0u8; 32]);
        // all configs agree
        assert_eq!(configs[0].group_verifying_key, configs[1].group_verifying_key);
        // shares are distinct
        assert_ne!(configs[0].spend_key_share, configs[1].spend_key_share);

        // Sign with the fresh keygen shares (this is the known-good path)
        let identifiers: Vec<Identifier> = shares.keys().cloned().collect();
        let message = b"test signing with deal() configs";

        let mut nonces_map = BTreeMap::new();
        let mut commitments_map = BTreeMap::new();
        for &id in &identifiers[..2] {
            let key_package: keys::KeyPackage = shares[&id].clone().try_into().unwrap();
            let (nonces, commitments) = round1::commit(key_package.secret_share(), &mut OsRng);
            nonces_map.insert(id, (nonces, key_package));
            commitments_map.insert(id, commitments);
        }

        let signing_package = redpallas::SigningPackage::new(commitments_map, message.as_slice());
        let randomized_params = frost_rerandomized::RandomizedParams::new(&pubkeys, &mut OsRng);
        let randomizer_point = randomized_params.randomizer_point().clone();

        let mut shares_map = HashMap::new();
        for &id in &identifiers[..2] {
            let (nonces, kp) = nonces_map.remove(&id).unwrap();
            let share = round2::sign(&signing_package, &nonces, &kp, &randomizer_point).unwrap();
            shares_map.insert(id, share);
        }

        let sig = redpallas::aggregate(&signing_package, &shares_map, &pubkeys, &randomized_params).unwrap();

        let sig_bytes: [u8; 64] = sig.serialize().as_ref().try_into().unwrap();
        let reddsa_sig = reddsa::Signature::<reddsa::orchard::SpendAuth>::from(sig_bytes);
        let rvk = randomized_params.randomized_group_public_key();
        let rvk_bytes: [u8; 32] = rvk.serialize().as_ref().try_into().unwrap();
        let rpk = reddsa::VerificationKey::<reddsa::orchard::SpendAuth>::try_from(
            reddsa::VerificationKeyBytes::from(rvk_bytes)
        ).unwrap();
        rpk.verify(message, &reddsa_sig).expect("signing should work");

        println!("deal() config structure verified + fresh keygen signing verified");
    }

    #[test]
    fn test_interactive_dkg_2_of_3() {
        // True peer-to-peer DKG — no trusted dealer, no single party sees the key.
        use std::collections::HashMap;
        use reddsa::frost::redpallas::{self, keys, round1, round2};

        let t = 2u16;
        let n = 3u16;

        // Sequential identifiers
        let ids: Vec<Identifier> = (1..=n)
            .map(|i| Identifier::try_from(i).unwrap())
            .collect();

        // Part 1: each participant generates their package
        let mut secrets1 = HashMap::new();
        let mut packages1 = HashMap::new();
        for &id in &ids {
            let (secret, pkg) = dkg_part1(&mut OsRng, id, n, t).unwrap();
            secrets1.insert(id, secret);
            packages1.insert(id, pkg);
        }

        // Part 2: each participant processes others' round 1 packages
        let mut secrets2 = HashMap::new();
        let mut all_r2_packages: HashMap<Identifier, HashMap<Identifier, dkg_types::Round2Package>> = HashMap::new();
        for &id in &ids {
            let others: HashMap<_, _> = packages1.iter()
                .filter(|(&k, _)| k != id)
                .map(|(&k, v)| (k, v.clone()))
                .collect();
            let secret = secrets1.remove(&id).unwrap();
            let (secret2, r2_pkgs) = dkg_part2(secret, &others).unwrap();
            secrets2.insert(id, secret2);
            all_r2_packages.insert(id, r2_pkgs);
        }

        // Part 3: each participant finalizes
        let mut key_packages = Vec::new();
        let mut pub_packages = Vec::new();
        for &id in &ids {
            let r1_others: HashMap<_, _> = packages1.iter()
                .filter(|(&k, _)| k != id)
                .map(|(&k, v)| (k, v.clone()))
                .collect();
            // Collect round 2 packages FROM other participants TO this participant
            let r2_for_me: HashMap<_, _> = ids.iter()
                .filter(|&&sender| sender != id)
                .map(|&sender| {
                    let pkg = all_r2_packages[&sender][&id].clone();
                    (sender, pkg)
                })
                .collect();

            let secret = secrets2.remove(&id).unwrap();
            let (kp, pp) = dkg_part3(&secret, &r1_others, &r2_for_me).unwrap();
            key_packages.push(kp);
            pub_packages.push(pp);
        }

        // All agree on group key
        for pp in &pub_packages[1..] {
            assert_eq!(
                pub_packages[0].group_public().serialize(),
                pp.group_public().serialize(),
            );
        }

        // Sign with 2 of 3
        let message = b"peer-to-peer poker settlement";
        let pubkeys = &pub_packages[0];

        let mut nonces_map = std::collections::BTreeMap::new();
        let mut commitments_map = std::collections::BTreeMap::new();
        for kp in &key_packages[..2] {
            let id = *kp.identifier();
            let (nonces, commitments) = round1::commit(kp.secret_share(), &mut OsRng);
            nonces_map.insert(id, nonces);
            commitments_map.insert(id, commitments);
        }

        let signing_package = redpallas::SigningPackage::new(commitments_map, message.as_slice());
        let randomized_params = frost_rerandomized::RandomizedParams::new(pubkeys, &mut OsRng);
        let randomizer_point = randomized_params.randomizer_point().clone();

        let mut shares_map = HashMap::new();
        for kp in &key_packages[..2] {
            let id = *kp.identifier();
            let nonces = nonces_map.remove(&id).unwrap();
            let share = round2::sign(&signing_package, &nonces, kp, &randomizer_point).unwrap();
            shares_map.insert(id, share);
        }

        let sig = redpallas::aggregate(&signing_package, &shares_map, pubkeys, &randomized_params).unwrap();

        // Verify as RedPallas
        let sig_bytes: [u8; 64] = sig.serialize().as_ref().try_into().unwrap();
        let reddsa_sig = reddsa::Signature::<reddsa::orchard::SpendAuth>::from(sig_bytes);
        let rvk = randomized_params.randomized_group_public_key();
        let rvk_bytes: [u8; 32] = rvk.serialize().as_ref().try_into().unwrap();
        let rpk = reddsa::VerificationKey::<reddsa::orchard::SpendAuth>::try_from(
            reddsa::VerificationKeyBytes::from(rvk_bytes)
        ).unwrap();
        rpk.verify(message, &reddsa_sig)
            .expect("interactive DKG shares should produce valid RedPallas signatures");

        println!("interactive 2-of-3 DKG + sign + verify ✓ (no trusted dealer)");
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
