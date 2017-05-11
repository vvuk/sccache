// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use cache::disk::DiskCache;
#[cfg(feature = "redis")]
use cache::redis::RedisCache;
#[cfg(feature = "s3")]
use cache::s3::S3Cache;
use config::{self, CONFIG};
use futures_cpupool::CpuPool;
use std::fmt;
use std::io::{
    self,
    Read,
    Seek,
    Write,
};
use std::sync::Arc;
use std::time::Duration;
use tokio_core::reactor::Handle;
use zip::{CompressionMethod, ZipArchive, ZipWriter};
use zip::write::FileOptions;

use errors::*;

/// Result of a cache lookup.
pub enum Cache {
    /// Result was found in cache.
    Hit(CacheRead),
    /// Result was not found in cache.
    Miss,
    /// Cache entry should be ignored, force compilation.
    Recache,
}

impl fmt::Debug for Cache {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Cache::Hit(_) => write!(f, "Cache::Hit(...)"),
            Cache::Miss => write!(f, "Cache::Miss"),
            Cache::Recache => write!(f, "Cache::Recache"),
        }
    }
}

/// Trait objects can't be bounded by more than one non-builtin trait.
pub trait ReadSeek : Read + Seek + Send {}

impl<T: Read + Seek + Send> ReadSeek for T {}

/// Data stored in the compiler cache.
pub struct CacheRead {
    zip: ZipArchive<Box<ReadSeek>>,
}

impl CacheRead {
    /// Create a cache entry from `reader`.
    pub fn from<R>(reader: R) -> Result<CacheRead>
        where R: ReadSeek + 'static,
    {
        let z = ZipArchive::new(Box::new(reader) as Box<ReadSeek>).chain_err(|| {
            "Failed to parse cache entry"
        })?;
        Ok(CacheRead {
            zip: z,
        })
    }

    /// Get an object from this cache entry at `name` and write it to `to`.
    /// If the file has stored permissions, return them.
    pub fn get_object<T>(&mut self, name: &str, to: &mut T) -> Result<Option<u32>>
        where T: Write,
    {
        let mut file = self.zip.by_name(name).chain_err(|| {
            "Failed to read object from cache entry"
        })?;
        io::copy(&mut file, to)?;
        Ok(file.unix_mode())
    }
}

/// Data to be stored in the compiler cache.
pub struct CacheWrite {
    zip: ZipWriter<io::Cursor<Vec<u8>>>,
}

impl CacheWrite {
    /// Create a new, empty cache entry.
    pub fn new() -> CacheWrite
    {
        CacheWrite {
            zip: ZipWriter::new(io::Cursor::new(vec!())),
        }
    }

    /// Add an object containing the contents of `from` to this cache entry at `name`.
    /// If `mode` is `Some`, store the file entry with that mode.
    pub fn put_object<T>(&mut self, name: &str, from: &mut T, mode: Option<u32>) -> Result<()>
        where T: Read,
    {
        let opts = FileOptions::default().compression_method(CompressionMethod::Deflated);
        let opts = if let Some(mode) = mode { opts.unix_permissions(mode) } else { opts };
        self.zip.start_file(name, opts).chain_err(|| {
            "Failed to start cache entry object"
        })?;
        io::copy(from, &mut self.zip)?;
        Ok(())
    }

    /// Finish writing data to the cache entry writer, and return the data.
    pub fn finish(self) -> Result<Vec<u8>>
    {
        let CacheWrite { mut zip } = self;
        let cur = zip.finish().chain_err(|| "Failed to finish cache entry zip")?;
        Ok(cur.into_inner())
    }
}

/// An interface to cache storage.
pub trait Storage {
    /// Get a cache entry by `key`.
    ///
    /// If an error occurs, this method should return a `Cache::Error`.
    /// If nothing fails but the entry is not found in the cache,
    /// it should return a `Cache::Miss`.
    /// If the entry is successfully found in the cache, it should
    /// return a `Cache::Hit`.
    fn get(&self, key: &str) -> SFuture<Cache>;

    /// Put `entry` in the cache under `key`.
    ///
    /// Returns a `Future` that will provide the result or error when the put is
    /// finished.
    fn put(&self, key: &str, entry: CacheWrite) -> SFuture<Duration>;

    /// Get the storage location.
    fn location(&self) -> String;

    /// Get the current storage usage, if applicable.
    fn current_size(&self) -> Option<usize>;

    /// Get the maximum storage size, if applicable.
    fn max_size(&self) -> Option<usize>;
}

/// Get a suitable `Storage` implementation from the environment.
pub fn storage_from_environment(pool: &CpuPool, _handle: &Handle) -> Arc<Storage> {
    use config::CacheType;
    match CONFIG.cache_type {
        CacheType::S3(ref c) => {
            if cfg!(feature = "s3") {
                debug!("Trying S3Cache({})", c.endpoint);
                #[cfg(feature = "s3")]
                match S3Cache::new(&c.bucket, &c.endpoint, _handle) {
                    Ok(s) => {
                        trace!("Using S3Cache");
                        return Arc::new(s);
                    }
                    Err(e) => warn!("Failed to create S3Cache: {:?}", e),
                }
            } else {
                warn!("S3 cache selected by config, but s3 feature was not built!");
            }
        },

        CacheType::Redis(ref c) => {
            if cfg!(feature = "redis") {
                debug!("Trying Redis({})", c.url);
                #[cfg(feature = "redis")]
                match RedisCache::new(&c.url, pool) {
                    Ok(s) => {
                        trace!("Using Redis: {}", url);
                        return Arc::new(s);
                    }
                    Err(e) => warn!("Failed to create RedisCache: {:?}", e),
                }
            } else {
                warn!("Redis cache selected by config, but redis feature was not built!");
            }
        },

        CacheType::Disk(ref c) => {
            trace!("Using DiskCache({:?})", c.cache_dir);
            trace!("DiskCache size: {}", c.cache_size);
            return Arc::new(DiskCache::new(&c.cache_dir, c.cache_size, pool))
        },

        CacheType::Invalid => {
            panic!("Somehow got here with uninitialized CONFIG!");
        },
    }

    // Fall through to default disk cache
    let dir = config::default_disk_cache_dir();
    trace!("Using fallback DiskCache! ({:?})", dir);
    return Arc::new(DiskCache::new(&dir, 10 * 1024 * 1024 * 1024, pool));
}

