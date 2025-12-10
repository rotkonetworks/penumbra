//! Schema generation for airgap signer compatibility
//!
//! Generates action schema that can be imported into airgap signers (like Zigner)
//! via QR code, enabling them to parse and display transaction details.
//!
//! Uses merkleized metadata (similar to Polkadot RFC-0078) for:
//! - Compact root hash storage (32 bytes vs ~10KB full schema)
//! - Transaction-specific proofs (only include proofs for used actions)
//! - Incremental updates (only changed action definitions need updating)

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Schema version - increment when format changes
pub const SCHEMA_VERSION: u32 = 1;

/// QR type for schema updates (0x12 in the tx_type field)
pub const SCHEMA_QR_TYPE: u8 = 0x12;

/// Penumbra crypto type in QR prelude
pub const PENUMBRA_CRYPTO_TYPE: u8 = 0x03;

/// Complete schema for Penumbra transaction parsing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PenumbraActionSchema {
    pub version: u32,
    pub chain_id: String,
    pub protocol_version: String,
    pub actions: BTreeMap<u32, ActionDefinition>,
    #[serde(default)]
    pub types: BTreeMap<String, TypeDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDefinition {
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub requires_signature: bool,
    #[serde(default)]
    pub fields: Vec<FieldDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDefinition {
    pub path: String,
    pub label: String,
    pub field_type: FieldType,
    #[serde(default = "default_true")]
    pub visible: bool,
    #[serde(default = "default_priority")]
    pub priority: u32,
}

fn default_true() -> bool { true }
fn default_priority() -> u32 { 100 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FieldType {
    String,
    Bool,
    U32,
    U64,
    Amount { decimals: u8 },
    AssetId,
    Address,
    IdentityKey,
    Bytes,
    Message { type_name: String },
    Enum { variants: Vec<(u32, String)> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeDefinition {
    pub name: String,
    pub fields: Vec<FieldDefinition>,
}

/// Generate schema for current Penumbra protocol
/// Field numbers from proto/penumbra/penumbra/core/transaction/v1/transaction.proto
pub fn generate_schema(chain_id: &str) -> PenumbraActionSchema {
    let mut schema = PenumbraActionSchema {
        version: SCHEMA_VERSION,
        chain_id: chain_id.to_string(),
        protocol_version: env!("CARGO_PKG_VERSION").to_string(),
        actions: BTreeMap::new(),
        types: BTreeMap::new(),
    };

    // Field 1: Spend
    schema.actions.insert(1, ActionDefinition {
        name: "Spend".to_string(),
        display_name: "Spend".to_string(),
        description: "Spend a note from your wallet".to_string(),
        requires_signature: true,
        fields: vec![
            FieldDefinition {
                path: "note.value.amount".to_string(),
                label: "Amount".to_string(),
                field_type: FieldType::Amount { decimals: 6 },
                visible: true,
                priority: 1,
            },
            FieldDefinition {
                path: "note.value.asset_id".to_string(),
                label: "Asset".to_string(),
                field_type: FieldType::AssetId,
                visible: true,
                priority: 2,
            },
        ],
    });

    // Field 2: Output
    schema.actions.insert(2, ActionDefinition {
        name: "Output".to_string(),
        display_name: "Output".to_string(),
        description: "Create an output note".to_string(),
        requires_signature: false,
        fields: vec![
            FieldDefinition {
                path: "value.amount".to_string(),
                label: "Amount".to_string(),
                field_type: FieldType::Amount { decimals: 6 },
                visible: true,
                priority: 1,
            },
            FieldDefinition {
                path: "value.asset_id".to_string(),
                label: "Asset".to_string(),
                field_type: FieldType::AssetId,
                visible: true,
                priority: 2,
            },
            FieldDefinition {
                path: "dest_address".to_string(),
                label: "To".to_string(),
                field_type: FieldType::Address,
                visible: true,
                priority: 3,
            },
        ],
    });

    // Field 3: Swap
    schema.actions.insert(3, ActionDefinition {
        name: "Swap".to_string(),
        display_name: "Swap".to_string(),
        description: "Swap assets via DEX".to_string(),
        requires_signature: false,
        fields: vec![
            FieldDefinition {
                path: "swap_plaintext.delta_1_i".to_string(),
                label: "Input Amount".to_string(),
                field_type: FieldType::Amount { decimals: 6 },
                visible: true,
                priority: 1,
            },
            FieldDefinition {
                path: "swap_plaintext.trading_pair.asset_1".to_string(),
                label: "From Asset".to_string(),
                field_type: FieldType::AssetId,
                visible: true,
                priority: 2,
            },
            FieldDefinition {
                path: "swap_plaintext.trading_pair.asset_2".to_string(),
                label: "To Asset".to_string(),
                field_type: FieldType::AssetId,
                visible: true,
                priority: 3,
            },
        ],
    });

    // Field 4: SwapClaim
    schema.actions.insert(4, ActionDefinition {
        name: "SwapClaim".to_string(),
        display_name: "Claim Swap".to_string(),
        description: "Claim outputs from a completed swap".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 16: ValidatorDefinition
    schema.actions.insert(16, ActionDefinition {
        name: "ValidatorDefinition".to_string(),
        display_name: "Validator Definition".to_string(),
        description: "Define or update validator".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 17: IbcRelay
    schema.actions.insert(17, ActionDefinition {
        name: "IbcRelay".to_string(),
        display_name: "IBC Relay".to_string(),
        description: "Relay IBC message".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 18: ProposalSubmit
    schema.actions.insert(18, ActionDefinition {
        name: "ProposalSubmit".to_string(),
        display_name: "Submit Proposal".to_string(),
        description: "Submit governance proposal".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 19: ProposalWithdraw
    schema.actions.insert(19, ActionDefinition {
        name: "ProposalWithdraw".to_string(),
        display_name: "Withdraw Proposal".to_string(),
        description: "Withdraw governance proposal".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 20: ValidatorVote
    schema.actions.insert(20, ActionDefinition {
        name: "ValidatorVote".to_string(),
        display_name: "Validator Vote".to_string(),
        description: "Vote on proposal as validator".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 21: DelegatorVote
    schema.actions.insert(21, ActionDefinition {
        name: "DelegatorVote".to_string(),
        display_name: "Delegator Vote".to_string(),
        description: "Vote on proposal as delegator".to_string(),
        requires_signature: true,
        fields: vec![
            FieldDefinition {
                path: "proposal".to_string(),
                label: "Proposal".to_string(),
                field_type: FieldType::U64,
                visible: true,
                priority: 1,
            },
            FieldDefinition {
                path: "vote.vote".to_string(),
                label: "Vote".to_string(),
                field_type: FieldType::Enum {
                    variants: vec![
                        (0, "Abstain".to_string()),
                        (1, "Yes".to_string()),
                        (2, "No".to_string()),
                    ],
                },
                visible: true,
                priority: 2,
            },
        ],
    });

    // Field 22: ProposalDepositClaim
    schema.actions.insert(22, ActionDefinition {
        name: "ProposalDepositClaim".to_string(),
        display_name: "Claim Proposal Deposit".to_string(),
        description: "Claim deposit from finished proposal".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 30: PositionOpen
    schema.actions.insert(30, ActionDefinition {
        name: "PositionOpen".to_string(),
        display_name: "Open LP Position".to_string(),
        description: "Open a liquidity position".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 31: PositionClose
    schema.actions.insert(31, ActionDefinition {
        name: "PositionClose".to_string(),
        display_name: "Close LP Position".to_string(),
        description: "Close a liquidity position".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 32: PositionWithdraw
    schema.actions.insert(32, ActionDefinition {
        name: "PositionWithdraw".to_string(),
        display_name: "Withdraw LP".to_string(),
        description: "Withdraw from closed position".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 40: Delegate
    schema.actions.insert(40, ActionDefinition {
        name: "Delegate".to_string(),
        display_name: "Delegate".to_string(),
        description: "Delegate stake to validator".to_string(),
        requires_signature: false,
        fields: vec![
            FieldDefinition {
                path: "unbonded_amount".to_string(),
                label: "Amount".to_string(),
                field_type: FieldType::Amount { decimals: 6 },
                visible: true,
                priority: 1,
            },
            FieldDefinition {
                path: "validator_identity".to_string(),
                label: "Validator".to_string(),
                field_type: FieldType::IdentityKey,
                visible: true,
                priority: 2,
            },
        ],
    });

    // Field 41: Undelegate
    schema.actions.insert(41, ActionDefinition {
        name: "Undelegate".to_string(),
        display_name: "Undelegate".to_string(),
        description: "Undelegate stake from validator".to_string(),
        requires_signature: false,
        fields: vec![
            FieldDefinition {
                path: "delegation_amount".to_string(),
                label: "Amount".to_string(),
                field_type: FieldType::Amount { decimals: 6 },
                visible: true,
                priority: 1,
            },
            FieldDefinition {
                path: "validator_identity".to_string(),
                label: "Validator".to_string(),
                field_type: FieldType::IdentityKey,
                visible: true,
                priority: 2,
            },
        ],
    });

    // Field 42: UndelegateClaim
    schema.actions.insert(42, ActionDefinition {
        name: "UndelegateClaim".to_string(),
        display_name: "Claim Undelegation".to_string(),
        description: "Claim unbonded stake".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 50: CommunityPoolSpend
    schema.actions.insert(50, ActionDefinition {
        name: "CommunityPoolSpend".to_string(),
        display_name: "Community Pool Spend".to_string(),
        description: "Spend from community pool".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 51: CommunityPoolOutput
    schema.actions.insert(51, ActionDefinition {
        name: "CommunityPoolOutput".to_string(),
        display_name: "Community Pool Output".to_string(),
        description: "Output to community pool".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 52: CommunityPoolDeposit
    schema.actions.insert(52, ActionDefinition {
        name: "CommunityPoolDeposit".to_string(),
        display_name: "Community Pool Deposit".to_string(),
        description: "Deposit to community pool".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 53: ActionDutchAuctionSchedule
    schema.actions.insert(53, ActionDefinition {
        name: "ActionDutchAuctionSchedule".to_string(),
        display_name: "Schedule Auction".to_string(),
        description: "Schedule a Dutch auction".to_string(),
        requires_signature: false,
        fields: vec![
            FieldDefinition {
                path: "description.input.amount".to_string(),
                label: "Input Amount".to_string(),
                field_type: FieldType::Amount { decimals: 6 },
                visible: true,
                priority: 1,
            },
            FieldDefinition {
                path: "description.input.asset_id".to_string(),
                label: "Selling".to_string(),
                field_type: FieldType::AssetId,
                visible: true,
                priority: 2,
            },
            FieldDefinition {
                path: "description.output_id".to_string(),
                label: "For Asset".to_string(),
                field_type: FieldType::AssetId,
                visible: true,
                priority: 3,
            },
        ],
    });

    // Field 54: ActionDutchAuctionEnd
    schema.actions.insert(54, ActionDefinition {
        name: "ActionDutchAuctionEnd".to_string(),
        display_name: "End Auction".to_string(),
        description: "End a Dutch auction early".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 55: ActionDutchAuctionWithdraw
    schema.actions.insert(55, ActionDefinition {
        name: "ActionDutchAuctionWithdraw".to_string(),
        display_name: "Withdraw Auction".to_string(),
        description: "Withdraw from ended auction".to_string(),
        requires_signature: false,
        fields: vec![],
    });

    // Field 70: ActionLiquidityTournamentVote
    schema.actions.insert(70, ActionDefinition {
        name: "ActionLiquidityTournamentVote".to_string(),
        display_name: "LQT Vote".to_string(),
        description: "Vote in liquidity tournament".to_string(),
        requires_signature: true,
        fields: vec![
            FieldDefinition {
                path: "body.incentivized_asset".to_string(),
                label: "Vote For".to_string(),
                field_type: FieldType::AssetId,
                visible: true,
                priority: 1,
            },
        ],
    });

    // Field 200: Ics20Withdrawal
    schema.actions.insert(200, ActionDefinition {
        name: "Ics20Withdrawal".to_string(),
        display_name: "IBC Transfer Out".to_string(),
        description: "Transfer assets via IBC".to_string(),
        requires_signature: false,
        fields: vec![
            FieldDefinition {
                path: "amount".to_string(),
                label: "Amount".to_string(),
                field_type: FieldType::Amount { decimals: 6 },
                visible: true,
                priority: 1,
            },
            FieldDefinition {
                path: "denom".to_string(),
                label: "Asset".to_string(),
                field_type: FieldType::String,
                visible: true,
                priority: 2,
            },
            FieldDefinition {
                path: "destination_chain_address".to_string(),
                label: "To".to_string(),
                field_type: FieldType::String,
                visible: true,
                priority: 3,
            },
        ],
    });

    schema
}

/// Encode schema as QR payload
/// Format: [0x53][0x03][0x12][version:4LE][checksum:32][schema_json]
pub fn encode_schema_qr(schema: &PenumbraActionSchema) -> Result<Vec<u8>> {
    let schema_json = serde_json::to_vec(schema)?;

    // Simple XOR checksum (should use blake2b in production)
    let mut checksum = [0u8; 32];
    for (i, byte) in schema_json.iter().enumerate() {
        checksum[i % 32] ^= byte;
    }

    let mut result = Vec::with_capacity(39 + schema_json.len());

    // Prelude
    result.push(0x53); // Substrate/Signer prefix
    result.push(PENUMBRA_CRYPTO_TYPE);
    result.push(SCHEMA_QR_TYPE);

    // Version (4 bytes LE)
    result.extend_from_slice(&schema.version.to_le_bytes());

    // Checksum (32 bytes)
    result.extend_from_slice(&checksum);

    // Schema JSON
    result.extend_from_slice(&schema_json);

    Ok(result)
}

/// Decode schema from QR payload (hex string)
/// Format: [0x53][0x03][0x12][version:4LE][checksum:32][schema_json]
pub fn decode_schema_qr(hex_str: &str) -> Result<PenumbraActionSchema> {
    // Decode hex to bytes
    let data: Vec<u8> = (0..hex_str.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("invalid hex: {}", e))?;

    // Verify prelude
    if data.len() < 39 {
        anyhow::bail!("payload too short: {} bytes (minimum 39)", data.len());
    }

    if data[0] != 0x53 {
        anyhow::bail!("invalid prefix: expected 0x53, got 0x{:02x}", data[0]);
    }

    if data[1] != PENUMBRA_CRYPTO_TYPE {
        anyhow::bail!(
            "invalid crypto type: expected 0x{:02x} (Penumbra), got 0x{:02x}",
            PENUMBRA_CRYPTO_TYPE,
            data[1]
        );
    }

    if data[2] != SCHEMA_QR_TYPE {
        anyhow::bail!(
            "invalid QR type: expected 0x{:02x} (Schema), got 0x{:02x}",
            SCHEMA_QR_TYPE,
            data[2]
        );
    }

    // Parse version
    let version = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);

    // Parse checksum (for verification)
    let stored_checksum = &data[7..39];

    // Parse schema JSON
    let schema_json = &data[39..];

    // Verify checksum
    let mut computed_checksum = [0u8; 32];
    for (i, byte) in schema_json.iter().enumerate() {
        computed_checksum[i % 32] ^= byte;
    }

    if stored_checksum != computed_checksum {
        anyhow::bail!("checksum mismatch - data may be corrupted");
    }

    // Parse JSON
    let schema: PenumbraActionSchema = serde_json::from_slice(schema_json)
        .map_err(|e| anyhow::anyhow!("failed to parse schema JSON: {}", e))?;

    // Verify version matches
    if schema.version != version {
        anyhow::bail!(
            "version mismatch: header says {}, schema says {}",
            version,
            schema.version
        );
    }

    Ok(schema)
}

/// Display schema information
pub fn display_schema(schema: &PenumbraActionSchema) {
    println!("Penumbra Action Schema");
    println!("======================");
    println!("Version:          {}", schema.version);
    println!("Chain ID:         {}", schema.chain_id);
    println!("Protocol Version: {}", schema.protocol_version);
    println!("Actions Defined:  {}", schema.actions.len());
    println!();

    // Sort actions by field number
    let mut actions: Vec<_> = schema.actions.iter().collect();
    actions.sort_by_key(|(k, _)| *k);

    println!("Actions:");
    println!("--------");
    for (field_num, action) in actions {
        let sig_marker = if action.requires_signature { "*" } else { " " };
        println!(
            "  {:>3}{} {} - {}",
            field_num, sig_marker, action.display_name, action.description
        );
        for field in &action.fields {
            println!("       - {}: {:?}", field.label, field.field_type);
        }
    }
    println!();
    println!("* = requires signature");
}

// ============================================================================
// Merkleized Metadata (RFC-0078 style)
// ============================================================================

/// Hash type for merkle tree (blake3)
pub type Hash = [u8; 32];

/// QR type for merkleized schema digest (0x13)
pub const MERKLE_SCHEMA_QR_TYPE: u8 = 0x13;

/// Compact schema digest - what Zigner actually stores
/// Only 32 bytes for the merkle root instead of ~10KB full schema
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchemaDigest {
    /// Version of the digest format
    pub version: u32,
    /// Chain identifier
    pub chain_id: String,
    /// Protocol version (e.g. "2.1.0")
    pub protocol_version: String,
    /// Merkle root of action definitions tree
    pub action_tree_root: Hash,
    /// Number of actions in the tree (for info)
    pub action_count: u32,
}

/// Merkle proof for a single action definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionProof {
    /// The action field number (position in protobuf oneof)
    pub field_number: u32,
    /// The action definition
    pub action: ActionDefinition,
    /// Merkle proof siblings (hashes needed to reconstruct root)
    pub proof: Vec<Hash>,
    /// Leaf index in the tree
    pub leaf_index: u32,
}

/// Transaction payload with proofs - sent alongside transaction QR
/// Only includes proofs for actions actually used in the transaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionWithProofs {
    /// The transaction plan bytes
    pub transaction_plan: Vec<u8>,
    /// Proofs for each action type used in this transaction
    pub action_proofs: Vec<ActionProof>,
}

/// Build a merkle tree from action definitions
/// Returns (root_hash, leaves) where leaves are sorted by field number
pub fn build_action_merkle_tree(schema: &PenumbraActionSchema) -> (Hash, Vec<(u32, Hash)>) {
    // Sort actions by field number for deterministic ordering
    let mut actions: Vec<_> = schema.actions.iter().collect();
    actions.sort_by_key(|(k, _)| *k);

    if actions.is_empty() {
        return ([0u8; 32], vec![]);
    }

    // Hash each action definition to create leaves
    let leaves: Vec<(u32, Hash)> = actions
        .iter()
        .map(|(field_num, action)| {
            let leaf_data = serde_json::to_vec(action).expect("action serializable");
            let hash = blake3_hash(&leaf_data);
            (**field_num, hash)
        })
        .collect();

    // Build tree from leaves
    let root = compute_merkle_root(&leaves.iter().map(|(_, h)| *h).collect::<Vec<_>>());

    (root, leaves)
}

/// Compute merkle root from leaf hashes
/// Uses complete binary merkle tree construction (RFC-0078 style)
fn compute_merkle_root(leaves: &[Hash]) -> Hash {
    if leaves.is_empty() {
        return [0u8; 32];
    }
    if leaves.len() == 1 {
        return leaves[0];
    }

    let mut nodes: Vec<Hash> = leaves.to_vec();

    // Pad to power of 2 for complete binary tree
    let next_pow2 = nodes.len().next_power_of_two();
    while nodes.len() < next_pow2 {
        nodes.push([0u8; 32]); // Empty leaf marker
    }

    // Build tree bottom-up
    while nodes.len() > 1 {
        let mut next_level = Vec::with_capacity(nodes.len() / 2);
        for chunk in nodes.chunks(2) {
            let combined = combine_hashes(&chunk[0], &chunk[1]);
            next_level.push(combined);
        }
        nodes = next_level;
    }

    nodes[0]
}

/// Combine two hashes for merkle tree node
fn combine_hashes(left: &Hash, right: &Hash) -> Hash {
    let mut data = Vec::with_capacity(64);
    data.extend_from_slice(left);
    data.extend_from_slice(right);
    blake3_hash(&data)
}

/// Simple blake3 hash (using the blake3 crate would be better in production)
/// For now, use a simple hash that's compatible with what we have
fn blake3_hash(data: &[u8]) -> Hash {
    // Use SHA256 for now (blake3 would need to be added as dependency)
    // In production, replace with actual blake3
    use sha2::{Sha256, Digest};
    let result = Sha256::digest(data);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&result);
    hash
}

/// Generate merkle proof for a specific action
pub fn generate_action_proof(
    schema: &PenumbraActionSchema,
    field_number: u32,
) -> Option<ActionProof> {
    let action = schema.actions.get(&field_number)?;

    // Get sorted actions
    let mut actions: Vec<_> = schema.actions.iter().collect();
    actions.sort_by_key(|(k, _)| *k);

    // Find leaf index
    let leaf_index = actions.iter().position(|(k, _)| **k == field_number)? as u32;

    // Build all leaves
    let leaves: Vec<Hash> = actions
        .iter()
        .map(|(_, action)| {
            let leaf_data = serde_json::to_vec(action).expect("action serializable");
            blake3_hash(&leaf_data)
        })
        .collect();

    // Pad to power of 2
    let mut padded_leaves = leaves.clone();
    let next_pow2 = padded_leaves.len().next_power_of_two();
    while padded_leaves.len() < next_pow2 {
        padded_leaves.push([0u8; 32]);
    }

    // Generate proof
    let proof = generate_merkle_proof(&padded_leaves, leaf_index as usize);

    Some(ActionProof {
        field_number,
        action: action.clone(),
        proof,
        leaf_index,
    })
}

/// Generate merkle proof for a leaf at given index
fn generate_merkle_proof(leaves: &[Hash], leaf_index: usize) -> Vec<Hash> {
    if leaves.len() <= 1 {
        return vec![];
    }

    let mut proof = Vec::new();
    let mut nodes = leaves.to_vec();
    let mut index = leaf_index;

    while nodes.len() > 1 {
        // Get sibling
        let sibling_index = if index % 2 == 0 { index + 1 } else { index - 1 };
        if sibling_index < nodes.len() {
            proof.push(nodes[sibling_index]);
        }

        // Move to next level
        let mut next_level = Vec::with_capacity(nodes.len() / 2);
        for chunk in nodes.chunks(2) {
            let combined = combine_hashes(&chunk[0], &chunk[1]);
            next_level.push(combined);
        }
        nodes = next_level;
        index /= 2;
    }

    proof
}

/// Verify a merkle proof
pub fn verify_action_proof(proof: &ActionProof, expected_root: &Hash) -> bool {
    // Hash the action definition
    let leaf_data = serde_json::to_vec(&proof.action).expect("action serializable");
    let mut current_hash = blake3_hash(&leaf_data);

    // Reconstruct path to root
    let mut index = proof.leaf_index as usize;
    for sibling in &proof.proof {
        current_hash = if index % 2 == 0 {
            combine_hashes(&current_hash, sibling)
        } else {
            combine_hashes(sibling, &current_hash)
        };
        index /= 2;
    }

    current_hash == *expected_root
}

/// Generate schema digest (compact representation)
pub fn generate_schema_digest(schema: &PenumbraActionSchema) -> SchemaDigest {
    let (root, _) = build_action_merkle_tree(schema);

    SchemaDigest {
        version: schema.version,
        chain_id: schema.chain_id.clone(),
        protocol_version: schema.protocol_version.clone(),
        action_tree_root: root,
        action_count: schema.actions.len() as u32,
    }
}

/// Encode schema digest as QR payload (compact binary format)
///
/// Format:
/// - 3 bytes: prelude (0x53, crypto_type, 0x13)
/// - 4 bytes: version (LE)
/// - 1 byte: chain_id length
/// - N bytes: chain_id
/// - 1 byte: protocol_version length
/// - M bytes: protocol_version
/// - 32 bytes: merkle root hash
/// - 4 bytes: action count (LE)
pub fn encode_digest_qr(digest: &SchemaDigest) -> Result<Vec<u8>> {
    let chain_bytes = digest.chain_id.as_bytes();
    let proto_bytes = digest.protocol_version.as_bytes();

    if chain_bytes.len() > 255 || proto_bytes.len() > 255 {
        anyhow::bail!("chain_id or protocol_version too long");
    }

    let mut result = Vec::with_capacity(3 + 4 + 1 + chain_bytes.len() + 1 + proto_bytes.len() + 32 + 4);

    // Prelude
    result.push(0x53);
    result.push(PENUMBRA_CRYPTO_TYPE);
    result.push(MERKLE_SCHEMA_QR_TYPE);

    // Version
    result.extend_from_slice(&digest.version.to_le_bytes());

    // Chain ID (length-prefixed)
    result.push(chain_bytes.len() as u8);
    result.extend_from_slice(chain_bytes);

    // Protocol version (length-prefixed)
    result.push(proto_bytes.len() as u8);
    result.extend_from_slice(proto_bytes);

    // Merkle root (32 bytes)
    result.extend_from_slice(&digest.action_tree_root);

    // Action count
    result.extend_from_slice(&digest.action_count.to_le_bytes());

    Ok(result)
}

/// Decode schema digest from QR payload (compact binary format)
pub fn decode_digest_qr(hex_str: &str) -> Result<SchemaDigest> {
    let data: Vec<u8> = (0..hex_str.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("invalid hex: {}", e))?;

    // Minimum size: prelude(3) + version(4) + chain_len(1) + chain(1) + proto_len(1) + proto(1) + hash(32) + count(4) = 47
    if data.len() < 47 {
        anyhow::bail!("payload too short");
    }

    if data[0] != 0x53 || data[1] != PENUMBRA_CRYPTO_TYPE || data[2] != MERKLE_SCHEMA_QR_TYPE {
        anyhow::bail!("invalid prelude for merkle digest");
    }

    let version = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);

    let chain_len = data[7] as usize;
    let chain_id = String::from_utf8(data[8..8 + chain_len].to_vec())
        .map_err(|e| anyhow::anyhow!("invalid chain_id: {}", e))?;

    let proto_len_idx = 8 + chain_len;
    let proto_len = data[proto_len_idx] as usize;
    let protocol_version = String::from_utf8(data[proto_len_idx + 1..proto_len_idx + 1 + proto_len].to_vec())
        .map_err(|e| anyhow::anyhow!("invalid protocol_version: {}", e))?;

    let hash_start = proto_len_idx + 1 + proto_len;
    if data.len() < hash_start + 32 + 4 {
        anyhow::bail!("payload too short for hash and count");
    }

    let mut action_tree_root = [0u8; 32];
    action_tree_root.copy_from_slice(&data[hash_start..hash_start + 32]);

    let count_start = hash_start + 32;
    let action_count = u32::from_le_bytes([
        data[count_start],
        data[count_start + 1],
        data[count_start + 2],
        data[count_start + 3],
    ]);

    Ok(SchemaDigest {
        version,
        chain_id,
        protocol_version,
        action_tree_root,
        action_count,
    })
}

/// Display schema digest info
pub fn display_digest(digest: &SchemaDigest) {
    println!("Penumbra Schema Digest (Merkleized)");
    println!("===================================");
    println!("Version:          {}", digest.version);
    println!("Chain ID:         {}", digest.chain_id);
    println!("Protocol Version: {}", digest.protocol_version);
    println!("Action Count:     {}", digest.action_count);
    println!("Merkle Root:      {}", hex::encode(digest.action_tree_root));
}

// ============================================================================
// Asset Registry (for human-readable token names)
// ============================================================================

/// QR type for asset registry digest (0x14)
pub const ASSET_REGISTRY_QR_TYPE: u8 = 0x14;

/// Asset entry in the registry
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssetEntry {
    /// The asset ID (32 bytes, derived from denom)
    #[serde(with = "hex_bytes")]
    pub asset_id: [u8; 32],
    /// Full denomination path (e.g., "upenumbra" or "transfer/channel-0/uusdc")
    pub denom: String,
    /// Display symbol (e.g., "UM", "USDC")
    pub symbol: String,
    /// Decimal places for display
    pub decimals: u8,
    /// Human-readable name (e.g., "Penumbra", "USD Coin")
    #[serde(default)]
    pub display_name: String,
}

/// Compact registry digest - what Zigner stores
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegistryDigest {
    /// Version of the digest format
    pub version: u32,
    /// Chain identifier
    pub chain_id: String,
    /// Merkle root of asset entries tree
    pub asset_tree_root: Hash,
    /// Number of assets in the registry
    pub asset_count: u32,
    /// Timestamp of registry snapshot
    #[serde(default)]
    pub timestamp: u64,
}

/// Merkle proof for a single asset
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetProof {
    /// The asset entry
    pub entry: AssetEntry,
    /// Merkle proof siblings
    pub proof: Vec<Hash>,
    /// Leaf index in the tree
    pub leaf_index: u32,
}

/// Combined transaction payload with all proofs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionPayload {
    /// The transaction plan bytes (protobuf)
    pub plan: Vec<u8>,
    /// Proofs for action types used
    pub action_proofs: Vec<ActionProof>,
    /// Proofs for assets used
    pub asset_proofs: Vec<AssetProof>,
}

/// Build asset registry from known assets
pub fn build_asset_registry(assets: &[AssetEntry]) -> (Hash, Vec<(Hash, [u8; 32])>) {
    if assets.is_empty() {
        return ([0u8; 32], vec![]);
    }

    // Sort by asset_id for deterministic ordering
    let mut sorted: Vec<_> = assets.iter().collect();
    sorted.sort_by_key(|a| a.asset_id);

    // Hash each entry to create leaves
    let leaves: Vec<(Hash, [u8; 32])> = sorted
        .iter()
        .map(|entry| {
            let leaf_data = serde_json::to_vec(entry).expect("entry serializable");
            let hash = blake3_hash(&leaf_data);
            (hash, entry.asset_id)
        })
        .collect();

    // Build tree
    let root = compute_merkle_root(&leaves.iter().map(|(h, _)| *h).collect::<Vec<_>>());

    (root, leaves)
}

/// Generate asset proof (for future merkle-proof transaction flow)
#[allow(dead_code)]
pub fn generate_asset_proof(assets: &[AssetEntry], asset_id: &[u8; 32]) -> Option<AssetProof> {
    let entry = assets.iter().find(|a| &a.asset_id == asset_id)?;

    // Sort for deterministic ordering
    let mut sorted: Vec<_> = assets.iter().collect();
    sorted.sort_by_key(|a| a.asset_id);

    let leaf_index = sorted.iter().position(|a| &a.asset_id == asset_id)? as u32;

    // Build leaves
    let leaves: Vec<Hash> = sorted
        .iter()
        .map(|e| {
            let leaf_data = serde_json::to_vec(e).expect("entry serializable");
            blake3_hash(&leaf_data)
        })
        .collect();

    // Pad to power of 2
    let mut padded = leaves.clone();
    let next_pow2 = padded.len().next_power_of_two();
    while padded.len() < next_pow2 {
        padded.push([0u8; 32]);
    }

    let proof = generate_merkle_proof(&padded, leaf_index as usize);

    Some(AssetProof {
        entry: entry.clone(),
        proof,
        leaf_index,
    })
}

/// Verify asset proof (for future merkle-proof transaction flow)
#[allow(dead_code)]
pub fn verify_asset_proof(proof: &AssetProof, expected_root: &Hash) -> bool {
    let leaf_data = serde_json::to_vec(&proof.entry).expect("entry serializable");
    let mut current_hash = blake3_hash(&leaf_data);

    let mut index = proof.leaf_index as usize;
    for sibling in &proof.proof {
        current_hash = if index % 2 == 0 {
            combine_hashes(&current_hash, sibling)
        } else {
            combine_hashes(sibling, &current_hash)
        };
        index /= 2;
    }

    current_hash == *expected_root
}

/// Generate registry digest
pub fn generate_registry_digest(assets: &[AssetEntry], chain_id: &str) -> RegistryDigest {
    let (root, _) = build_asset_registry(assets);

    RegistryDigest {
        version: 1,
        chain_id: chain_id.to_string(),
        asset_tree_root: root,
        asset_count: assets.len() as u32,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    }
}

/// Encode registry digest as QR payload (compact binary format)
///
/// Format:
/// - 3 bytes: prelude (0x53, crypto_type, qr_type)
/// - 4 bytes: version (LE)
/// - 1 byte: chain_id length
/// - N bytes: chain_id
/// - 32 bytes: merkle root hash
/// - 4 bytes: asset count (LE)
/// - 8 bytes: timestamp (LE)
pub fn encode_registry_qr(digest: &RegistryDigest) -> Result<Vec<u8>> {
    let chain_bytes = digest.chain_id.as_bytes();
    if chain_bytes.len() > 255 {
        anyhow::bail!("chain_id too long");
    }

    let mut result = Vec::with_capacity(3 + 4 + 1 + chain_bytes.len() + 32 + 4 + 8);

    // Prelude
    result.push(0x53);
    result.push(PENUMBRA_CRYPTO_TYPE);
    result.push(ASSET_REGISTRY_QR_TYPE);

    // Version
    result.extend_from_slice(&digest.version.to_le_bytes());

    // Chain ID (length-prefixed)
    result.push(chain_bytes.len() as u8);
    result.extend_from_slice(chain_bytes);

    // Merkle root (32 bytes)
    result.extend_from_slice(&digest.asset_tree_root);

    // Asset count
    result.extend_from_slice(&digest.asset_count.to_le_bytes());

    // Timestamp
    result.extend_from_slice(&digest.timestamp.to_le_bytes());

    Ok(result)
}

/// Decode registry digest from QR payload (compact binary format)
pub fn decode_registry_qr(hex_str: &str) -> Result<RegistryDigest> {
    let data: Vec<u8> = (0..hex_str.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("invalid hex: {}", e))?;

    // Minimum size: prelude(3) + version(4) + chain_len(1) + chain(1) + hash(32) + count(4) + timestamp(8) = 53
    if data.len() < 53 {
        anyhow::bail!("payload too short");
    }

    if data[0] != 0x53 || data[1] != PENUMBRA_CRYPTO_TYPE || data[2] != ASSET_REGISTRY_QR_TYPE {
        anyhow::bail!("invalid prelude for registry digest");
    }

    let version = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);

    let chain_len = data[7] as usize;
    if data.len() < 8 + chain_len + 32 + 4 + 8 {
        anyhow::bail!("payload too short for chain_id");
    }

    let chain_id = String::from_utf8(data[8..8 + chain_len].to_vec())
        .map_err(|e| anyhow::anyhow!("invalid chain_id: {}", e))?;

    let hash_start = 8 + chain_len;
    let mut asset_tree_root = [0u8; 32];
    asset_tree_root.copy_from_slice(&data[hash_start..hash_start + 32]);

    let count_start = hash_start + 32;
    let asset_count = u32::from_le_bytes([
        data[count_start],
        data[count_start + 1],
        data[count_start + 2],
        data[count_start + 3],
    ]);

    let ts_start = count_start + 4;
    let timestamp = u64::from_le_bytes([
        data[ts_start],
        data[ts_start + 1],
        data[ts_start + 2],
        data[ts_start + 3],
        data[ts_start + 4],
        data[ts_start + 5],
        data[ts_start + 6],
        data[ts_start + 7],
    ]);

    Ok(RegistryDigest {
        version,
        chain_id,
        asset_tree_root,
        asset_count,
        timestamp,
    })
}

/// Parse prax-registry JSON format into AssetEntry list
pub fn parse_prax_registry(json: &serde_json::Value) -> Result<Vec<AssetEntry>> {
    let mut assets = Vec::new();

    let asset_by_id = json.get("assetById")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow::anyhow!("missing assetById in registry"))?;

    for (_key, asset) in asset_by_id {
        // Get penumbraAssetId.inner (base64 encoded)
        let asset_id_b64 = asset
            .get("penumbraAssetId")
            .and_then(|v| v.get("inner"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing penumbraAssetId.inner"))?;

        // Decode base64 to bytes
        use simple_base64::Engine;
        let asset_id_bytes = simple_base64::engine::general_purpose::STANDARD
            .decode(asset_id_b64)
            .map_err(|e| anyhow::anyhow!("invalid base64: {}", e))?;

        if asset_id_bytes.len() != 32 {
            continue; // Skip invalid asset IDs
        }

        let mut asset_id = [0u8; 32];
        asset_id.copy_from_slice(&asset_id_bytes);

        // Get symbol
        let symbol = asset
            .get("symbol")
            .and_then(|v| v.as_str())
            .unwrap_or("???")
            .to_string();

        // Get base denom
        let denom = asset
            .get("base")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Get decimals from denomUnits (find the display unit's exponent)
        let decimals = asset
            .get("denomUnits")
            .and_then(|v| v.as_array())
            .and_then(|units| {
                units.iter()
                    .filter_map(|u| u.get("exponent").and_then(|e| e.as_u64()))
                    .max()
            })
            .unwrap_or(6) as u8;

        // Get display name
        let display_name = asset
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&symbol)
            .to_string();

        assets.push(AssetEntry {
            asset_id,
            denom,
            symbol,
            decimals,
            display_name,
        });
    }

    // Sort by symbol for consistent ordering
    assets.sort_by(|a, b| a.symbol.cmp(&b.symbol));

    Ok(assets)
}

/// Generate sample asset registry (core Penumbra assets)
#[allow(dead_code)]
pub fn default_asset_registry() -> Vec<AssetEntry> {
    vec![
        AssetEntry {
            asset_id: asset_id_from_denom("upenumbra"),
            denom: "upenumbra".to_string(),
            symbol: "UM".to_string(),
            decimals: 6,
            display_name: "Penumbra".to_string(),
        },
        AssetEntry {
            asset_id: asset_id_from_denom("gm"),
            denom: "gm".to_string(),
            symbol: "GM".to_string(),
            decimals: 6,
            display_name: "GM Token".to_string(),
        },
        AssetEntry {
            asset_id: asset_id_from_denom("gn"),
            denom: "gn".to_string(),
            symbol: "GN".to_string(),
            decimals: 6,
            display_name: "GN Token".to_string(),
        },
        AssetEntry {
            asset_id: asset_id_from_denom("test_usd"),
            denom: "test_usd".to_string(),
            symbol: "TUSD".to_string(),
            decimals: 6,
            display_name: "Test USD".to_string(),
        },
        // IBC assets would be added here from prax-registry
    ]
}

/// Compute asset ID from denom (simplified - real impl uses blake2b)
#[allow(dead_code)]
fn asset_id_from_denom(denom: &str) -> [u8; 32] {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(b"penumbra_asset_id:");
    hasher.update(denom.as_bytes());
    let result = hasher.finalize();
    let mut id = [0u8; 32];
    id.copy_from_slice(&result);
    id
}

/// Display registry digest info
pub fn display_registry(digest: &RegistryDigest) {
    println!("Penumbra Asset Registry Digest");
    println!("==============================");
    println!("Version:     {}", digest.version);
    println!("Chain ID:    {}", digest.chain_id);
    println!("Assets:      {}", digest.asset_count);
    println!("Merkle Root: {}", hex::encode(&digest.asset_tree_root[..16]));
    if digest.timestamp > 0 {
        println!("Timestamp:   {}", digest.timestamp);
    }
}

/// Helper for serde hex encoding of fixed arrays
mod hex_bytes {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        if bytes.len() != 32 {
            return Err(serde::de::Error::custom("expected 32 bytes"));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(arr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merkle_tree_roundtrip() {
        let schema = generate_schema("test-chain");
        let (root, _) = build_action_merkle_tree(&schema);

        // Generate and verify proof for Spend action (field 1)
        let proof = generate_action_proof(&schema, 1).unwrap();
        assert!(verify_action_proof(&proof, &root));

        // Generate and verify proof for Delegate action (field 40)
        let proof = generate_action_proof(&schema, 40).unwrap();
        assert!(verify_action_proof(&proof, &root));
    }

    #[test]
    fn test_digest_encoding() {
        let schema = generate_schema("test-chain");
        let digest = generate_schema_digest(&schema);

        let encoded = encode_digest_qr(&digest).unwrap();
        let hex = encoded.iter().map(|b| format!("{:02x}", b)).collect::<String>();

        let decoded = decode_digest_qr(&hex).unwrap();
        assert_eq!(digest, decoded);
    }

    #[test]
    fn test_proof_fails_with_wrong_root() {
        let schema = generate_schema("test-chain");
        let proof = generate_action_proof(&schema, 1).unwrap();

        let wrong_root = [0xffu8; 32];
        assert!(!verify_action_proof(&proof, &wrong_root));
    }
}
