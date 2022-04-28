/// This file is a modified copy of the file from https://github.com/tonlabs/ton-labs-node
///
/// Changes:
/// - replaced old `failure` crate with `anyhow`
/// - replaced file storage with direct rocksdb storage
/// - removed all temporary unused code
///
use std::borrow::Borrow;
use std::collections::BTreeSet;
use std::convert::TryInto;
use std::hash::Hash;
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::RwLock;

use super::archive_package::*;
use super::block_handle::*;
use super::package_entry_id::*;
use crate::storage::{columns, StoredValue, TopBlocks, Tree};

pub struct ArchiveManager {
    db: Arc<rocksdb::DB>,
    archives: Tree<columns::Archives>,
    package_entries: Tree<columns::PackageEntries>,
    block_handles: Tree<columns::BlockHandles>,
    key_blocks: Tree<columns::KeyBlocks>,
    archive_ids: RwLock<BTreeSet<u32>>,
}

impl ArchiveManager {
    pub fn with_db(db: &Arc<rocksdb::DB>) -> Result<Self> {
        let manager = Self {
            db: db.clone(),
            archives: Tree::new(db)?,
            package_entries: Tree::new(db)?,
            block_handles: Tree::new(db)?,
            key_blocks: Tree::new(db)?,
            archive_ids: Default::default(),
        };

        manager.preload()?;

        Ok(manager)
    }

    fn preload(&self) -> Result<()> {
        fn check_archive(value: &[u8]) -> Result<(), ArchivePackageError> {
            let mut verifier = ArchivePackageVerifier::default();
            verifier.verify(value)?;
            verifier.final_check()
        }

        let mut iter = self.archives.raw_iterator();
        iter.seek_to_first();

        let mut archive_ids = self.archive_ids.write();

        while let (Some(key), value) = (iter.key(), iter.value()) {
            let archive_id = u32::from_be_bytes(
                key.try_into()
                    .with_context(|| format!("Invalid archive key: {}", hex::encode(key)))?,
            );

            if let Some(Err(e)) = value.map(check_archive) {
                log::error!("Failed to read archive {archive_id}: {e:?}")
            }

            archive_ids.insert(archive_id);
            iter.next();
        }

        log::info!("Selfcheck complete");
        Ok(())
    }

    pub fn add_data<I>(&self, id: &PackageEntryId<I>, data: &[u8]) -> Result<()>
    where
        I: Borrow<ton_block::BlockIdExt> + Hash,
    {
        self.package_entries.insert(id.to_vec(), data)
    }

    pub fn has_data<I>(&self, id: &PackageEntryId<I>) -> Result<bool>
    where
        I: Borrow<ton_block::BlockIdExt> + Hash,
    {
        self.package_entries.contains_key(id.to_vec())
    }

    pub async fn get_data<I>(&self, handle: &BlockHandle, id: &PackageEntryId<I>) -> Result<Vec<u8>>
    where
        I: Borrow<ton_block::BlockIdExt> + Hash,
    {
        let _lock = match &id {
            PackageEntryId::Block(_) => handle.block_data_lock().read().await,
            PackageEntryId::Proof(_) | PackageEntryId::ProofLink(_) => {
                handle.proof_data_lock().read().await
            }
        };

        match self.package_entries.get(id.to_vec())? {
            Some(a) => Ok(a.to_vec()),
            None => Err(ArchiveManagerError::InvalidBlockData.into()),
        }
    }

    pub async fn get_data_ref<'a, I>(
        &'a self,
        handle: &'a BlockHandle,
        id: &PackageEntryId<I>,
    ) -> Result<impl AsRef<[u8]> + 'a>
    where
        I: Borrow<ton_block::BlockIdExt> + Hash,
    {
        let lock = match id {
            PackageEntryId::Block(_) => handle.block_data_lock().read().await,
            PackageEntryId::Proof(_) | PackageEntryId::ProofLink(_) => {
                handle.proof_data_lock().read().await
            }
        };

        match self.package_entries.get(id.to_vec())? {
            Some(data) => Ok(BlockContentsLock { _lock: lock, data }),
            None => Err(ArchiveManagerError::InvalidBlockData.into()),
        }
    }

    pub fn gc(
        &self,
        max_blocks_per_batch: Option<usize>,
        top_blocks: &TopBlocks,
    ) -> Result<BlockGcStats> {
        let mut stats = BlockGcStats::default();

        // Cache cfs before loop
        let blocks_cf = self.package_entries.get_cf();
        let block_handles_cf = self.block_handles.get_cf();
        let key_blocks_cf = self.key_blocks.get_cf();
        let raw_db = self.package_entries.raw_db_handle().clone();

        // Create batch
        let mut batch = rocksdb::WriteBatch::default();
        let mut batch_len = 0;

        // Iterate all entries and find expired items
        let blocks_iter = self.package_entries.iterator(rocksdb::IteratorMode::Start);
        for (key, _) in blocks_iter {
            // Read only prefix with shard ident and seqno
            let prefix = PackageEntryIdPrefix::from_slice(key.as_ref())?;

            // Don't gc latest blocks
            if top_blocks.contains_shard_seq_no(&prefix.shard_ident, prefix.seq_no) {
                continue;
            }

            // Additionally check whether this item is a key block
            if prefix.seq_no == 0
                || prefix.shard_ident.is_masterchain()
                    && raw_db
                        .get_pinned_cf_opt(
                            &key_blocks_cf,
                            prefix.seq_no.to_be_bytes(),
                            self.key_blocks.read_config(),
                        )?
                        .is_some()
            {
                // Don't remove key blocks
                continue;
            }

            // Add item to the batch
            batch.delete_cf(&blocks_cf, &key);
            stats.total_package_entries_removed += 1;
            if prefix.shard_ident.is_masterchain() {
                stats.mc_package_entries_removed += 1;
            }

            // Key structure:
            // [workchain id, 4 bytes]
            // [shard id, 8 bytes]
            // [seqno, 4 bytes]
            // [root hash, 32 bytes] <-
            // ..
            if key.len() >= 48 {
                batch.delete_cf(&block_handles_cf, &key[16..48]);
                stats.total_handles_removed += 1;
            }

            batch_len += 1;
            if matches!(
                max_blocks_per_batch,
                Some(max_blocks_per_batch) if batch_len >= max_blocks_per_batch
            ) {
                log::info!(
                    "Applying intermediate batch {}...",
                    stats.total_package_entries_removed
                );
                let batch = std::mem::take(&mut batch);
                raw_db.write(batch)?;
                batch_len = 0;
            }
        }

        if batch_len > 0 {
            log::info!("Applying final batch...");
            raw_db.write(batch)?;
        }

        // Done
        Ok(stats)
    }

    pub async fn move_into_archive(&self, handle: &BlockHandle) -> Result<()> {
        if handle.meta().is_archived() {
            return Ok(());
        }
        if !handle.meta().set_is_moving_to_archive() {
            return Ok(());
        }

        // Prepare data
        let block_id = handle.id();

        let has_data = handle.meta().has_data();
        let mut is_link = false;
        let has_proof = handle.has_proof_or_link(&mut is_link);

        let block_data = if has_data {
            let lock = handle.block_data_lock().write().await;

            let entry_id = PackageEntryId::Block(block_id);
            let data = self.make_archive_segment(&entry_id)?;

            Some((lock, data))
        } else {
            None
        };

        let block_proof_data = if has_proof {
            let lock = handle.proof_data_lock().write().await;

            let entry_id = if is_link {
                PackageEntryId::ProofLink(block_id)
            } else {
                PackageEntryId::Proof(block_id)
            };
            let data = self.make_archive_segment(&entry_id)?;

            Some((lock, data))
        } else {
            None
        };

        // Prepare cf
        let storage_cf = self.archives.get_cf();
        let handle_cf = self.block_handles.get_cf();

        // Prepare archive
        let archive_id = self.compute_archive_id(handle);
        let archive_id_bytes = archive_id.to_be_bytes();

        // 0. Create transaction
        let mut batch = rocksdb::WriteBatch::default();
        // 1. Append archive segment with block data
        if let Some((_, data)) = &block_data {
            batch.merge_cf(&storage_cf, &archive_id_bytes, data);
        }
        // 2. Append archive segment with block proof data
        if let Some((_, data)) = &block_proof_data {
            batch.merge_cf(&storage_cf, &archive_id_bytes, data);
        }
        // 3. Update block handle meta
        if handle.meta().set_is_archived() {
            batch.put_cf(
                &handle_cf,
                handle.id().root_hash.as_slice(),
                handle.meta().to_vec(),
            );
        }
        // 5. Execute transaction
        self.db.write(batch)?;

        // Block will be removed after blocks gc

        // Done
        Ok(())
    }

    pub async fn move_into_archive_with_data(
        &self,
        handle: &BlockHandle,
        block_data: &[u8],
        block_proof_data: &[u8],
    ) -> Result<()> {
        if handle.meta().is_archived() {
            return Ok(());
        }
        if !handle.meta().set_is_moving_to_archive() {
            return Ok(());
        }

        // Prepare cf
        let storage_cf = self.archives.get_cf();
        let handle_cf = self.block_handles.get_cf();

        // Prepare archive
        let archive_id = self.compute_archive_id(handle);
        let archive_id_bytes = archive_id.to_be_bytes();

        let mut batch = rocksdb::WriteBatch::default();
        batch.merge_cf(&storage_cf, &archive_id_bytes, block_data);
        batch.merge_cf(&storage_cf, &archive_id_bytes, block_proof_data);
        if handle.meta().set_is_archived() {
            batch.put_cf(
                &handle_cf,
                handle.id().root_hash.as_slice(),
                handle.meta().to_vec(),
            );
        }
        self.db.write(batch)?;

        Ok(())
    }

    pub fn get_archive_id(&self, mc_seq_no: u32) -> Option<u32> {
        match self.archive_ids.read().range(..=mc_seq_no).next_back() {
            // NOTE: handles case when mc_seq_no is far in the future.
            // However if there is a key block between `id` and `mc_seq_no`,
            // this will return an archive without that specified block.
            Some(id) if mc_seq_no < id + ARCHIVE_PACKAGE_SIZE => Some(*id),
            _ => None,
        }
    }

    #[allow(unused)]
    pub fn get_archives(
        &self,
        range: impl RangeBounds<u32> + 'static,
    ) -> impl Iterator<Item = (u32, Vec<u8>)> + '_ {
        struct ArchivesIterator<'a> {
            first: bool,
            ids: (Bound<u32>, Bound<u32>),
            iter: rocksdb::DBRawIterator<'a>,
        }

        impl<'a> Iterator for ArchivesIterator<'a> {
            type Item = (u32, Vec<u8>);

            fn next(&mut self) -> Option<Self::Item> {
                if self.first {
                    match self.ids.0 {
                        Bound::Included(id) => {
                            self.iter.seek(id.to_be_bytes());
                        }
                        Bound::Excluded(id) => {
                            self.iter.seek((id + 1).to_be_bytes());
                        }
                        Bound::Unbounded => {
                            self.iter.seek_to_first();
                        }
                    }
                    self.first = false;
                } else {
                    self.iter.next();
                }

                match (self.iter.key(), self.iter.value()) {
                    (Some(key), Some(value)) => {
                        let id = u32::from_be_bytes(key.try_into().unwrap_or_default());
                        match self.ids.1 {
                            Bound::Included(bound_id) if id > bound_id => None,
                            Bound::Excluded(bound_id) if id >= bound_id => None,
                            _ => Some((id, value.to_vec())),
                        }
                    }
                    _ => None,
                }
            }
        }

        ArchivesIterator {
            first: true,
            ids: (range.start_bound().cloned(), range.end_bound().cloned()),
            iter: self.archives.raw_iterator(),
        }
    }

    pub fn get_archive_slice(
        &self,
        id: u32,
        offset: usize,
        limit: usize,
    ) -> Result<Option<Vec<u8>>> {
        match self.archives.get(id.to_be_bytes())? {
            Some(slice) if offset < slice.len() => {
                let end = std::cmp::min(offset.saturating_add(limit), slice.len());
                Ok(Some(slice[offset..end].to_vec()))
            }
            Some(_) => Err(ArchiveManagerError::InvalidOffset.into()),
            None => Ok(None),
        }
    }

    pub fn remove_outdated_archives(&self, until_id: u32) -> Result<()> {
        let mut archive_ids = self.archive_ids.write();

        let retained_ids = match archive_ids.iter().rev().find(|&id| *id < until_id).cloned() {
            // Splits `archive_ids` into two parts - [..until_id] and [until_id..]
            // `archive_ids` will now contain [..until_id]
            Some(until_id) => archive_ids.split_off(&until_id),
            None => {
                log::info!("Archives GC: nothing to remove");
                return Ok(());
            }
        };
        // so we must swap maps to retain [until_id..] and get ids to remove
        let removed_ids = std::mem::replace(&mut *archive_ids, retained_ids);

        // Print removed range bounds
        match (removed_ids.iter().next(), removed_ids.iter().next_back()) {
            (Some(first), Some(last)) => {
                let len = removed_ids.len();
                log::info!("Archives GC: removing {len} archives (from {first} to {last})...");
            }
            _ => {
                log::info!("Archives GC: nothing to remove");
                return Ok(());
            }
        }

        // Remove archives
        let archives_cf = self.archives.get_cf();

        let mut batch = rocksdb::WriteBatch::default();
        for id in removed_ids {
            batch.delete_cf(&archives_cf, id.to_be_bytes());
        }

        self.db.write(batch)?;

        log::info!("Archives GC: done");
        Ok(())
    }

    fn compute_archive_id(&self, handle: &BlockHandle) -> u32 {
        let mc_seq_no = handle.masterchain_ref_seqno();

        if handle.meta().is_key_block() {
            self.archive_ids.write().insert(mc_seq_no);
            return mc_seq_no;
        }

        let mut archive_id = mc_seq_no - mc_seq_no % ARCHIVE_SLICE_SIZE;

        let prev_id = {
            let latest_archives = self.archive_ids.read();
            latest_archives.range(..=mc_seq_no).next_back().cloned()
        };

        if let Some(prev_id) = prev_id {
            if archive_id < prev_id {
                archive_id = prev_id;
            }
        }

        if mc_seq_no.saturating_sub(archive_id) >= ARCHIVE_PACKAGE_SIZE {
            self.archive_ids.write().insert(mc_seq_no);
            archive_id = mc_seq_no;
        }

        archive_id
    }

    fn make_archive_segment<I>(&self, entry_id: &PackageEntryId<I>) -> Result<Vec<u8>>
    where
        I: Borrow<ton_block::BlockIdExt> + Hash,
    {
        match self.package_entries.get(entry_id.to_vec())? {
            Some(data) => make_archive_segment(&entry_id.filename(), &data).map_err(From::from),
            None => Err(ArchiveManagerError::InvalidBlockData.into()),
        }
    }
}

#[derive(Debug, Copy, Clone, Default)]
pub struct BlockGcStats {
    pub mc_package_entries_removed: usize,
    pub total_package_entries_removed: usize,
    pub total_handles_removed: usize,
}

struct BlockContentsLock<'a> {
    _lock: tokio::sync::RwLockReadGuard<'a, ()>,
    data: rocksdb::DBPinnableSlice<'a>,
}

impl<'a> AsRef<[u8]> for BlockContentsLock<'a> {
    fn as_ref(&self) -> &[u8] {
        self.data.as_ref()
    }
}

pub const ARCHIVE_PACKAGE_SIZE: u32 = 100;
pub const ARCHIVE_SLICE_SIZE: u32 = 20_000;

#[derive(thiserror::Error, Debug)]
enum ArchiveManagerError {
    #[error("Invalid block data")]
    InvalidBlockData,
    #[error("Offset is outside of the archive slice")]
    InvalidOffset,
}
