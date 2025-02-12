// Copyright 2020 Ant Group. All rights reserved.
// Copyright (C) 2021 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! A blob cache layer over storage backend to improve performance.
//!
//! One of Rafs filesystem's goal is to support "on demand data loading". On demand loading may
//! help to speed up application/container startup, but it may also cause serious performance
//! penalty if all data chunks are retrieved from remoted backend storage. So cache layer is
//! introduced between Rafs filesystem and backend storage, which caches remote data onto local
//! storage and merge small data request into bigger request to improve network performance.
//!
//! There are several cache drivers implemented:
//! - [DummyCacheMgr](dummycache/struct.DummyCacheMgr.html): a dummy implementation of
//!   `BlobCacheMgr`, simply reporting each chunk as cached or not cached according to
//!   configuration.

use std::cmp;
use std::io::Result;
use std::sync::Arc;
use std::time::Instant;

use fuse_backend_rs::file_buf::FileVolatileSlice;
use nydus_utils::{compress, digest};

use crate::backend::{BlobBackend, BlobReader};
use crate::cache::state::ChunkMap;
use crate::device::{
    BlobChunkInfo, BlobInfo, BlobIoDesc, BlobIoRange, BlobIoVec, BlobObject, BlobPrefetchRequest,
};
use crate::utils::{alloc_buf, check_digest};
use crate::{StorageResult, RAFS_MAX_CHUNK_SIZE};

mod cachedfile;
mod dummycache;
mod filecache;
mod fscache;
mod worker;

pub mod state;

pub use dummycache::DummyCacheMgr;
pub use filecache::FileCacheMgr;
pub use fscache::FsCacheMgr;

/// Timeout in milli-seconds to retrieve blob data from backend storage.
pub const SINGLE_INFLIGHT_WAIT_TIMEOUT: u64 = 2000;

struct BlobIoMergeState<'a, F: FnMut(BlobIoRange)> {
    cb: F,
    // size of compressed data
    size: u32,
    bios: Vec<&'a BlobIoDesc>,
}

impl<'a, F: FnMut(BlobIoRange)> BlobIoMergeState<'a, F> {
    /// Create a new instance of 'IoMergeState`.
    pub fn new(bio: &'a BlobIoDesc, cb: F) -> Self {
        let size = bio.chunkinfo.compressed_size();

        BlobIoMergeState {
            cb,
            size,
            bios: vec![bio],
        }
    }

    /// Get size of pending io operations.
    #[inline]
    pub fn size(&self) -> usize {
        self.size as usize
    }

    /// Push a new io descriptor into the pending list.
    #[inline]
    pub fn push(&mut self, bio: &'a BlobIoDesc) {
        let size = bio.chunkinfo.compressed_size();

        assert!(self.size.checked_add(size).is_some());
        self.bios.push(bio);
        self.size += bio.chunkinfo.compressed_size();
    }

    /// Issue all pending io descriptors.
    #[inline]
    pub fn issue(&mut self) {
        if !self.bios.is_empty() {
            let mut mr = BlobIoRange::new(self.bios[0], self.bios.len());
            for bio in self.bios[1..].iter() {
                mr.merge(bio, 0);
            }
            (self.cb)(mr);

            self.bios.truncate(0);
            self.size = 0;
        }
    }

    /// Merge and issue all blob Io descriptors.
    pub fn merge_and_issue(bios: &[BlobIoDesc], max_size: usize, op: F) {
        if !bios.is_empty() {
            let mut index = 1;
            let mut state = BlobIoMergeState::new(&bios[0], op);

            for cur_bio in &bios[1..] {
                if !bios[index - 1].is_continuous(cur_bio, 0) || state.size() >= max_size {
                    state.issue();
                }
                state.push(cur_bio);
                index += 1
            }
            state.issue();
        }
    }
}

/// Trait representing a cache object for a blob on backend storage.
///
/// The caller may use the `BlobCache` trait to access blob data on backend storage, with an
/// optional intermediate cache layer to improve performance.
pub trait BlobCache: Send + Sync {
    /// Get id of the blob object.
    fn blob_id(&self) -> &str;

    /// Get size of the decompressed blob object.
    fn blob_uncompressed_size(&self) -> Result<u64>;

    /// Get size of the compressed blob object.
    fn blob_compressed_size(&self) -> Result<u64>;

    /// Get data compression algorithm to handle chunks in the blob.
    fn compressor(&self) -> compress::Algorithm;

    /// Get message digest algorithm to handle chunks in the blob.
    fn digester(&self) -> digest::Algorithm;

    /// Check whether the cache object is for an stargz image with legacy chunk format.
    fn is_legacy_stargz(&self) -> bool;

    /// Get maximum size of gzip compressed data.
    fn get_legacy_stargz_size(&self, offset: u64, uncomp_size: usize) -> Result<usize> {
        let blob_size = self.blob_compressed_size()?;
        let max_size = blob_size.checked_sub(offset).ok_or_else(|| {
            einval!(format!(
                "chunk compressed offset {:x} is bigger than blob file size {:x}",
                offset, blob_size
            ))
        })?;
        let max_size = cmp::min(max_size, usize::MAX as u64) as usize;
        Ok(compress::compute_compressed_gzip_size(
            uncomp_size,
            max_size,
        ))
    }

    /// Check whether need to validate the data chunk by digest value.
    fn need_validate(&self) -> bool;

    /// Get the [BlobReader](../backend/trait.BlobReader.html) to read data from storage backend.
    fn reader(&self) -> &dyn BlobReader;

    /// Get the underlying `ChunkMap` object.
    fn get_chunk_map(&self) -> &Arc<dyn ChunkMap>;

    /// Get the `BlobChunkInfo` object corresponding to `chunk_index`.
    fn get_chunk_info(&self, chunk_index: u32) -> Option<Arc<dyn BlobChunkInfo>>;

    /// Get a `BlobObject` instance to directly access uncompressed blob file.
    fn get_blob_object(&self) -> Option<&dyn BlobObject> {
        None
    }

    /// Enable prefetching blob data in background.
    ///
    /// It should be paired with stop_prefetch().
    fn start_prefetch(&self) -> StorageResult<()>;

    /// Stop prefetching blob data in background.
    ///
    /// It should be paired with start_prefetch().
    fn stop_prefetch(&self) -> StorageResult<()>;

    // Check whether data prefetch is still active.
    fn is_prefetch_active(&self) -> bool;

    /// Start to prefetch requested data in background.
    fn prefetch(
        &self,
        cache: Arc<dyn BlobCache>,
        prefetches: &[BlobPrefetchRequest],
        bios: &[BlobIoDesc],
    ) -> StorageResult<usize>;

    /// Execute filesystem data prefetch.
    fn prefetch_range(&self, _range: &BlobIoRange) -> Result<usize> {
        Err(enosys!("doesn't support prefetch_range()"))
    }

    /// Read chunk data described by the blob Io descriptors from the blob cache into the buffer.
    fn read(&self, iovec: &mut BlobIoVec, buffers: &[FileVolatileSlice]) -> Result<usize>;

    /// Read multiple chunks from the blob cache in batch mode.
    ///
    /// This is an interface to optimize chunk data fetch performance by merging multiple continuous
    /// chunks into one backend request. Callers must ensure that chunks in `chunks` covers a
    /// continuous range, and the range exactly matches [`blob_offset`..`blob_offset` + `blob_size`].
    /// Function `read_chunks_from_backend()` returns one buffer containing decompressed chunk data
    /// for each entry in the `chunks` array in corresponding order.
    ///
    /// This method returns success only if all requested data are successfully fetched.
    fn read_chunks_from_backend(
        &self,
        blob_offset: u64,
        blob_size: usize,
        chunks: &[Arc<dyn BlobChunkInfo>],
        prefetch: bool,
    ) -> Result<(Vec<Vec<u8>>, Vec<u8>)> {
        // Read requested data from the backend by altogether.
        let mut c_buf = alloc_buf(blob_size);
        let start = Instant::now();
        let nr_read = self
            .reader()
            .read(c_buf.as_mut_slice(), blob_offset)
            .map_err(|e| eio!(e))?;
        if nr_read != blob_size {
            return Err(eio!(format!(
                "request for {} bytes but got {} bytes",
                blob_size, nr_read
            )));
        }
        let duration = Instant::now().duration_since(start).as_millis();
        debug!(
            "read_chunks_from_backend: {} {} {} bytes at {}, duration {}ms",
            std::thread::current().name().unwrap_or_default(),
            if prefetch { "prefetch" } else { "fetch" },
            blob_size,
            blob_offset,
            duration
        );

        self.decompress_normal_chunks(blob_offset, chunks, c_buf)
    }

    /// Read a whole chunk directly from the storage backend.
    ///
    /// The fetched chunk data may be compressed or not, which depends on chunk information from
    /// `chunk`.Moreover, chunk data from backend storage may be validated per user's configuration.
    fn read_chunk_from_backend(
        &self,
        chunk: &dyn BlobChunkInfo,
        buffer: &mut [u8],
        force_validation: bool,
    ) -> Result<Option<Vec<u8>>> {
        let offset = chunk.compressed_offset();

        let mut c_buf = None;
        if chunk.is_compressed() {
            let c_size = if self.is_legacy_stargz() {
                self.get_legacy_stargz_size(offset, buffer.len())?
            } else {
                chunk.compressed_size() as usize
            };
            let mut raw_buffer = alloc_buf(c_size);
            let size = self
                .reader()
                .read(raw_buffer.as_mut_slice(), offset)
                .map_err(|e| eio!(e))?;
            if size != raw_buffer.len() {
                return Err(eio!("storage backend returns less data than requested"));
            }
            self.decompress_chunk_data(&raw_buffer, buffer, true)?;
            c_buf = Some(raw_buffer);
        } else {
            let size = self.reader().read(buffer, offset).map_err(|e| eio!(e))?;
            if size != buffer.len() {
                return Err(eio!("storage backend returns less data than requested"));
            }
        }

        self.validate_chunk_data(chunk, buffer, force_validation)?;

        Ok(c_buf)
    }

    fn decompress_normal_chunks(
        &self,
        blob_offset: u64,
        chunks: &[Arc<dyn BlobChunkInfo>],
        c_buf: Vec<u8>,
    ) -> Result<(Vec<Vec<u8>>, Vec<u8>)> {
        let mut buffers: Vec<Vec<u8>> = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let offset = chunk.compressed_offset();
            let size = chunk.compressed_size();
            let d_size = chunk.uncompressed_size() as usize;
            // Ensure BlobIoChunk is valid and continuous.
            if offset - blob_offset > usize::MAX as u64
                || offset.checked_add(size as u64).is_none()
                || offset + size as u64 - blob_offset > c_buf.len() as u64
                || d_size as u64 > RAFS_MAX_CHUNK_SIZE
            {
                return Err(eio!(format!(
                    "chunks to read_chunks() is invalid, offset {} blob_offset {} d_size {}",
                    offset, blob_offset, d_size
                )));
            }

            let offset_merged = (offset - blob_offset) as usize;
            let end_merged = offset_merged + size as usize;
            let buf = &c_buf[offset_merged..end_merged];
            let mut buffer = alloc_buf(d_size);
            self.decompress_chunk_data(buf, &mut buffer, chunk.is_compressed())?;
            self.validate_chunk_data(chunk.as_ref(), &buffer, self.need_validate())?;
            buffers.push(buffer);
        }

        Ok((buffers, c_buf))
    }

    /// Decompress chunk data.
    fn decompress_chunk_data(
        &self,
        raw_buffer: &[u8],
        buffer: &mut [u8],
        is_compressed: bool,
    ) -> Result<()> {
        if is_compressed {
            let ret = compress::decompress(raw_buffer, buffer, self.compressor()).map_err(|e| {
                error!("failed to decompress chunk: {}", e);
                e
            })?;
            if ret != buffer.len() {
                return Err(eother!("size of decompressed data doesn't match expected"));
            }
        } else if raw_buffer.as_ptr() != buffer.as_ptr() {
            // raw_chunk and chunk may point to the same buffer, so only copy data when needed.
            buffer.copy_from_slice(raw_buffer);
        }
        Ok(())
    }

    /// Validate chunk data.
    fn validate_chunk_data(
        &self,
        chunk: &dyn BlobChunkInfo,
        buffer: &[u8],
        force_validation: bool,
    ) -> Result<usize> {
        let d_size = chunk.uncompressed_size() as usize;
        if buffer.len() != d_size {
            Err(eio!("uncompressed size and buffer size doesn't match"))
        } else if (self.need_validate() || force_validation)
            && !check_digest(buffer, chunk.chunk_id(), self.digester())
        {
            Err(eio!("data digest value doesn't match"))
        } else {
            Ok(d_size)
        }
    }
}

/// Trait representing blob manager to manage a group of [BlobCache](trait.BlobCache.html) objects.
///
/// The main responsibility of the blob cache manager is to create blob cache objects for blobs,
/// all IO requests should be issued to the blob cache object directly.
pub(crate) trait BlobCacheMgr: Send + Sync {
    /// Initialize the blob cache manager.
    fn init(&self) -> Result<()>;

    /// Tear down the blob cache manager.
    fn destroy(&self);

    /// Garbage-collect unused resources.
    ///
    /// Return true if the blob cache manager itself should be garbage-collected.
    fn gc(&self, _id: Option<&str>) -> bool;

    /// Get the underlying `BlobBackend` object of the blob cache object.
    fn backend(&self) -> &(dyn BlobBackend);

    /// Get the blob cache to provide access to the `blob` object.
    fn get_blob_cache(&self, blob_info: &Arc<BlobInfo>) -> Result<Arc<dyn BlobCache>>;

    /// Check the blob cache data status, if data all ready stop prefetch workers.
    fn check_stat(&self);
}

#[cfg(test)]
mod tests {
    use crate::device::{BlobChunkFlags, BlobFeatures};
    use crate::test::MockChunkInfo;

    use super::*;

    #[test]
    fn test_io_merge_state_new() {
        let blob_info = Arc::new(BlobInfo::new(
            1,
            "test1".to_owned(),
            0x200000,
            0x100000,
            0x100000,
            512,
            BlobFeatures::V5_NO_EXT_BLOB_TABLE,
        ));
        let chunk1 = Arc::new(MockChunkInfo {
            block_id: Default::default(),
            blob_index: 1,
            flags: BlobChunkFlags::empty(),
            compress_size: 0x800,
            uncompress_size: 0x1000,
            compress_offset: 0,
            uncompress_offset: 0,
            file_offset: 0,
            index: 0,
            reserved: 0,
        }) as Arc<dyn BlobChunkInfo>;
        let chunk2 = Arc::new(MockChunkInfo {
            block_id: Default::default(),
            blob_index: 1,
            flags: BlobChunkFlags::empty(),
            compress_size: 0x800,
            uncompress_size: 0x1000,
            compress_offset: 0x800,
            uncompress_offset: 0x1000,
            file_offset: 0x1000,
            index: 1,
            reserved: 0,
        }) as Arc<dyn BlobChunkInfo>;
        let chunk3 = Arc::new(MockChunkInfo {
            block_id: Default::default(),
            blob_index: 1,
            flags: BlobChunkFlags::empty(),
            compress_size: 0x800,
            uncompress_size: 0x1000,
            compress_offset: 0x1000,
            uncompress_offset: 0x1000,
            file_offset: 0x1000,
            index: 1,
            reserved: 0,
        }) as Arc<dyn BlobChunkInfo>;

        let cb = |_merged| {};
        let desc1 = BlobIoDesc {
            blob: blob_info.clone(),
            chunkinfo: chunk1.into(),
            offset: 0,
            size: 0x1000,
            user_io: true,
        };
        let mut state = BlobIoMergeState::new(&desc1, cb);
        assert_eq!(state.size(), 0x800);
        assert_eq!(state.bios.len(), 1);

        let desc2 = BlobIoDesc {
            blob: blob_info.clone(),
            chunkinfo: chunk2.into(),
            offset: 0,
            size: 0x1000,
            user_io: true,
        };
        state.push(&desc2);
        assert_eq!(state.size, 0x1000);
        assert_eq!(state.bios.len(), 2);

        state.issue();
        assert_eq!(state.size(), 0x0);
        assert_eq!(state.bios.len(), 0);

        let desc3 = BlobIoDesc {
            blob: blob_info,
            chunkinfo: chunk3.into(),
            offset: 0,
            size: 0x1000,
            user_io: true,
        };
        state.push(&desc3);
        assert_eq!(state.size, 0x800);
        assert_eq!(state.bios.len(), 1);

        state.issue();
        assert_eq!(state.size(), 0x0);
        assert_eq!(state.bios.len(), 0);

        let mut count = 0;
        BlobIoMergeState::merge_and_issue(
            &[desc1.clone(), desc2.clone(), desc3.clone()],
            0x4000,
            |_v| count += 1,
        );
        assert_eq!(count, 1);

        let mut count = 0;
        BlobIoMergeState::merge_and_issue(
            &[desc1.clone(), desc2.clone(), desc3.clone()],
            0x1000,
            |_v| count += 1,
        );
        assert_eq!(count, 2);

        let mut count = 0;
        BlobIoMergeState::merge_and_issue(&[desc1.clone(), desc3.clone()], 0x4000, |_v| count += 1);
        assert_eq!(count, 2);

        assert!(desc1.is_continuous(&desc2, 0));
        assert!(!desc1.is_continuous(&desc3, 0));
    }
}
