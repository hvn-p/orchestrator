//! The local API: `watch` serves what the read commands show, and the
//! events as they come, over HTTP on a Unix socket in the runtime
//! directory. Every read of the state goes through it, the CLI's included:
//! without `watch`, nothing answers.
//!
//! `GET /sessions?heads=true`, `/admission`, `/peaks?at_least_mb=&most=`,
//! `/machine` and `/config` answer the reports of `report` as JSON. `GET
//! /events` is a server-sent event stream (`text/event-stream`, HTML
//! Living Standard): each event comes as one `data:` line holding the JSON
//! line `events.jsonl` gets, and a comment line keeps an idle stream open.
//! An error answers JSON `{"error": "..."}`.

use crate::events::Hub;
use crate::report::{AdmissionReport, ConfigReport, PeaksReport, SessionsReport, State};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use system::machine::Machine;

/// The socket, in the runtime directory.
pub const SOCKET: &str = "api.sock";
/// The longest request head read.
const HEAD_MAX: usize = 8192;
/// Between two comment lines on an idle event stream.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

pub fn socket(runtime: &Path) -> PathBuf {
    runtime.join(SOCKET)
}

/// Listens at `path`, readable by this user only. Refuses when another
/// `watch` answers there; a socket left by one that ended is replaced.
pub fn bind(path: &Path) -> Result<UnixListener> {
    if UnixStream::connect(path).is_ok() {
        bail!("another orchestrator watch answers at {}", path.display());
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", path.display()));
        }
        _ => {}
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("listening at {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))?;
    Ok(listener)
}

/// Answers on `listener` from a thread of its own, each connection in a
/// thread too: an event stream stays open.
pub fn serve(listener: UnixListener, state: Arc<dyn State + Send + Sync>, hub: Hub) {
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (state, hub) = (Arc::clone(&state), hub.clone());
            std::thread::spawn(move || {
                if let Err(e) = answer(stream, state.as_ref(), &hub) {
                    eprintln!("orchestrator: api: {e:#}");
                }
            });
        }
    });
}

fn answer(mut stream: UnixStream, state: &dyn State, hub: &Hub) -> Result<()> {
    let target = match read_head(&mut stream)? {
        Head::Get(target) => target,
        // Connected and gone, as a `watch` checking whether one answers.
        Head::None => return Ok(()),
        Head::Other(method) => {
            return respond(
                &mut stream,
                405,
                &error(&format!("{method} is not allowed")),
            );
        }
    };
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let param = |name: &str| {
        query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    };
    let number = |name: &str| param(name).and_then(|v| v.parse::<u64>().ok());
    let body = match path {
        "/events" => return stream_events(stream, hub),
        "/sessions" => json(state.sessions(param("heads") == Some("true"))),
        "/admission" => json(state.admission()),
        "/peaks" => json(state.peaks(
            number("at_least_mb"),
            number("most").and_then(|n| usize::try_from(n).ok()),
        )),
        "/machine" => json(state.machine()),
        "/config" => json(state.config()),
        _ => return respond(&mut stream, 404, &error(&format!("no {path}"))),
    };
    match body {
        Ok(body) => respond(&mut stream, 200, &body),
        Err(e) => respond(&mut stream, 500, &error(&format!("{e:#}"))),
    }
}

enum Head {
    Get(String),
    Other(String),
    /// Closed before sending anything.
    None,
}

/// Reads a request's head, up to its blank line.
fn read_head(stream: &mut UnixStream) -> Result<Head> {
    let mut head = Vec::new();
    let mut byte = [0; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= HEAD_MAX {
            bail!("the request head exceeds {HEAD_MAX} bytes");
        }
        if stream.read(&mut byte)? == 0 {
            if head.is_empty() {
                return Ok(Head::None);
            }
            bail!("the request head ended early");
        }
        head.push(byte[0]);
    }
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    request.parse(&head).context("parsing the request")?;
    let method = request.method.unwrap_or_default();
    let target = request.path.unwrap_or_default().to_string();
    Ok(if method == "GET" {
        Head::Get(target)
    } else {
        Head::Other(method.to_string())
    })
}

fn json(value: Result<impl Serialize>) -> Result<String> {
    Ok(serde_json::to_string(&value?)?)
}

fn error(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

fn respond(stream: &mut UnixStream, status: u16, body: &str) -> Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    Ok(())
}

/// The event stream, until the client goes away.
fn stream_events(mut stream: UnixStream, hub: &Hub) -> Result<()> {
    let events = hub.subscribe();
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n",
    )?;
    loop {
        let chunk = match events.recv_timeout(KEEP_ALIVE) {
            Ok(line) => format!("data: {line}\n\n"),
            Err(RecvTimeoutError::Timeout) => ": keep-alive\n\n".to_string(),
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        if stream.write_all(chunk.as_bytes()).is_err() {
            return Ok(());
        }
    }
}

/// The state, asked of the `watch` that serves `runtime`.
pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn new(runtime: &Path) -> Client {
        Client {
            socket: socket(runtime),
        }
    }

    fn connect(&self) -> Result<UnixStream> {
        UnixStream::connect(&self.socket).map_err(|e| match e.kind() {
            ErrorKind::NotFound | ErrorKind::ConnectionRefused => anyhow!(
                "orchestrator watch is not running: start it with `orchestrator watch` (no answer at {})",
                self.socket.display()
            ),
            _ => anyhow!(e).context(format!("connecting to {}", self.socket.display())),
        })
    }

    /// Fails as a request would when no `watch` answers.
    pub fn reachable(&self) -> Result<()> {
        self.connect().map(drop)
    }

    fn request(&self, target: &str) -> Result<UnixStream> {
        let mut stream = self.connect()?;
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nHost: orchestrator\r\nConnection: close\r\n\r\n"
        )?;
        Ok(stream)
    }

    fn get<T: DeserializeOwned>(&self, target: &str) -> Result<T> {
        let mut answer = Vec::new();
        self.request(target)?.read_to_end(&mut answer)?;
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut response = httparse::Response::new(&mut headers);
        let start = match response.parse(&answer).context("parsing the answer")? {
            httparse::Status::Complete(start) => start,
            httparse::Status::Partial => bail!("orchestrator watch answered partly"),
        };
        let body = answer.get(start..).unwrap_or_default();
        if response.code == Some(200) {
            return serde_json::from_slice(body).context("reading the answer");
        }
        let message = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
        Err(anyhow!(message))
    }

    /// The events from now on, each as the JSON line `events.jsonl` gets.
    pub fn events(&self) -> Result<impl Iterator<Item = Result<String>>> {
        let reader = BufReader::new(self.request("/events")?);
        Ok(reader.lines().filter_map(|line| match line {
            Ok(line) => line.strip_prefix("data: ").map(|data| Ok(data.to_string())),
            Err(e) => Some(Err(e.into())),
        }))
    }
}

impl State for Client {
    fn sessions(&self, heads: bool) -> Result<SessionsReport> {
        self.get(&format!("/sessions?heads={heads}"))
    }

    fn admission(&self) -> Result<AdmissionReport> {
        self.get("/admission")
    }

    fn peaks(&self, at_least_mb: Option<u64>, most: Option<usize>) -> Result<PeaksReport> {
        let query: Vec<String> = [
            at_least_mb.map(|mb| format!("at_least_mb={mb}")),
            most.map(|n| format!("most={n}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        self.get(&format!("/peaks?{}", query.join("&")))
    }

    fn machine(&self) -> Result<Machine> {
        self.get("/machine")
    }

    fn config(&self) -> Result<ConfigReport> {
        self.get("/config")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Process, Session, Thresholds};

    /// Fixed answers, and an error for the configuration.
    struct Fixed;

    impl State for Fixed {
        fn sessions(&self, heads: bool) -> Result<SessionsReport> {
            Ok(SessionsReport {
                available_mb: 900,
                sessions: vec![Session {
                    name: "alpha".into(),
                    session_id: "a1".into(),
                    rss_mb: 400,
                    largest: Process {
                        pid: 7,
                        rss_mb: 300,
                        command: if heads {
                            "node".into()
                        } else {
                            "node --secret x".into()
                        },
                    },
                }],
                orphans: vec![],
            })
        }

        fn admission(&self) -> Result<AdmissionReport> {
            Ok(AdmissionReport {
                thresholds: Thresholds::Off("no file".into()),
                available_mb: 900,
                held_mb: 0,
                free_mb: 900,
                waiting: vec![],
                reserved: vec![],
            })
        }

        fn peaks(&self, at_least_mb: Option<u64>, _most: Option<usize>) -> Result<PeaksReport> {
            Ok(PeaksReport {
                at_least_mb,
                repositories: vec![],
            })
        }

        fn machine(&self) -> Result<Machine> {
            bail!("no machine here")
        }

        fn config(&self) -> Result<ConfigReport> {
            bail!("unreadable configuration")
        }
    }

    fn served(dir: &Path, hub: Hub) -> Client {
        serve(bind(&socket(dir)).unwrap(), Arc::new(Fixed), hub);
        Client::new(dir)
    }

    #[test]
    fn answers_the_reports_and_their_errors() {
        let dir = tempfile::tempdir().unwrap();
        let client = served(dir.path(), Hub::default());
        assert_eq!(
            client.sessions(true).unwrap().sessions[0].largest.command,
            "node"
        );
        assert_eq!(
            client.sessions(false).unwrap().sessions[0].largest.command,
            "node --secret x"
        );
        assert_eq!(client.admission().unwrap().free_mb, 900);
        assert_eq!(
            client.peaks(Some(1500), Some(3)).unwrap().at_least_mb,
            Some(1500)
        );
        let err = client.config().unwrap_err().to_string();
        assert_eq!(err, "unreadable configuration");
        let mut raw = UnixStream::connect(socket(dir.path())).unwrap();
        raw.write_all(b"GET /nothing HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut answer = String::new();
        raw.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 404 Not Found\r\n"), "{answer}");
        let metadata = std::fs::metadata(socket(dir.path())).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn one_watch_answers_at_a_time_and_a_dead_one_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let _client = served(dir.path(), Hub::default());
        let err = bind(&socket(dir.path())).unwrap_err().to_string();
        assert!(err.contains("another orchestrator watch"), "{err}");
        let stale = tempfile::tempdir().unwrap();
        drop(UnixListener::bind(socket(stale.path())).unwrap());
        assert!(bind(&socket(stale.path())).is_ok());
    }

    #[test]
    fn streams_the_events_as_they_come() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::default();
        let client = served(dir.path(), hub.clone());
        let mut events = client.events().unwrap();
        // The stream subscribes once its request is read: publish until it
        // has.
        std::thread::spawn(move || {
            for _ in 0..200 {
                hub.publish(r#"{"at":1,"kind":"orphans","orphans":[]}"#);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let first = events.next().unwrap().unwrap();
        assert_eq!(first, r#"{"at":1,"kind":"orphans","orphans":[]}"#);
    }

    #[test]
    fn without_watch_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let err = Client::new(dir.path())
            .sessions(true)
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("orchestrator watch is not running"),
            "{err}"
        );
    }
}
