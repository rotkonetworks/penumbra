//! Zcash threshold signing via RedPallas FROST.
//!
//! This mirrors `threshold::sign` but produces RedPallas signatures
//! (Zcash Orchard spend authorization) instead of decaf377-rdsa.
//!
//! # Protocol (2 rounds, same as Penumbra threshold signing)
//!
//! Coordinator → all followers: "sign this Zcash transaction"
//!   + OSST authorization proof (2/3 stake approved this action)
//!
//! Round 1: followers → coordinator:
//!   - Nonce commitments (one per Orchard action to sign)
//!   - ed25519 signature over commitments (authentication)
//!
//! Round 2: coordinator → followers (all commitments), followers → coordinator:
//!   - Signature shares (one per action)
//!   - ed25519 signature over shares (authentication)
//!
//! Coordinator aggregates → produces RedPallas signatures for each action.
//!
//! # OSST gate
//!
//! The coordinator MUST include a valid OSST authorization proof in the
//! signing request. Followers MUST verify this proof before participating.
//! This prevents the FROST committee from signing unauthorized transactions.
//!
//! Without the OSST proof:
//! - Followers refuse to sign (enforcement at protocol level)
//! - Even if all executors collude, no valid OSST proof = no signature
//!
//! # Differences from threshold::sign
//!
//! - Signs Zcash Orchard actions (not Penumbra spend/delegator_vote/lqt_vote)
//! - Uses RedPallas (randomized Schnorr on Pallas) via `reddsa::frost`
//! - Requires OSST authorization proof in the signing request
//! - Signing request contains a Zcash PCZT (not a Penumbra TransactionPlan)

use std::collections::HashMap;

use ed25519_consensus::{SigningKey, Signature, VerificationKey};

/// A request to sign a Zcash transaction.
///
/// The coordinator sends this to initiate signing.
/// It includes the OSST authorization proof so followers can verify
/// that the action was approved by the required stake threshold.
#[derive(Clone, Debug)]
pub struct ZcashSigningRequest {
    /// The Zcash transaction data to sign (PCZT or raw action bytes).
    /// Each entry is one Orchard action requiring a RedPallas signature.
    pub actions_to_sign: Vec<Vec<u8>>,

    /// OSST authorization proof (serialized).
    /// This proves that 2/3 of stake approved this specific transaction.
    /// Followers MUST verify this before producing signature shares.
    pub osst_proof: Vec<u8>,

    /// Hash of the OSST payload (for binding the proof to the request).
    pub osst_payload_hash: [u8; 32],
}

/// Round 1 response from a follower.
#[derive(Clone, Debug)]
pub struct FollowerRound1 {
    /// One nonce commitment per action to sign.
    pub commitments: Vec<Vec<u8>>,
    /// Follower's identity.
    pub vk: VerificationKey,
    /// Signature over the commitments.
    pub sig: Signature,
}

impl FollowerRound1 {
    pub fn make(sk: &SigningKey, commitments: Vec<Vec<u8>>) -> Self {
        let data = Self::signing_data(&commitments);
        Self {
            commitments,
            vk: sk.verification_key(),
            sig: sk.sign(&data),
        }
    }

    pub fn verify(&self) -> Result<(), &'static str> {
        let data = Self::signing_data(&self.commitments);
        self.vk.verify(&self.sig, &data).map_err(|_| "invalid follower round1 signature")
    }

    fn signing_data(commitments: &[Vec<u8>]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(b"zcash-sign-r1\0");
        for c in commitments {
            data.extend_from_slice(&(c.len() as u32).to_le_bytes());
            data.extend_from_slice(c);
        }
        data
    }
}

/// Round 2 response from a follower.
#[derive(Clone, Debug)]
pub struct FollowerRound2 {
    /// One signature share per action.
    pub shares: Vec<Vec<u8>>,
    /// Follower's identity.
    pub vk: VerificationKey,
    /// Signature over the shares.
    pub sig: Signature,
}

impl FollowerRound2 {
    pub fn make(sk: &SigningKey, shares: Vec<Vec<u8>>) -> Self {
        let data = Self::signing_data(&shares);
        Self {
            shares,
            vk: sk.verification_key(),
            sig: sk.sign(&data),
        }
    }

    pub fn verify(&self) -> Result<(), &'static str> {
        let data = Self::signing_data(&self.shares);
        self.vk.verify(&self.sig, &data).map_err(|_| "invalid follower round2 signature")
    }

    fn signing_data(shares: &[Vec<u8>]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(b"zcash-sign-r2\0");
        for s in shares {
            data.extend_from_slice(&(s.len() as u32).to_le_bytes());
            data.extend_from_slice(s);
        }
        data
    }
}

/// Aggregated RedPallas signatures for a Zcash transaction.
///
/// Each entry corresponds to one Orchard action.
/// These are standard RedPallas signatures — the Zcash network
/// cannot distinguish them from single-signer signatures.
#[derive(Clone, Debug)]
pub struct ZcashAuthorizationData {
    /// One RedPallas signature per Orchard action (serialized).
    pub spend_auths: Vec<[u8; 64]>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    #[test]
    fn test_follower_round1_auth() {
        let sk = SigningKey::new(OsRng);
        let commitments = vec![vec![1, 2, 3], vec![4, 5, 6]];
        let msg = FollowerRound1::make(&sk, commitments);
        assert!(msg.verify().is_ok());
    }

    #[test]
    fn test_follower_round1_tampered() {
        let sk = SigningKey::new(OsRng);
        let commitments = vec![vec![1, 2, 3]];
        let mut msg = FollowerRound1::make(&sk, commitments);
        msg.commitments[0] = vec![9, 9, 9]; // tamper
        assert!(msg.verify().is_err());
    }

    #[test]
    fn test_follower_round2_auth() {
        let sk = SigningKey::new(OsRng);
        let shares = vec![vec![0xAA; 32], vec![0xBB; 32]];
        let msg = FollowerRound2::make(&sk, shares);
        assert!(msg.verify().is_ok());
    }

    #[test]
    fn test_follower_round2_tampered() {
        let sk = SigningKey::new(OsRng);
        let shares = vec![vec![0xAA; 32]];
        let mut msg = FollowerRound2::make(&sk, shares);
        msg.shares[0] = vec![0; 32]; // tamper
        assert!(msg.verify().is_err());
    }
}
