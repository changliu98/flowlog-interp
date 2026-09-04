//! Unix-socket control plane for the process-resident state cache.
//!
//! One daemon serves any number of programs at once: a `reload` names the
//! program, fact directory and output directory it is about (falling back to
//! the ones the daemon was started with), runs on its own thread, and shares
//! the cache with every other reload in flight.  Lookups and inserts hold the
//! cache lock for the length of a hash-map operation; evaluation never does.

use crate::arg::Args;
use crate::cache::{CacheRunStats, CacheStateStats, StrataCache};
use crate::runner::{lock, run_cached};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use tracing::{info, warn};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum DaemonRequest {
    /// Evaluate one program, reading along the cache.  Each path defaults to
    /// the one the daemon was started with.
    Reload {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        facts: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        csvs: Option<String>,
    },
    Stats,
    Shutdown,
}

impl DaemonRequest {
    pub fn reload() -> Self {
        Self::Reload {
            program: None,
            facts: None,
            csvs: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DaemonResponse {
    pub ok: bool,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<CacheRunStats>,
    pub cache: CacheStateStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn serve(args: Args, socket_path: PathBuf) -> io::Result<()> {
    if socket_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "refusing to replace existing daemon socket {}",
                socket_path.display()
            ),
        ));
    }

    let listener = UnixListener::bind(&socket_path)?;
    let _socket_guard = SocketGuard(socket_path.clone());
    let cache = Arc::new(Mutex::new(StrataCache::from_args(&args)));
    let last_run: Arc<Mutex<Option<CacheRunStats>>> = Arc::new(Mutex::new(None));
    let mut in_flight: Vec<JoinHandle<()>> = Vec::new();
    info!(
        "FlowLog cache daemon listening on {} (memory limit {} bytes{})",
        socket_path.display(),
        args.cache_max_bytes(),
        args.cache_dir()
            .map(|directory| format!(", state store {}", directory.display()))
            .unwrap_or_default()
    );

    for connection in listener.incoming() {
        let mut stream = match connection {
            Ok(stream) => stream,
            Err(error) => {
                warn!("cache daemon accept failed: {error}");
                continue;
            }
        };
        in_flight.retain(|handle| !handle.is_finished());

        let request = read_request(&stream);
        let (response, shutdown) = match request {
            Ok(DaemonRequest::Reload {
                program,
                facts,
                csvs,
            }) => {
                let run_args = args.with_paths(program, facts, csvs);
                let cache = Arc::clone(&cache);
                let last_run = Arc::clone(&last_run);
                in_flight.push(std::thread::spawn(move || {
                    let result = catch_unwind(AssertUnwindSafe(|| run_cached(run_args, &cache)));
                    let response = match result {
                        Ok(run) => {
                            *last_run.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                Some(run.clone());
                            DaemonResponse {
                                ok: true,
                                command: "reload".to_string(),
                                run: Some(run),
                                cache: lock(&cache).state_stats(),
                                error: None,
                            }
                        }
                        Err(panic) => DaemonResponse {
                            ok: false,
                            command: "reload".to_string(),
                            run: None,
                            cache: lock(&cache).state_stats(),
                            error: Some(panic_message(panic)),
                        },
                    };
                    if let Err(error) = write_response(&mut stream, &response) {
                        warn!("cache daemon could not write a response: {error}");
                    }
                }));
                continue;
            }
            Ok(DaemonRequest::Stats) => (
                DaemonResponse {
                    ok: true,
                    command: "stats".to_string(),
                    run: last_run
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone(),
                    cache: lock(&cache).state_stats(),
                    error: None,
                },
                false,
            ),
            Ok(DaemonRequest::Shutdown) => (
                DaemonResponse {
                    ok: true,
                    command: "shutdown".to_string(),
                    run: last_run
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone(),
                    cache: lock(&cache).state_stats(),
                    error: None,
                },
                true,
            ),
            Err(error) => (
                DaemonResponse {
                    ok: false,
                    command: "invalid".to_string(),
                    run: None,
                    cache: lock(&cache).state_stats(),
                    error: Some(error.to_string()),
                },
                false,
            ),
        };

        if let Err(error) = write_response(&mut stream, &response) {
            warn!("cache daemon could not write a response: {error}");
        }
        if shutdown {
            break;
        }
    }

    for handle in in_flight {
        let _ = handle.join();
    }
    Ok(())
}

fn write_response(stream: &mut UnixStream, response: &DaemonResponse) -> io::Result<()> {
    serde_json::to_writer(&mut *stream, response)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

pub fn request(socket_path: &Path, request: DaemonRequest) -> io::Result<String> {
    let mut stream = UnixStream::connect(socket_path)?;
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    Ok(response)
}

fn read_request(stream: &UnixStream) -> io::Result<DaemonRequest> {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid daemon request: {error}"),
        )
    })
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_string()
    } else {
        "reload panicked without a text message".to_string()
    }
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
