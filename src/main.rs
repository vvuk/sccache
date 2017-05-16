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

extern crate app_dirs;
extern crate bincode;
extern crate byteorder;
#[cfg(feature = "chrono")]
extern crate chrono;
#[macro_use]
extern crate clap;
#[cfg(feature = "rust-crypto")]
extern crate crypto;
#[cfg(unix)]
extern crate daemonize;
extern crate env_logger;
#[macro_use]
extern crate error_chain;
extern crate filetime;
#[macro_use]
extern crate futures;
extern crate futures_cpupool;
#[cfg(feature = "hyper")]
extern crate hyper;
#[cfg(feature = "hyper-tls")]
extern crate hyper_tls;
#[cfg(windows)]
extern crate kernel32;
extern crate local_encoding;
#[macro_use]
extern crate log;
extern crate lru_disk_cache;
extern crate fern;
#[cfg(test)]
extern crate itertools;
#[macro_use]
extern crate lazy_static;
extern crate libc;
#[cfg(windows)]
extern crate mio_named_pipes;
extern crate number_prefix;
extern crate ring;
#[cfg(feature = "redis")]
extern crate redis;
extern crate regex;
extern crate retry;
extern crate rustc_serialize;
extern crate serde_json;
#[macro_use]
extern crate serde_derive;
extern crate tempdir;
extern crate time;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_process;
extern crate tokio_proto;
extern crate tokio_service;
extern crate tokio_serde_bincode;
extern crate toml;
extern crate uuid;
#[cfg(windows)]
extern crate winapi;
extern crate which;
extern crate zip;

// To get macros in scope, this has to be first.
#[cfg(test)]
#[macro_use]
mod test;

#[macro_use]
mod errors;

mod cache;
mod client;
mod cmdline;
mod commands;
mod compiler;
mod config;
mod mock_command;
mod protocol;
mod server;
#[cfg(feature = "simple-s3")]
mod simples3;
mod util;
mod env_splitter;
mod osstringext;

use std::env;
use std::io::Write;

fn main() {
    init_logging();
    std::process::exit(match cmdline::parse() {
        Ok(cmd) => {
            //println!("Cache config: {:?}", *config::CONFIG);
            match commands::run_command(cmd) {
                Ok(s) => s,
                Err(e) =>  {
                    let stderr = &mut std::io::stderr();
                    writeln!(stderr, "error: {}", e).unwrap();

                    for e in e.iter().skip(1) {
                        writeln!(stderr, "caused by: {}", e).unwrap();
                    }
                    2
                }
            }
        }
        Err(e) => {
            println!("sccache: {}", e);
            cmdline::get_app().print_help().unwrap();
            println!("");
            1
        }
    });
}

fn init_logging() {
    match if env::var("RUST_LOG").is_ok() {
        env_logger::init()
            .map_err(|e| format!("{:?}", e))
    } else {
        match env::var("SCCACHE_LOG_LEVEL") {
            Ok(log_level) => {
                let log_level = match &*log_level.to_lowercase() {
                    "off" => log::LogLevelFilter::Off,
                    "trace" => log::LogLevelFilter::Trace,
                    "debug" => log::LogLevelFilter::Debug,
                    "info" => log::LogLevelFilter::Info,
                    "warn" => log::LogLevelFilter::Warn,
                    "error" => log::LogLevelFilter::Error,
                    _ => panic!("Invalid log level {}", log_level),
                };

                let logger_config = fern::DispatchConfig {
                    format: Box::new(|msg: &str, level: &log::LogLevel, _location: &log::LogLocation| {
                        format!("[{}][{}] {}", time::now().strftime("%Y-%m-%d][%H:%M:%S").unwrap(), level, msg)
                    }),
                    //TODO: only the server process should output to the log file.
                    output: vec![fern::OutputConfig::stdout(), fern::OutputConfig::file("sccache.log")],
                    level: log::LogLevelFilter::Trace,
                };
                fern::init_global_logger(logger_config, log_level)
                    .map_err(|e| format!("{:?}", e))
            },
            Err(_) => Ok(()),
        }
    } {
        Ok(_) => (),
        Err(e) => panic!(format!("Failed to initalize logging: {}", e)),
    }
}
