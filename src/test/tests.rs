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

use ::cache::disk::DiskCache;
use ::client::{
    connect_to_server,
};
use ::commands::{
    do_compile,
    request_shutdown,
    request_stats,
};
use env_logger;
use futures::sync::oneshot::{self, Sender};
use futures_cpupool::CpuPool;
use ::mock_command::*;
use ::server::{
    ServerMessage,
    SccacheServer,
};
use std::fs::File;
use std::io::{
    Cursor,
    Write,
};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc,Mutex,mpsc};
use std::thread;
use std::time::Duration;
use std::usize;
use test::utils::*;
use tokio_core::reactor::Core;

/// Options for running the server in tests.
#[derive(Default)]
struct ServerOptions {
    /// The server's idle shutdown timeout.
    idle_timeout: Option<u64>,
    /// The maximum size of the disk cache.
    cache_size: Option<usize>,
}

/// Run a server on a background thread, and return a tuple of useful things.
///
/// * The port on which the server is listening.
/// * A `Sender` which can be used to send messages to the server.
///   (Most usefully, ServerMessage::Shutdown.)
/// * An `Arc`-and-`Mutex`-wrapped `MockCommandCreator` which the server will
///   use for all process creation.
/// * The `JoinHandle` for the server thread.
fn run_server_thread<T>(cache_dir: &Path, options: T)
                        -> (u16, Sender<ServerMessage>, Arc<Mutex<MockCommandCreator>>, thread::JoinHandle<()>)
    where T: Into<Option<ServerOptions>> + Send + 'static
{
    let options = options.into();
    let cache_dir = cache_dir.to_path_buf();

    let cache_size = options.as_ref()
                            .and_then(|o| o.cache_size.as_ref())
                            .map(|s| *s)
                            .unwrap_or(usize::MAX);
    let pool = CpuPool::new(1);
    let storage = Arc::new(DiskCache::new_for_testing(&cache_dir, cache_size, &pool));

    // Create a server on a background thread, get some useful bits from it.
    let (tx, rx) = mpsc::channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = thread::spawn(move || {
        let core = Core::new().unwrap();
        let srv = SccacheServer::new(0, pool, core, storage).unwrap();
        let mut srv: SccacheServer<Arc<Mutex<MockCommandCreator>>> = srv;
        assert!(srv.port() > 0);
        if let Some(options) = options {
            if let Some(timeout) = options.idle_timeout {
                 srv.set_idle_timeout(Duration::from_millis(timeout));
            }
        }
        let port = srv.port();
        let creator = srv.command_creator().clone();
        tx.send((port, creator)).unwrap();
        srv.run(shutdown_rx).unwrap();
    });
    let (port, creator) = rx.recv().unwrap();
    (port, shutdown_tx, creator, handle)
}

#[test]
fn test_server_shutdown() {
    let f = TestFixture::new();
    let (port, _sender, _storage, child) = run_server_thread(&f.tempdir.path(), None);
    // Connect to the server.
    let conn = connect_to_server(port).unwrap();
    // Ask it to shut down
    request_shutdown(conn).unwrap();
    // Ensure that it shuts down.
    child.join().unwrap();
}

#[test]
fn test_server_idle_timeout() {
    let f = TestFixture::new();
    // Set a ridiculously low idle timeout.
    let (_port, _sender, _storage, child) = run_server_thread(&f.tempdir.path(), ServerOptions { idle_timeout: Some(1), .. Default::default() });
    // Don't connect to it.
    // Ensure that it shuts down.
    // It would be nice to have an explicit timeout here so we don't hang
    // if something breaks...
    child.join().unwrap();
}

#[test]
fn test_server_stats() {
    let f = TestFixture::new();
    let (port, sender, _storage, child) = run_server_thread(&f.tempdir.path(), None);
    // Connect to the server.
    let conn = connect_to_server(port).unwrap();
    // Ask it for stats.
    let info = request_stats(conn).unwrap();
    assert_eq!(0, info.stats.compile_requests);
    // Now signal it to shut down.
    sender.send(ServerMessage::Shutdown).ok().unwrap();
    // Ensure that it shuts down.
    child.join().unwrap();
}

#[test]
fn test_server_unsupported_compiler() {
    let f = TestFixture::new();
    let (port, sender, server_creator, child) = run_server_thread(&f.tempdir.path(), None);
    // Connect to the server.
    let conn = connect_to_server(port).unwrap();
    {
        let mut c = server_creator.lock().unwrap();
        // The server will check the compiler, so pretend to be an unsupported
        // compiler.
        c.next_command_spawns(Ok(MockChild::new(exit_status(0), "hello", "error")));
    }
    // Ask the server to compile something.
    //TODO: MockCommand should validate these!
    let exe = &f.bins[0];
    let cmdline = vec!["-c".into(), "file.c".into(), "-o".into(), "file.o".into()];
    let cwd = f.tempdir.path();
    let client_creator = new_creator();
    const COMPILER_STDOUT: &'static [u8] = b"some stdout";
    const COMPILER_STDERR: &'static [u8] = b"some stderr";
    {
        let mut c = client_creator.lock().unwrap();
        // Actual client output.
        c.next_command_spawns(Ok(MockChild::new(exit_status(0), COMPILER_STDOUT, COMPILER_STDERR)));
    }
    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());
    let path = Some(f.paths);
    let mut core = Core::new().unwrap();
    assert_eq!(0, do_compile(client_creator.clone(), &mut core, conn, exe, cmdline, cwd, path, vec![], &mut stdout, &mut stderr).unwrap());
    // Make sure we ran the mock processes.
    assert_eq!(0, server_creator.lock().unwrap().children.len());
    assert_eq!(0, client_creator.lock().unwrap().children.len());
    assert_eq!(COMPILER_STDOUT, &stdout.into_inner()[..]);
    assert_eq!(COMPILER_STDERR, &stderr.into_inner()[..]);
    // Shut down the server.
    sender.send(ServerMessage::Shutdown).ok().unwrap();
    // Ensure that it shuts down.
    child.join().unwrap();
}

#[test]
fn test_server_compile() {
    match env_logger::init() {
        Ok(_) => {},
        Err(_) => {},
    }
    let f = TestFixture::new();
    let (port, sender, server_creator, child) = run_server_thread(&f.tempdir.path(), None);
    // Connect to the server.
    const PREPROCESSOR_STDOUT : &'static [u8] = b"preprocessor stdout";
    const PREPROCESSOR_STDERR : &'static [u8] = b"preprocessor stderr";
    const STDOUT : &'static [u8] = b"some stdout";
    const STDERR : &'static [u8] = b"some stderr";
    let conn = connect_to_server(port).unwrap();
    {
        let mut c = server_creator.lock().unwrap();
        // The server will check the compiler. Pretend it's GCC.
        c.next_command_spawns(Ok(MockChild::new(exit_status(0), "gcc", "")));
        // Preprocessor invocation.
        c.next_command_spawns(Ok(MockChild::new(exit_status(0), PREPROCESSOR_STDOUT, PREPROCESSOR_STDERR)));
        // Compiler invocation.
        //TODO: wire up a way to get data written to stdin.
        let obj = f.tempdir.path().join("file.o");
        c.next_command_calls(move |_| {
            // Pretend to compile something.
            match File::create(&obj)
                .and_then(|mut f| f.write_all(b"file contents")) {
                    Ok(_) => Ok(MockChild::new(exit_status(0), STDOUT, STDERR)),
                    Err(e) => Err(e),
                }
        });
    }
    // Ask the server to compile something.
    //TODO: MockCommand should validate these!
    let exe = &f.bins[0];
    let cmdline = vec!["-c".into(), "file.c".into(), "-o".into(), "file.o".into()];
    let cwd = f.tempdir.path();
    // This creator shouldn't create any processes. It will assert if
    // it tries to.
    let client_creator = new_creator();
    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());
    let path = Some(f.paths);
    let mut core = Core::new().unwrap();
    assert_eq!(0, do_compile(client_creator.clone(), &mut core, conn, exe, cmdline, cwd, path, vec![], &mut stdout, &mut stderr).unwrap());
    // Make sure we ran the mock processes.
    assert_eq!(0, server_creator.lock().unwrap().children.len());
    assert_eq!(STDOUT, stdout.into_inner().as_slice());
    assert_eq!(STDERR, stderr.into_inner().as_slice());
    // Shut down the server.
    sender.send(ServerMessage::Shutdown).ok().unwrap();
    // Ensure that it shuts down.
    child.join().unwrap();
}

#[test]
fn test_server_port_in_use() {
    // Bind an arbitrary free port.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let sccache = find_sccache_binary();
    let output = Command::new(&sccache)
        .arg("--start-server")
        .env("SCCACHE_SERVER_PORT", listener.local_addr().unwrap().port().to_string())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let s = String::from_utf8_lossy(&output.stderr);
    assert!(s.contains("Server startup failed:"),
            "Output did not contain 'Failed to start server:':\n========\n{}\n========",
            s);
}
