//! Distributed key generation for Zcash Orchard custody.
//!
//! This mirrors `threshold::dkg` but targets the Pallas curve.
//!
//! # Protocol (3 rounds)
//!
//! Round 1: Each participant broadcasts:
//!   - FROST DKG round1 package (commitment to polynomial + proof of knowledge)
//!   - Nullifier share commitment (blake2b hash of their nk share)
//!   - Ephemeral encryption public key (for round 2 encrypted packages)
//!   - ed25519 verification key (identity)
//!
//! Round 2: Each participant sends (encrypted, signed):
//!   - FROST DKG round2 package to each other participant
//!   - Opened nullifier share (verified against round 1 commitment)
//!   - ed25519 signature over the entire message
//!
//! Round 3 (local): Each participant:
//!   - Decrypts their round 2 packages
//!   - Verifies nullifier commitments
//!   - Derives their signing share + group verifying key
//!   - Sums nullifier shares to get shared nk
//!   - Produces ZcashConfig
//!
//! # Security properties
//!
//! - No single participant learns the spending key (ask) or can sign alone
//! - Nullifier key (nk) is derived collectively — committed in round 1,
//!   opened in round 2, summed in round 3
//! - ed25519 signatures on all messages prevent forgery and impersonation
//! - Encrypted round 2 packages prevent eavesdropping on shares
//! - Commitment scheme prevents equivocation on nullifier shares
//!
//! # Differences from threshold::dkg
//!
//! - Uses Pallas curve (not decaf377) — for Zcash Orchard compatibility
//! - Nullifier is a Pallas base field element (not Fq from decaf377)
//! - Group key is a Pallas point (derives Orchard address, not Penumbra address)
//! - Would use `reddsa` crate with `frost` feature for RedPallas FROST
//!
//! # TODO
//!
//! This module defines the types and protocol structure. The actual
//! cryptographic operations require integrating the `reddsa` crate
//! with its `frost` feature, which provides:
//! - `reddsa::frost::keys::dkg` — DKG round functions
//! - `reddsa::frost::round1`, `round2` — signing round functions
//! - `reddsa::frost::aggregate` — signature aggregation
//!
//! The `reddsa` crate uses `frost-rerandomized` internally, which
//! handles the randomization needed for Orchard spend authorization.

use std::collections::HashMap;
use ed25519_consensus::{SigningKey, VerificationKey};

/// Commitment to a nullifier share, preventing equivocation.
///
/// Created in round 1, verified in round 3 against the opened value.
/// Uses blake2b with domain separator to bind the commitment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NullifierCommitment([u8; 32]);

impl NullifierCommitment {
    /// Create a commitment to a nullifier share.
    pub fn create(share: &[u8; 32]) -> Self {
        let hash = blake2b_simd::Params::new()
            .personal(b"zcash-nk-commit\0")
            .hash(share);
        let mut out = [0u8; 32];
        out.copy_from_slice(&hash.as_bytes()[..32]);
        Self(out)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Round 1 message broadcast to all participants.
#[derive(Clone, Debug)]
pub struct Round1 {
    /// FROST DKG round 1 package (serialized).
    pub frost_package: Vec<u8>,
    /// Commitment to our nullifier share.
    pub nullifier_commitment: NullifierCommitment,
    /// Ephemeral encryption key for receiving round 2 packages.
    pub epk: [u8; 32],
    /// Our ed25519 verification key (identity).
    pub vk: VerificationKey,
}

/// Round 2 message sent to all participants (contains encrypted sub-shares).
#[derive(Clone, Debug)]
pub struct Round2 {
    /// For each other participant: encrypted FROST round 2 package.
    pub encrypted_packages: HashMap<[u8; 32], Vec<u8>>,
    /// Our opened nullifier share (verified against round 1 commitment).
    pub nullifier_share: [u8; 32],
    /// Our ed25519 verification key.
    pub vk: VerificationKey,
    /// Signature over (encrypted_packages || nullifier_share).
    pub sig: [u8; 64],
}

impl Round2 {
    /// Create a signed round 2 message.
    pub fn make(
        sk: &SigningKey,
        encrypted_packages: HashMap<[u8; 32], Vec<u8>>,
        nullifier_share: [u8; 32],
    ) -> Self {
        let sig_data = Self::signing_data(&encrypted_packages, &nullifier_share);
        let sig = sk.sign(&sig_data);
        Self {
            encrypted_packages,
            nullifier_share,
            vk: sk.verification_key(),
            sig: sig.to_bytes(),
        }
    }

    /// Verify the signature and extract the packages.
    pub fn verify_and_extract(
        &self,
    ) -> Result<(&HashMap<[u8; 32], Vec<u8>>, [u8; 32]), &'static str> {
        let sig_data = Self::signing_data(&self.encrypted_packages, &self.nullifier_share);
        let sig = ed25519_consensus::Signature::try_from(self.sig.as_slice())
            .map_err(|_| "invalid signature bytes")?;
        self.vk.verify(&sig, &sig_data).map_err(|_| "signature verification failed")?;
        Ok((&self.encrypted_packages, self.nullifier_share))
    }

    fn signing_data(
        packages: &HashMap<[u8; 32], Vec<u8>>,
        nullifier: &[u8; 32],
    ) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(b"zcash-dkg-round2\0");
        // Sort by key for deterministic encoding
        let mut sorted: Vec<_> = packages.iter().collect();
        sorted.sort_by_key(|(k, _)| *k);
        for (k, v) in sorted {
            data.extend_from_slice(k);
            data.extend_from_slice(&(v.len() as u32).to_le_bytes());
            data.extend_from_slice(v);
        }
        data.extend_from_slice(nullifier);
        data
    }
}

/// State retained after round 1 (secret, not transmitted).
pub struct Round1State {
    /// FROST's internal round 1 secret state (serialized).
    pub frost_secret: Vec<u8>,
    /// Our nullifier share (to open in round 2).
    pub nullifier_share: [u8; 32],
    /// Our ed25519 signing key.
    pub sk: SigningKey,
    /// Our ephemeral decryption key.
    pub edk: [u8; 32],
}

/// State retained after round 2 (secret, not transmitted).
pub struct Round2State {
    /// FROST's internal round 2 secret state (serialized).
    pub frost_secret: Vec<u8>,
    /// FROST round 1 packages from all participants.
    pub round1_packages: Vec<(VerificationKey, Vec<u8>)>,
    /// Map from vk → (nullifier commitment) for verification in round 3.
    pub nullifier_commitments: HashMap<[u8; 32], NullifierCommitment>,
    /// Our nullifier share.
    pub nullifier_share: [u8; 32],
    /// Our signing key.
    pub sk: SigningKey,
    /// Our decryption key.
    pub edk: [u8; 32],
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    #[test]
    fn test_nullifier_commitment() {
        let share = [42u8; 32];
        let commitment = NullifierCommitment::create(&share);
        let commitment2 = NullifierCommitment::create(&share);
        assert_eq!(commitment, commitment2);

        let different = NullifierCommitment::create(&[43u8; 32]);
        assert_ne!(commitment, different);
    }

    #[test]
    fn test_round2_signature_verification() {
        let sk = SigningKey::new(OsRng);
        let mut packages = HashMap::new();
        packages.insert([1u8; 32], vec![0xAA, 0xBB]);
        let nullifier = [99u8; 32];

        let round2 = Round2::make(&sk, packages, nullifier);

        // should verify
        let (_, opened_nk) = round2.verify_and_extract().unwrap();
        assert_eq!(opened_nk, nullifier);
    }

    #[test]
    fn test_round2_tampered_signature_fails() {
        let sk = SigningKey::new(OsRng);
        let mut packages = HashMap::new();
        packages.insert([1u8; 32], vec![0xAA]);
        let nullifier = [99u8; 32];

        let mut round2 = Round2::make(&sk, packages, nullifier);

        // tamper with nullifier
        round2.nullifier_share = [0u8; 32];

        // should fail
        assert!(round2.verify_and_extract().is_err());
    }
}
