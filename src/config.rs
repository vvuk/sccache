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

use app_dirs::{
    AppDataType,
    AppInfo,
    app_dir,
};
use regex::Regex;
use std::env;
use std::io::Read;
use std::fs::File;
use std::path::PathBuf;
use std::str::FromStr;
use toml;

lazy_static! {
    pub static ref CONFIG: Config = { Config::create() };
}

//TODO: might need to put this somewhere more central
const APP_INFO: AppInfo = AppInfo {
    name: "sccache",
    author: "Mozilla",
};

const TEN_GIGS: usize = 10 * 1024 * 1024 * 1024;

pub fn default_disk_cache_dir() -> PathBuf {
    app_dir(AppDataType::UserCache, &APP_INFO, "")
        // Fall back to something, even if it's not very good.
        .unwrap_or(env::temp_dir().join("sccache_cache"))
}

fn parse_size(val: &str) -> Option<usize> {
    let re = Regex::new(r"^(\d+)([KMGT])$").unwrap();
    re.captures(val)
        .and_then(|caps| caps.at(1).and_then(|size| usize::from_str(size).ok()).and_then(|size| Some((size, caps.at(2)))))
        .and_then(|(size, suffix)| {
            match suffix {
                Some("K") => Some(1024 * size),
                Some("M") => Some(1024 * 1024 * size),
                Some("G") => Some(1024 * 1024 * 1024 * size),
                Some("T") => Some(1024 * 1024 * 1024 * 1024 * size),
                _ => None,
            }
        })
}

#[derive(Debug, PartialEq)]
pub struct DiskCacheConfig {
    pub cache_dir: PathBuf,
    pub cache_size: usize,
}

#[derive(Debug, PartialEq)]
pub struct RedisCacheConfig {
    pub url: String,
}

#[derive(Debug, PartialEq)]
pub struct S3CacheConfig {
    pub endpoint: String,
    pub bucket: String,
}

#[derive(Debug, PartialEq)]
pub enum CacheType {
    Invalid, // internal
    Disk(DiskCacheConfig),
    S3(S3CacheConfig),
    Redis(RedisCacheConfig),
}

#[derive(Debug)]
pub struct Config {
    pub cache_type: CacheType,
    pub no_daemon: bool,
    pub force_recache: bool,
    pub msvc_force_z7: bool,
    pub compiler_dir: Option<PathBuf>,
}

impl Config {
    pub fn create() -> Config {
        // read from SCCACHE_CONF if present, otherwise
        // from ~/.sccache if present.
        let conf_data = env::var("SCCACHE_CONF").ok()
            .and_then(|env_path| Some(PathBuf::from(env_path)))
            .or_else(|| env::home_dir().map(|d| d.join(".sccache")))
            .and_then(|path| File::open(path).ok())
            .map(|mut file| {
                let mut data = String::new();
                file.read_to_string(&mut data).unwrap();
                data.parse::<toml::Value>().expect("error parsing sccache config")
            })
            .unwrap_or_else(|| "".parse::<toml::Value>().unwrap());

        let mut conf = Config {
            cache_type: CacheType::Invalid,
            no_daemon: false,
            force_recache: false,
            msvc_force_z7: false,
            compiler_dir: None,
        };

        let string_from_config = |conf_name: &str| -> Option<&str> {
            conf_data.get(conf_name).and_then(|v| v.as_str())
        };

        let usize_from_config = |conf_name: &str| -> Option<usize> {
            conf_data.get(conf_name).and_then(|v| v.as_str()).and_then(|v| parse_size(&v))
        };

        let bool_from_config = |conf_name: &str| -> Option<bool> {
            conf_data.get(conf_name).and_then(|v| v.as_bool())
        };

        fn string_from_env(env_name: &str) -> Option<String> {
            env::var(env_name).ok()
        }

        fn bool_from_env(env_name: &str) -> Option<bool> {
            env::var(env_name).ok().map(|v| v != "0")
        }

        fn usize_from_env(env_name: &str) -> Option<usize> {
            env::var(env_name).ok().and_then(|v| parse_size(&v))
        }

        //println!("cache_type: {:?}", conf_data.get("cache_type"));
        // "SCCACHE_DIR"
        // "SCCACHE_REDIS"
        // "SCCACHE_BUCKET"
        // "SCCACHE_ENDPOINT"
        // "SCCACHE_REGION"

        //println!("Cache type from config: {:?}", conf_data.get("cache_type"));

        conf.cache_type = match conf_data.get("cache_type").and_then(|s| s.as_str()) {
            None => CacheType::Invalid,
            Some("disk") => {
                let cache_dir = string_from_config("cache_dir")
                    .map(|s| PathBuf::from(s))
                    .unwrap_or_else(|| default_disk_cache_dir());
                CacheType::Disk(DiskCacheConfig { cache_dir: cache_dir, cache_size: TEN_GIGS })
            },
            Some("redis") => {
                let redis_url = string_from_config("redis_url").expect("missing redis_url for redis cache");
                CacheType::Redis(RedisCacheConfig { url: redis_url.to_owned() })
            },
            Some("s3") => {
                let s3_bucket = string_from_config("s3_bucket").expect("missing s3_bucket in config");
                let s3_endpoint = string_from_config("s3_endpoint").expect("missing s3_endpoint in config");
                CacheType::S3(S3CacheConfig {
                    bucket: s3_bucket.to_owned(),
                    endpoint: s3_endpoint.to_owned(),
                })
            },
            Some(s) => {
                panic!("cache_type must be 'disk', 'redis', or 's3' (got '{}')", s);
            },
        };

        // Handle legacy env vars for cache type setup; don't add any more of these!

        if env::var("SCCACHE_REDIS").is_ok() {
            let redis_url = string_from_env("SCCACHE_REDIS").unwrap();
            conf.cache_type = CacheType::Redis(RedisCacheConfig { url: redis_url });
        } else if env::var("SCCACHE_BUCKET").is_ok() ||
            env::var("SCCACHE_ENDPOINT").is_ok() ||
            env::var("SCCACHE_REGION").is_ok()
        {
            if let Ok(bucket) = env::var("SCCACHE_BUCKET") {
                let endpoint = match env::var("SCCACHE_ENDPOINT") {
                    Ok(endpoint) => format!("{}/{}", endpoint, bucket),
                    _ => match env::var("SCCACHE_REGION") {
                        Ok(ref region) if region != "us-east-1" =>
                            format!("{}.s3-{}.amazonaws.com", bucket, region),
                        _ => format!("{}.s3.amazonaws.com", bucket),
                    },
                };

                conf.cache_type = CacheType::S3(S3CacheConfig {
                    bucket: bucket,
                    endpoint: endpoint,
                });
            }
        } else if conf.cache_type == CacheType::Invalid {
            let cache_dir = string_from_env("SCCACHE_DIR")
                .map(|s| PathBuf::from(s))
                .unwrap_or_else(|| default_disk_cache_dir());
            conf.cache_type = CacheType::Disk(DiskCacheConfig { cache_dir: cache_dir, cache_size: TEN_GIGS });
        }

        // Handle common conf/env var configs
        match conf.cache_type {
            CacheType::Disk(ref mut c) => {
                c.cache_size = usize_from_env("SCCACHE_SIZE")
                    .or_else(|| usize_from_config("cache_size"))
                    .unwrap_or(TEN_GIGS);
            }
            _ => {}
        }
                
        conf.no_daemon = bool_from_env("SCCACHE_NO_DAEMON").or(bool_from_config("no_daemon")).unwrap_or(false);
        conf.force_recache = bool_from_env("SCCACHE_RECACHE").or(bool_from_config("force_recache")).unwrap_or(false);
        conf.msvc_force_z7 = bool_from_config("msvc_force_z7").unwrap_or(false);

        if let Some(compiler_dir) = string_from_config("compiler_dir") {
            if !compiler_dir.ends_with("/") || !compiler_dir.ends_with("\\") {
                conf.compiler_dir = Some(PathBuf::from(String::from(compiler_dir) +"."));
            } else {
                conf.compiler_dir = Some(PathBuf::from(compiler_dir));
            }
        }
        conf
    }
}

#[test]
fn test_parse_size() {
    assert_eq!(None, parse_size(""));
    assert_eq!(None, parse_size("100"));
    assert_eq!(Some(2048), parse_size("2K"));
    assert_eq!(Some(10 * 1024 * 1024), parse_size("10M"));
    assert_eq!(Some(TEN_GIGS), parse_size("10G"));
    assert_eq!(Some(1024 * TEN_GIGS), parse_size("10T"));
}
