//! Zcash Orchard threshold custody via RedPallas FROST.
//!
//! This module enables Penumbra validators to collectively custody ZEC.
//! The design mirrors `threshold/` but targets the Pallas curve for
//! Zcash Orchard spend authorization (RedPallas, ZIP 312).
//!
//! # Architecture: OSST authorization + FROST execution
//!
//! Two layers separate authorization from execution:
//!
//! - **OSST (all validators, stake-weighted)**: non-interactive threshold
//!   identification proves that 2/3 of stake approves an action. O(n)
//!   communication, scales to hundreds of validators.
//!
//! - **FROST (top validators by stake)**: small committee holds RedPallas
//!   signing shares. Produces actual Zcash transaction signatures. O(k²)
//!   communication where k = committee size (~5-10).
//!
//! FROST executors SHOULD only sign when presented with a valid OSST proof.
//! This is enforced at the protocol level (followers verify OSST before
//! signing) but NOT at the Zcash level (Zcash accepts any valid RedPallas
//! signature). Economic security (slashing) prevents FROST-only theft.
//!
//! # Key derivation
//!
//! Zcash Orchard spending requires both:
//! - `ask` (spend authorization key): signs transactions via RedPallas
//! - `nk` (nullifier key): computes nullifiers to prevent double-spends
//!
//! During DKG, participants commit to nullifier shares (same pattern as
//! Penumbra's threshold custody). The shared `nk` enables all validators
//! to scan for incoming notes, while spending requires FROST cooperation.
//!
//! # Share allocation
//!
//! 200 OSST shares distributed proportional to stake:
//! - Every validator gets ≥1 share (no exclusion)
//! - Top ~5 validators form the FROST execution committee
//! - Threshold: 134/200 (2/3 stake) for OSST, 2-of-5 for FROST
//!
//! # Security model
//!
//! Two layers with different guarantees:
//!
//! - **OSST**: cryptographic. Cannot forge 2/3 stake authorization.
//!   Prevents unauthorized transactions at the protocol level.
//!
//! - **FROST**: economic. The 2-of-5 committee CAN produce valid Zcash
//!   signatures directly (bypassing OSST on the Zcash side). Security
//!   comes from slashing: colluders lose more stake than they can steal.
//!
//! Additional protections:
//! - Nonce in OSST payload prevents replay
//! - Epoch-bound FROST shares prevent use after reshare
//! - Slashing: unauthorized spends are publicly attributable, colluders
//!   lose their staked collateral (held in FROST addresses by peers)
//!
//! # References
//!
//! - ZIP 312: Orchard Spend Authorization Multisignatures
//! - FROST: Flexible Round-Optimized Schnorr Threshold (RFC 9591)
//! - OSST: One-Step Schnorr Threshold Identification (Mergoupis-Anagnou)

pub mod config;
pub mod dkg;
pub mod sign;

pub use config::ZcashConfig;
