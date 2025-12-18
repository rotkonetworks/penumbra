//! Storage pruning for Penumbra nodes.
//!
//! This module implements pruning of historical JMT (Jellyfish Merkle Tree) state
//! to reduce disk usage. Pruning removes old versions of the state tree while
//! preserving the current state and a configurable number of recent versions.
//!
//! ## Storage Structure
//!
//! The JMT stores data across multiple column families per substore:
//! - `substore-{prefix}-jmt`: Node keys and nodes (keyed by version prefix)
//! - `substore-{prefix}-jmt-values`: Values at each version (keyed by keyhash + version)
//! - `substore-{prefix}-jmt-keys`: Key preimage index
//! - `substore-{prefix}-jmt-keys-by-keyhash`: Keyhash to preimage index
//! - `substore-{prefix}-nonverifiable`: Non-consensus data
//!
//! ## Pruning Strategy
//!
//! Pruning removes:
//! 1. JMT nodes from versions older than `target_version`
//! 2. JMT values from versions older than `target_version`
//! 3. Optionally, historical nonverifiable data (DEX candlesticks, swap executions, etc.)

use anyhow::{Context, Result};
use rocksdb::{Options, DB};
use std::path::Path;
use tracing::{info, warn};

/// Configuration for storage pruning.
#[derive(Debug, Clone)]
pub struct PruneConfig {
    /// Keep this many recent versions. Versions older than
    /// `latest_version - keep_versions` will be pruned.
    pub keep_versions: u64,
    /// Whether to prune historical DEX data (candlesticks, swap executions, arb executions).
    pub prune_dex_data: bool,
    /// Whether to prune historical SCT data (block timestamps, anchor history).
    pub prune_sct_data: bool,
    /// Batch size for delete operations (to avoid holding locks too long).
    pub batch_size: usize,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            // Keep ~24 hours of history at 5s blocks = 17280 blocks
            keep_versions: 17280,
            prune_dex_data: true,
            prune_sct_data: true,
            batch_size: 10000,
        }
    }
}

/// Statistics from a pruning operation.
#[derive(Debug, Default)]
pub struct PruneStats {
    pub jmt_nodes_deleted: u64,
    pub jmt_values_deleted: u64,
    pub nonverifiable_deleted: u64,
    pub bytes_freed_estimate: u64,
}

/// Prune historical state from the RocksDB storage.
///
/// This function opens the database directly (not through cnidarium) to perform
/// bulk delete operations on historical data.
///
/// # Arguments
/// * `rocksdb_path` - Path to the rocksdb directory
/// * `config` - Pruning configuration
///
/// # Safety
/// This function should only be called when the node is stopped or the database
/// is otherwise not in use.
pub fn prune_storage(rocksdb_path: &Path, config: &PruneConfig) -> Result<PruneStats> {
    info!(?rocksdb_path, ?config, "starting storage pruning");

    let mut opts = Options::default();
    opts.create_if_missing(false);
    opts.create_missing_column_families(false);

    // List all column families
    let cf_names = DB::list_cf(&opts, rocksdb_path)
        .context("failed to list column families")?;

    info!(num_cfs = cf_names.len(), "found column families");

    // Open database with all column families
    let db = DB::open_cf(&opts, rocksdb_path, &cf_names)
        .context("failed to open rocksdb")?;

    // Find the latest version from the main store
    let latest_version = find_latest_version(&db, &cf_names)?;

    if latest_version == u64::MAX {
        warn!("database appears empty, nothing to prune");
        return Ok(PruneStats::default());
    }

    let target_version = latest_version.saturating_sub(config.keep_versions);

    if target_version == 0 {
        info!(
            latest_version,
            keep_versions = config.keep_versions,
            "not enough versions to prune"
        );
        return Ok(PruneStats::default());
    }

    info!(
        latest_version,
        target_version,
        versions_to_prune = target_version,
        "pruning versions older than target"
    );

    let mut stats = PruneStats::default();

    // Prune each substore
    for cf_name in &cf_names {
        if cf_name.ends_with("-jmt") {
            let deleted = prune_jmt_nodes(&db, cf_name, target_version, config.batch_size)?;
            stats.jmt_nodes_deleted += deleted;
        } else if cf_name.ends_with("-jmt-values") {
            let deleted = prune_jmt_values(&db, cf_name, target_version, config.batch_size)?;
            stats.jmt_values_deleted += deleted;
        } else if cf_name.ends_with("-nonverifiable") && config.prune_dex_data {
            let deleted = prune_nonverifiable_data(&db, cf_name, target_version, config)?;
            stats.nonverifiable_deleted += deleted;
        }
    }

    // Compact the database to reclaim space
    info!("compacting database to reclaim space");
    for cf_name in &cf_names {
        if let Some(cf) = db.cf_handle(cf_name) {
            db.compact_range_cf(cf, None::<&[u8]>, None::<&[u8]>);
        }
    }

    info!(?stats, "pruning complete");
    Ok(stats)
}

/// Find the latest JMT version from the main store.
fn find_latest_version(db: &DB, cf_names: &[String]) -> Result<u64> {
    // Look for the main store JMT column family (empty prefix)
    let main_jmt_cf = cf_names
        .iter()
        .find(|name| *name == "substore--jmt")
        .context("main store jmt column family not found")?;

    let cf = db.cf_handle(main_jmt_cf)
        .context("failed to get main jmt column family handle")?;

    let mut iter = db.raw_iterator_cf(cf);
    iter.seek_to_last();

    if iter.valid() {
        if let Some(key) = iter.key() {
            if key.len() >= 8 {
                let version_bytes: [u8; 8] = key[0..8].try_into()?;
                return Ok(u64::from_be_bytes(version_bytes));
            }
        }
    }

    Ok(u64::MAX)
}

/// Prune JMT nodes older than the target version.
///
/// JMT node keys are prefixed with the version in big-endian format,
/// so we can use range deletion to efficiently remove old nodes.
fn prune_jmt_nodes(
    db: &DB,
    cf_name: &str,
    target_version: u64,
    batch_size: usize,
) -> Result<u64> {
    let cf = match db.cf_handle(cf_name) {
        Some(cf) => cf,
        None => return Ok(0),
    };

    info!(cf_name, target_version, "pruning JMT nodes");

    let mut deleted = 0u64;
    let mut batch = rocksdb::WriteBatch::default();

    let mut iter = db.raw_iterator_cf(cf);
    iter.seek_to_first();

    while iter.valid() {
        if let Some(key) = iter.key() {
            if key.len() >= 8 {
                let version_bytes: [u8; 8] = key[0..8].try_into().unwrap_or([0; 8]);
                let version = u64::from_be_bytes(version_bytes);

                if version < target_version {
                    batch.delete_cf(cf, key);
                    deleted += 1;

                    if deleted % batch_size as u64 == 0 {
                        db.write(batch)?;
                        batch = rocksdb::WriteBatch::default();
                        info!(cf_name, deleted, "pruning progress");
                    }
                } else {
                    // Keys are ordered by version, so we can stop here
                    break;
                }
            }
        }
        iter.next();
    }

    if !batch.is_empty() {
        db.write(batch)?;
    }

    info!(cf_name, deleted, "finished pruning JMT nodes");
    Ok(deleted)
}

/// Prune JMT values older than the target version.
///
/// JMT value keys are: keyhash (32 bytes) + version (8 bytes big-endian).
/// We need to scan all keys and check the version suffix.
fn prune_jmt_values(
    db: &DB,
    cf_name: &str,
    target_version: u64,
    batch_size: usize,
) -> Result<u64> {
    let cf = match db.cf_handle(cf_name) {
        Some(cf) => cf,
        None => return Ok(0),
    };

    info!(cf_name, target_version, "pruning JMT values");

    let mut deleted = 0u64;
    let mut batch = rocksdb::WriteBatch::default();

    let mut iter = db.raw_iterator_cf(cf);
    iter.seek_to_first();

    while iter.valid() {
        if let Some(key) = iter.key() {
            // Key format: keyhash (32 bytes) + version (8 bytes BE)
            if key.len() == 40 {
                let version_bytes: [u8; 8] = key[32..40].try_into().unwrap_or([0; 8]);
                let version = u64::from_be_bytes(version_bytes);

                if version < target_version {
                    batch.delete_cf(cf, key);
                    deleted += 1;

                    if deleted % batch_size as u64 == 0 {
                        db.write(batch)?;
                        batch = rocksdb::WriteBatch::default();
                        info!(cf_name, deleted, "pruning progress");
                    }
                }
            }
        }
        iter.next();
    }

    if !batch.is_empty() {
        db.write(batch)?;
    }

    info!(cf_name, deleted, "finished pruning JMT values");
    Ok(deleted)
}

/// Prune nonverifiable data (DEX candlesticks, swap executions, etc.).
///
/// This function removes historical data that is not part of consensus
/// and can be safely pruned without affecting chain validity.
fn prune_nonverifiable_data(
    db: &DB,
    cf_name: &str,
    target_version: u64,
    config: &PruneConfig,
) -> Result<u64> {
    let cf = match db.cf_handle(cf_name) {
        Some(cf) => cf,
        None => return Ok(0),
    };

    info!(cf_name, "pruning nonverifiable data");

    let mut deleted = 0u64;
    let mut batch = rocksdb::WriteBatch::default();

    let mut iter = db.raw_iterator_cf(cf);
    iter.seek_to_first();

    // Prefixes for data that can be pruned
    let prunable_prefixes: &[&[u8]] = if config.prune_dex_data && config.prune_sct_data {
        &[
            b"dex/candlesticks/data/",
            b"dex/swap_execution/",
            b"dex/arb_execution/",
            b"dex/output/",
            b"sct/block_manager/historical_block_timestamp/",
        ]
    } else if config.prune_dex_data {
        &[
            b"dex/candlesticks/data/",
            b"dex/swap_execution/",
            b"dex/arb_execution/",
            b"dex/output/",
        ]
    } else if config.prune_sct_data {
        &[b"sct/block_manager/historical_block_timestamp/"]
    } else {
        &[]
    };

    while iter.valid() {
        if let Some(key) = iter.key() {
            // Check if this key matches any prunable prefix
            let should_prune = prunable_prefixes.iter().any(|prefix| key.starts_with(prefix));

            if should_prune {
                // Extract height from key if possible
                // Most keys have format: prefix/{height:020}/...
                if let Some(height) = extract_height_from_key(key) {
                    if height < target_version {
                        batch.delete_cf(cf, key);
                        deleted += 1;

                        if deleted % config.batch_size as u64 == 0 {
                            db.write(batch)?;
                            batch = rocksdb::WriteBatch::default();
                            info!(cf_name, deleted, "pruning progress");
                        }
                    }
                }
            }
        }
        iter.next();
    }

    if !batch.is_empty() {
        db.write(batch)?;
    }

    info!(cf_name, deleted, "finished pruning nonverifiable data");
    Ok(deleted)
}

/// Extract block height from a key that contains a zero-padded height.
/// Keys typically have format: prefix/{height:020}/...
fn extract_height_from_key(key: &[u8]) -> Option<u64> {
    // Find sequences of digits that could be heights
    let key_str = std::str::from_utf8(key).ok()?;

    // Look for the pattern of 20 consecutive digits (zero-padded height)
    for (i, window) in key_str.as_bytes().windows(20).enumerate() {
        if window.iter().all(|&b| b.is_ascii_digit()) {
            // Check that it's bounded by non-digits (or start/end)
            let before_ok = i == 0 || !key_str.as_bytes()[i - 1].is_ascii_digit();
            let after_ok = i + 20 >= key_str.len() || !key_str.as_bytes()[i + 20].is_ascii_digit();

            if before_ok && after_ok {
                if let Ok(height) = std::str::from_utf8(window).ok()?.parse::<u64>() {
                    return Some(height);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_height_from_key() {
        // DEX candlestick key format
        assert_eq!(
            extract_height_from_key(b"dex/candlesticks/data/asset1/asset2/00000000000000012345"),
            Some(12345)
        );

        // Swap execution key format
        assert_eq!(
            extract_height_from_key(b"dex/swap_execution/00000000000000054321/asset1/asset2"),
            Some(54321)
        );

        // Historical timestamp
        assert_eq!(
            extract_height_from_key(b"sct/block_manager/historical_block_timestamp/00000000000000099999"),
            Some(99999)
        );

        // No height
        assert_eq!(extract_height_from_key(b"some/random/key"), None);
    }
}
