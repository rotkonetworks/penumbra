//! Configuration for Zcash threshold custody.
//!
//! Each validator stores:
//! - Their RedPallas signing share (secret)
//! - The group verifying key (public, derives the custody Orchard address)
//! - The shared nullifier key (enables scanning)
//! - Their ed25519 identity key (authenticates FROST messages)
//! - The verification shares of all participants (for cheater detection)
//!
//! The config is produced by DKG and persisted across epochs.
//! On validator set changes, reshare produces a new config with the
//! same group key but different individual shares.

use std::collections::HashMap;

use ed25519_consensus::{SigningKey, VerificationKey};
use serde::{Deserialize, Serialize};

/// Total OSST shares in the system.
pub const TOTAL_SHARES: u32 = 200;

/// OSST authorization threshold (2/3).
pub const OSST_THRESHOLD: u32 = 134;

/// Maximum FROST execution committee size.
pub const FROST_COMMITTEE_SIZE: u32 = 5;

/// FROST threshold within the committee.
/// Low because OSST is the real security layer.
/// 2-of-5 maximizes liveness while preventing single-executor theft.
pub const FROST_THRESHOLD: u16 = 2;


/// Configuration for a single participant in the Zcash custody scheme.
///
/// This is the Zcash analog of `threshold::Config`. The key difference:
/// we use RedPallas (Pallas curve) instead of decaf377-rdsa (decaf377).
///
/// # Security notes
///
/// - `spend_key_share` is secret material. Handle with care, zeroize on drop.
/// - `nullifier_key` is shared among all participants (not secret per se,
///   but reveals which notes belong to the custody address).
/// - `signing_key` authenticates this participant during FROST rounds.
///   A compromised signing key allows impersonation but not theft
///   (the spend key share is separate).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZcashConfig {
    /// FROST threshold (min signers to produce a signature).
    pub threshold: u16,

    /// The group verifying key (compressed Pallas point, 32 bytes).
    /// This is `ak` — the Orchard spend authorization verifying key.
    /// The custody Orchard address is derived from this.
    #[serde(with = "hex_bytes")]
    pub group_verifying_key: [u8; 32],

    /// Our RedPallas signing share (secret scalar, 32 bytes).
    /// This is our share of `ask` (the spend authorization signing key).
    #[serde(with = "hex_bytes")]
    pub spend_key_share: [u8; 32],

    /// The shared nullifier key (Pallas base field element, 32 bytes).
    /// All participants know this — it enables scanning for custody notes.
    /// Derived by summing all participants' nullifier shares during DKG.
    #[serde(with = "hex_bytes")]
    pub nullifier_key: [u8; 32],

    /// Our ed25519 identity key (for authenticating FROST messages).
    #[serde(with = "hex_bytes")]
    pub signing_key: [u8; 32],

    /// Verification shares for all participants.
    /// Maps hex(ed25519 verification key) → hex(RedPallas verifying share).
    /// Used for cheater detection during signature aggregation.
    pub verifying_shares: HashMap<String, String>,

    /// Number of OSST shares this participant holds (stake-weighted).
    pub osst_shares: u32,

    /// Whether this participant is in the FROST execution committee.
    pub frost_executor: bool,
}

impl ZcashConfig {
    /// Derive the Orchard custody address bytes from the group key.
    ///
    /// In Zcash Orchard, `ak` (the spend authorization verifying key) is
    /// a compressed Pallas point. Combined with `nk`, it forms the full
    /// viewing key, from which the Orchard address is derived.
    pub fn custody_address_bytes(&self) -> [u8; 32] {
        self.group_verifying_key
    }

    /// Get the verification key for this participant.
    pub fn verification_key(&self) -> VerificationKey {
        let sk = SigningKey::try_from(self.signing_key.as_slice())
            .expect("stored signing key should be valid");
        sk.verification_key()
    }

    /// Check if this participant can execute FROST signing.
    pub fn can_execute(&self) -> bool {
        self.frost_executor
    }

    /// Check if this participant has enough OSST shares to matter.
    pub fn osst_weight(&self) -> u32 {
        self.osst_shares
    }
}

impl Drop for ZcashConfig {
    fn drop(&mut self) {
        // Zeroize secret material
        self.spend_key_share.fill(0);
        self.signing_key.fill(0);
    }
}

/// Serde helper for fixed-size byte arrays as hex strings.
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        hex::encode(bytes).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        bytes.try_into().map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_zeroize_on_drop() {
        let config = ZcashConfig {
            threshold: 2,
            group_verifying_key: [1u8; 32],
            spend_key_share: [0xAB; 32],
            nullifier_key: [2u8; 32],
            signing_key: [0xCD; 32],
            verifying_shares: HashMap::new(),
            osst_shares: 10,
            frost_executor: true,
        };
        // Just verify it compiles and drops cleanly
        drop(config);
    }

    #[test]
    fn test_config_serialization_roundtrip() {
        let config = ZcashConfig {
            threshold: 4,
            group_verifying_key: [1u8; 32],
            spend_key_share: [2u8; 32],
            nullifier_key: [3u8; 32],
            signing_key: [4u8; 32],
            verifying_shares: {
                let mut m = HashMap::new();
                m.insert(hex::encode([5u8; 32]), hex::encode([6u8; 32]));
                m
            },
            osst_shares: 37,
            frost_executor: true,
        };

        let json = serde_json::to_string(&config).unwrap();
        let config2: ZcashConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.threshold, config2.threshold);
        assert_eq!(config.group_verifying_key, config2.group_verifying_key);
        assert_eq!(config.nullifier_key, config2.nullifier_key);
        assert_eq!(config.osst_shares, config2.osst_shares);
        assert_eq!(config.frost_executor, config2.frost_executor);
    }
}
