//! Zcash threshold signing via RedPallas FROST.
//!
//! Produces RedPallas signatures for Zcash Orchard spend authorization.
//! Uses `reddsa::frost::redpallas` which wraps `frost-rerandomized` for
//! the Pallas curve with the correct Zcash challenge hash (ZIP 312).
//!
//! The FROST round functions (`commit`, `sign`, `aggregate`) are accessed
//! through reddsa's re-exports. The resulting signatures are standard
//! RedPallas — indistinguishable from single-signer signatures on Zcash.

use anyhow::{anyhow, Result};
use ed25519_consensus::{SigningKey, Signature, VerificationKey};

/// Authenticated message wrapper for FROST round data.
///
/// Each follower signs their round payload with ed25519 to prevent
/// impersonation. The coordinator verifies before processing.
#[derive(Clone, Debug)]
pub struct AuthenticatedPayload {
    pub round_tag: &'static [u8],
    pub payload: Vec<u8>,
    pub vk: VerificationKey,
    pub sig: Signature,
}

impl AuthenticatedPayload {
    pub fn make(sk: &SigningKey, round_tag: &'static [u8], payload: &[u8]) -> Self {
        let mut data = Vec::from(round_tag);
        data.extend_from_slice(payload);
        Self {
            round_tag,
            payload: payload.to_vec(),
            vk: sk.verification_key(),
            sig: sk.sign(&data),
        }
    }

    pub fn verify(&self) -> Result<()> {
        let mut data = Vec::from(self.round_tag);
        data.extend_from_slice(&self.payload);
        self.vk.verify(&self.sig, &data)
            .map_err(|_| anyhow!("invalid authenticated payload signature"))
    }
}

pub type FollowerRound1 = AuthenticatedPayload;
pub type FollowerRound2 = AuthenticatedPayload;

pub const ROUND1_TAG: &[u8] = b"zcash-frost-r1\0";
pub const ROUND2_TAG: &[u8] = b"zcash-frost-r2\0";

/// Aggregated RedPallas signatures for a Zcash transaction.
#[derive(Clone, Debug)]
pub struct ZcashAuthorizationData {
    /// One RedPallas signature per Orchard action (64 bytes each).
    pub spend_auths: Vec<[u8; 64]>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;
    use std::collections::{BTreeMap, HashMap};
    use reddsa::frost::redpallas::{self, keys, round1, round2, Identifier};

    #[test]
    fn test_end_to_end_deal_commit_sign_aggregate_verify() {
        // 1. Trusted dealer generates 2-of-3 shares
        let (shares, pubkeys) = keys::generate_with_dealer(
            3, 2,
            keys::IdentifierList::Default,
            OsRng,
        ).expect("dealer keygen should succeed");

        let identifiers: Vec<Identifier> = shares.keys().cloned().collect();
        let message = b"settle poker hand #42: A=600 B=400";

        // 2. Round 1: first 2 participants commit
        let mut nonces_map = BTreeMap::new();
        let mut commitments_map = BTreeMap::new();

        for &id in &identifiers[..2] {
            let secret_share = &shares[&id];
            let key_package: keys::KeyPackage = secret_share.clone().try_into()
                .expect("secret share to key package");
            let (nonces, commitments) = round1::commit(
                key_package.secret_share(),
                &mut OsRng,
            );
            nonces_map.insert(id, (nonces, key_package));
            commitments_map.insert(id, commitments);
        }

        // 3. Build signing package
        let signing_package = redpallas::SigningPackage::new(
            commitments_map,
            message.as_slice(),
        );

        // 4. Generate randomizer (shared between signers and aggregator)
        let randomized_params = frost_rerandomized::RandomizedParams::new(
            &pubkeys,
            &mut OsRng,
        );
        let randomizer_point = randomized_params.randomizer_point().clone();

        // 5. Round 2: first 2 participants sign with the shared randomizer
        let mut shares_map = BTreeMap::new();
        for &id in &identifiers[..2] {
            let (nonces, key_package) = nonces_map.remove(&id).unwrap();
            let share = round2::sign(
                &signing_package,
                &nonces,
                &key_package,
                &randomizer_point,
            ).expect("round2 sign should succeed");
            shares_map.insert(id, share);
        }

        // 6. Aggregate (reddsa::aggregate takes HashMap, not BTreeMap)
        let shares_hashmap: HashMap<Identifier, round2::SignatureShare> =
            shares_map.into_iter().collect();
        let group_signature = redpallas::aggregate(
            &signing_package,
            &shares_hashmap,
            &pubkeys,
            &randomized_params,
        ).expect("aggregate should succeed");

        // 7. Verify as standard RedPallas (Zcash interop)
        // Rerandomized signatures verify against the RANDOMIZED public key,
        // not the original group key. This is how Orchard works — each action
        // has its own randomized vk derived from (group_vk, randomizer).
        let sig_bytes: [u8; 64] = group_signature.serialize().as_ref().try_into().unwrap();
        let sig = reddsa::Signature::<reddsa::orchard::SpendAuth>::from(sig_bytes);

        let randomized_vk = randomized_params.randomized_group_public_key();
        let rpk_bytes: [u8; 32] = randomized_vk.serialize().as_ref().try_into().unwrap();
        let rpk = reddsa::VerificationKey::<reddsa::orchard::SpendAuth>::try_from(
            reddsa::VerificationKeyBytes::from(rpk_bytes)
        ).expect("randomized pubkey should be valid");

        rpk.verify(message, &sig).expect("FROST signature should verify as RedPallas");

        println!("2-of-3 RedPallas FROST: deal → commit → sign → aggregate → verify ✓");
    }

    #[test]
    fn test_authenticated_payload() {
        let sk = SigningKey::new(OsRng);
        let msg = AuthenticatedPayload::make(&sk, ROUND1_TAG, b"test data");
        assert!(msg.verify().is_ok());

        let mut tampered = msg.clone();
        tampered.payload = b"tampered".to_vec();
        assert!(tampered.verify().is_err());
    }
}
