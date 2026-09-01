//! Unix-socket control plane for the process-resident stratum cache.

use crate::arg::Args;
use crate::cache::{CacheRunStats, CacheStateStats, StrataCache};
use crate::runner::run_cached;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum DaemonRequest {
    Reload,
    Stats,
    Shutdown,
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
    let mut cache = StrataCache::new(args.cache_max_bytes());
    let mut last_run = None;
    info!(
        "FlowLog cache daemon listening on {} (limit {} bytes)",
        socket_path.display(),
        args.cache_max_bytes()
    );

    for connection in listener.incoming() {
        let mut stream = match connection {
            Ok(stream) => stream,
            Err(error) => {
                warn!("cache daemon accept failed: {error}");
                continue;
            }
        };
        let request = read_request(&stream);
        let (response, shutdown) = match request {
            Ok(DaemonRequest::Reload) => {
                let result =
                    catch_unwind(AssertUnwindSafe(|| run_cached(args.clone(), &mut cache)));
                match result {
                    Ok(run) => {
                        last_run = Some(run.clone());
                        (
                            DaemonResponse {
                                ok: true,
                                command: "reload".to_string(),
                                run: Some(run),
                                cache: cache.state_stats(),
                                error: None,
                            },
                            false,
                        )
                    }
                    Err(panic) => (
                        DaemonResponse {
                            ok: false,
                            command: "reload".to_string(),
                            run: None,
                            cache: cache.state_stats(),
                            error: Some(panic_message(panic)),
                        },
                        false,
                    ),
                }
            }
            Ok(DaemonRequest::Stats) => (
                DaemonResponse {
                    ok: true,
                    command: "stats".to_string(),
                    run: last_run.clone(),
                    cache: cache.state_stats(),
                    error: None,
                },
                false,
            ),
            Ok(DaemonRequest::Shutdown) => (
                DaemonResponse {
                    ok: true,
                    command: "shutdown".to_string(),
                    run: last_run.clone(),
                    cache: cache.state_stats(),
                    error: None,
                },
                true,
            ),
            Err(error) => (
                DaemonResponse {
                    ok: false,
                    command: "invalid".to_string(),
                    run: None,
                    cache: cache.state_stats(),
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
