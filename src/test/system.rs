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

#![allow(dead_code, unused_imports)]

extern crate gcc;

use client::connect_with_retry;
use env_logger;
use log::LogLevel::Trace;
use serde_json;
use server::ServerInfo;
use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr,OsString};
use std::fmt;
use std::fs;
use std::io::{
    self,
    Write,
};
use std::path::{Path,PathBuf};
use std::process::{
    Command,
    Output,
    Stdio,
};
use tempdir::TempDir;
use test::utils::*;
use which::which_in;


struct Compiler {
    pub name: &'static str,
    pub exe: OsString,
    pub env_vars: Vec<(OsString, OsString)>,
}

// Test GCC + clang on non-OS X platforms.
#[cfg(all(unix, not(target_os="macos")))]
const COMPILERS: &'static [&'static str] = &["gcc", "clang"];

// OS X ships a `gcc` that's just a clang wrapper, so only test clang there.
#[cfg(target_os="macos")]
const COMPILERS: &'static [&'static str] = &["clang"];

//TODO: could test gcc when targeting mingw.

fn do_run<T: AsRef<OsStr>>(exe: &Path, args: &[T], cwd: &Path, env_vars: &[(OsString, OsString)]) -> Output {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for &(ref k, ref v) in env_vars.iter() {
        cmd.env(k, v);
    }
    trace!("do_run: {:?}", cmd);
    cmd.spawn()
        .unwrap_or_else(|e| { panic!("failed to execute child: {}", e) })
        .wait_with_output()
        .unwrap()
}

fn run_stdout<T>(exe: &Path, args: &[T], cwd: &Path, env_vars: &[(OsString, OsString)]) -> String
    where T: AsRef<OsStr> + fmt::Debug,
{
    let output = do_run(exe, args, cwd, env_vars);
    assert!(output.status.success(), "Failed to run {:?} {:?}", exe, args);
    String::from_utf8(output.stdout).expect("Couldn't convert stdout to String")
}

fn run<T: AsRef<OsStr>>(exe: &Path, args: &[T], cwd: &Path, env_vars: &[(OsString, OsString)]) -> bool {
    let output = do_run(exe, args, cwd, env_vars);
    if output.status.success() {
        true
    } else {
        let va = args.iter().map(|a| a.as_ref().to_str().unwrap()).collect::<Vec<_>>();
        println!("Process `{:?} {}` failed:", exe, va.join(" "));
        print!("stdout: `");
        io::stdout().write(&output.stdout).unwrap();
        println!("`");
        print!("stderr: `");
        io::stdout().write(&output.stderr).unwrap();
        println!("`");
        false
    }
}

macro_rules! vec_from {
    ( $t:ty, $( $x:expr ),* ) => {
        vec!($( Into::<$t>::into(&$x), )*)
    };
}

fn compile_cmdline<T: AsRef<OsStr>>(compiler: &str, exe: T, input: &str, output: &str) -> Vec<OsString> {
    match compiler {
        "gcc" | "clang" => vec_from!(OsString, exe.as_ref(), "-c", input, "-o", output),
        "cl.exe" => vec_from!(OsString, exe, "-c", input, format!("-Fo{}", output)),
        _ => panic!("Unsupported compiler: {}", compiler),
    }
}

fn get_stats(sccache: &Path, cwd: &Path) -> ServerInfo {
    let output = run_stdout(sccache, &["--show-stats", "--stats-format=json"], cwd, &[]);
    serde_json::from_str(&output).expect("Failed to parse JSON stats")
}

fn run_sccache_command_test(sccache: &Path, compiler: Compiler, tempdir: &Path) {
    let Compiler { name, exe, env_vars } = compiler;
    // Ensure there's no existing sccache server running.
    trace!("stop server");
    do_run(sccache, &["--stop-server"], tempdir, &[]);
    // Create a subdir for the cache.
    let cache = tempdir.join("cache");
    fs::create_dir_all(&cache).unwrap();
    // Start a server.
    trace!("start server");
    // Don't run this with run() because on Windows `wait_with_output`
    // will hang because the internal server process is not detached.
    assert!(Command::new(sccache)
            .arg("--start-server")
            .current_dir(tempdir)
            .env("SCCACHE_DIR", &cache)
            .status()
            .unwrap()
            .success());
    trace!("run_sccache_command_test: {}", name);
    // Compile a source file.
    let original_source_file = Path::new(file!()).parent().unwrap().join("test.c");
    // Copy the source file into the tempdir so we can compile with relative paths, since the commandline winds up in the hash key.
    let source_file = tempdir.join("test.c");
    trace!("fs::copy({:?}, {:?})", original_source_file, source_file);
    fs::copy(&original_source_file, &source_file).unwrap();
    let out_file = tempdir.join("test.o");
    let input = source_file.file_name().unwrap().to_str().unwrap();
    let output = out_file.file_name().unwrap().to_str().unwrap();
    trace!("zero stats");
    assert_eq!(true, run(sccache, &["-z"], tempdir, &[]));
    trace!("compile");
    assert_eq!(true, run(sccache, &compile_cmdline(name, &exe, &input, &output), tempdir, &env_vars));
    assert_eq!(true, fs::metadata(&out_file).and_then(|m| Ok(m.len() > 0)).unwrap());
    trace!("request stats");
    let info = get_stats(sccache, tempdir);
    assert_eq!(1, info.stats.compile_requests);
    assert_eq!(1, info.stats.requests_executed);
    assert_eq!(0, info.stats.cache_hits);
    assert_eq!(1, info.stats.cache_misses);
    trace!("compile");
    fs::remove_file(&out_file).unwrap();
    assert_eq!(true, run(sccache, &compile_cmdline(name, &exe, &input, &output), tempdir, &env_vars));
    assert_eq!(true, fs::metadata(&out_file).and_then(|m| Ok(m.len() > 0)).unwrap());
    trace!("request stats");
    let info = get_stats(sccache, tempdir);
    assert_eq!(2, info.stats.compile_requests);
    assert_eq!(2, info.stats.requests_executed);
    assert_eq!(1, info.stats.cache_hits);
    assert_eq!(1, info.stats.cache_misses);
    trace!("stop server");
    assert_eq!(true, run(sccache, &["--stop-server"], tempdir, &[]));
}

#[cfg(unix)]
fn find_compilers() -> Vec<Compiler> {
    let cwd = env::current_dir().unwrap();
    COMPILERS.iter()
        .filter_map(|c| {
            match which_in(c, env::var_os("PATH"), &cwd) {
                Ok(full_path) => match full_path.canonicalize() {
                    Ok(full_path_canon) => Some(Compiler {
                        name: *c,
                        exe: full_path_canon.into_os_string(),
                        env_vars: vec![],
                    }),
                    Err(_) => None,
                },
                Err(_) => None,
            }
        })
        .collect::<Vec<_>>()
}

#[cfg(target_env="msvc")]
fn find_compilers() -> Vec<Compiler> {
    let tool = gcc::Config::new()
        .opt_level(1)
        .host("x86_64-pc-windows-msvc")
        .target("x86_64-pc-windows-msvc")
        .debug(false)
        .get_compiler();
    vec![
        Compiler {
            name: "cl.exe",
            exe: tool.path().as_os_str().to_os_string(),
            env_vars: tool.env().to_vec(),
        },
    ]
}

#[test]
#[cfg(any(unix, target_env="msvc"))]
fn test_sccache_command() {
    match env_logger::init() {
        Ok(_) => {},
        Err(_) => {},
    }
    let tempdir = TempDir::new("sccache_system_test").unwrap();
    let sccache = find_sccache_binary();
    let compilers = find_compilers();
    if compilers.is_empty() {
        warn!("No compilers found, skipping test");
    } else {
        for compiler in compilers {
            run_sccache_command_test(&sccache, compiler, tempdir.path())
        }
    }
}
