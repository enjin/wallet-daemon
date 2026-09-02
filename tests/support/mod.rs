//! A mock Enjin Platform, and a harness that runs the real daemon binary
//! against it.
//!
//! The daemon reaches the platform for everything it needs — including chain
//! metadata, via `GetChainInfo` — so pointing `PLATFORM_URL` at this server
//! exercises the whole daemon (fetch, sign, submit) with no chain, no funds and
//! no external network. That makes the failure paths this covers — pagination,
//! poison rows, platform outages, backoff pacing — reproducible in CI, which is
//! exactly what a live platform cannot give us.

#![allow(dead_code)]

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use parity_scale_codec::Encode;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};

/// `System.remark` in the bundled canary metadata: pallet 0, call 0, then a
/// SCALE-encoded empty `Vec<u8>`. The daemon re-encodes the trailing bytes
/// verbatim, so this is enough to drive a real signature.
pub const REMARK_PAYLOAD: &str = "0x000000";
/// A payload whose pallet/call indices do not exist in the metadata, so
/// `RawPayload::from_bytes` fails and the request is skipped every time.
pub const POISON_PAYLOAD: &str = "0xfefe00";

/// One pending transaction the mock will hand out.
#[derive(Clone)]
pub struct PendingTx {
    pub uuid: String,
    pub encoded_data: String,
}

impl PendingTx {
    pub fn good(uuid: &str) -> Self {
        Self {
            uuid: uuid.to_string(),
            encoded_data: REMARK_PAYLOAD.to_string(),
        }
    }

    /// A row that can be fetched and converted but never signed.
    pub fn poison(uuid: &str) -> Self {
        Self {
            uuid: uuid.to_string(),
            encoded_data: POISON_PAYLOAD.to_string(),
        }
    }

    /// A row that fails `TryFrom` outright, so the page arrives empty after
    /// filtering even though the platform said it had data.
    pub fn unconvertible(uuid: &str) -> Self {
        Self {
            uuid: uuid.to_string(),
            // No "0x", so the payload cannot be decoded at all.
            encoded_data: "not-a-payload".to_string(),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "uuid": self.uuid,
            "encodedData": self.encoded_data,
            "wallet": { "publicKey": null, "externalId": null },
            "network": "CANARY",
            "chain": "MATRIX",
            "shouldSignFuelTank": false,
            "fuelTankSignerExternalId": null,
        })
    }
}

/// How the mock should answer `SignTransactions`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SignBehaviour {
    Accept,
    /// Transport failure — the platform is down.
    HttpError,
}

/// How the mock should answer `PopulateManagedWallets`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PopulateBehaviour {
    Accept,
    /// A business-level rejection: HTTP 200 with `{"data":{"result":false}}`.
    RejectFalse,
}

/// One recorded call, with the time it arrived, so tests can assert pacing.
#[derive(Clone)]
pub struct Call {
    pub at: Instant,
    pub operation: String,
    pub variables: Value,
}

/// One scripted page: the rows to return, and the cursor to advertise next.
type Page<T> = (Vec<T>, Option<String>);

#[derive(Default)]
struct Pages {
    /// `cursor key -> page`. The `None` cursor is keyed `""`.
    by_cursor: HashMap<String, Page<PendingTx>>,
}

pub struct MockPlatform {
    pub addr: SocketAddr,
    state: Arc<State>,
}

struct State {
    metadata_hex: String,
    started: Instant,
    calls: Mutex<Vec<Call>>,
    /// Every uuid handed to `SignTransactions`, in order, across all calls.
    signed: Mutex<Vec<String>>,
    tx_pages: Mutex<Pages>,
    wallet_pages: Mutex<HashMap<String, Page<String>>>,
    sign_behaviour: Mutex<SignBehaviour>,
    populate_behaviour: Mutex<PopulateBehaviour>,
    /// When set, the mock keeps serving the same page forever, so a scenario
    /// can hold the daemon in a steady state while pacing is measured.
    repeat_first_page: AtomicBool,
    nonce: AtomicUsize,
}

impl MockPlatform {
    pub async fn start() -> Self {
        let metadata_path = format!(
            "{}/tests/fixtures/canary_matrix_metadata.scale",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read(&metadata_path).expect("metadata fixture missing");
        // The daemon decodes this field as `Option<Vec<u8>>`, so wrap it.
        let metadata_hex = format!("0x{}", hex::encode(Some(raw).encode()));

        let state = Arc::new(State {
            metadata_hex,
            started: Instant::now(),
            calls: Mutex::new(Vec::new()),
            signed: Mutex::new(Vec::new()),
            tx_pages: Mutex::new(Pages::default()),
            wallet_pages: Mutex::new(HashMap::new()),
            sign_behaviour: Mutex::new(SignBehaviour::Accept),
            populate_behaviour: Mutex::new(PopulateBehaviour::Accept),
            repeat_first_page: AtomicBool::new(false),
            nonce: AtomicUsize::new(0),
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let serve_state = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let conn_state = serve_state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| handle(req, conn_state.clone()));
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        Self { addr, state }
    }

    pub fn url(&self) -> String {
        format!("http://{}/graphql/daemon", self.addr)
    }

    /// Script a page of pending transactions. `cursor` is the cursor the daemon
    /// must send to receive it (`None` for a fresh scan).
    pub fn set_tx_page(
        &self,
        cursor: Option<&str>,
        rows: Vec<PendingTx>,
        next_cursor: Option<&str>,
    ) {
        self.state.tx_pages.lock().unwrap().by_cursor.insert(
            cursor.unwrap_or("").to_string(),
            (rows, next_cursor.map(str::to_string)),
        );
    }

    pub fn set_wallet_page(
        &self,
        cursor: Option<&str>,
        external_ids: Vec<&str>,
        next_cursor: Option<&str>,
    ) {
        self.state.wallet_pages.lock().unwrap().insert(
            cursor.unwrap_or("").to_string(),
            (
                external_ids.into_iter().map(str::to_string).collect(),
                next_cursor.map(str::to_string),
            ),
        );
    }

    pub fn set_sign_behaviour(&self, behaviour: SignBehaviour) {
        *self.state.sign_behaviour.lock().unwrap() = behaviour;
    }

    pub fn set_populate_behaviour(&self, behaviour: PopulateBehaviour) {
        *self.state.populate_behaviour.lock().unwrap() = behaviour;
    }

    /// Keep answering every cursor with the fresh page, so the daemon never
    /// runs out of work.
    pub fn repeat_first_page(&self, repeat: bool) {
        self.state.repeat_first_page.store(repeat, Ordering::SeqCst);
    }

    pub fn signed_uuids(&self) -> Vec<String> {
        self.state.signed.lock().unwrap().clone()
    }

    pub fn calls(&self) -> Vec<Call> {
        self.state.calls.lock().unwrap().clone()
    }

    pub fn calls_to(&self, operation: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| call.operation == operation)
            .collect()
    }

    pub fn count_of(&self, operation: &str) -> usize {
        self.calls_to(operation).len()
    }

    pub fn reset_calls(&self) {
        self.state.calls.lock().unwrap().clear();
    }

    /// Wait until `predicate` holds, or fail with a diagnostic after `timeout`.
    pub async fn wait_for<F>(&self, what: &str, timeout: Duration, predicate: F)
    where
        F: Fn(&MockPlatform) -> bool,
    {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let counts = summarise(&self.calls());
        panic!("timed out after {timeout:?} waiting for {what}; calls so far: {counts}");
    }
}

fn summarise(calls: &[Call]) -> String {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for call in calls {
        *counts.entry(call.operation.as_str()).or_default() += 1;
    }
    let mut parts: Vec<String> = counts
        .into_iter()
        .map(|(op, n)| format!("{op}={n}"))
        .collect();
    parts.sort();
    parts.join(" ")
}

async fn handle(
    req: Request<hyper::body::Incoming>,
    state: Arc<State>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let body = req.into_body().collect().await.unwrap().to_bytes();
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let operation = request
        .get("operationName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let variables = request.get("variables").cloned().unwrap_or(Value::Null);

    state.calls.lock().unwrap().push(Call {
        at: Instant::now(),
        operation: operation.clone(),
        variables: variables.clone(),
    });

    // `GetChainInfo` and the truncated `GetCurrentBlockNumber` share an
    // operation name; returning the full object satisfies both.
    let (status, data) = match operation.as_str() {
        "SetDaemonWalletAccount" => (200, json!({ "result": true })),
        "GetChainInfo" => (
            200,
            json!({
                "result": {
                    "currentBlockNumber": 1_000_000,
                    "currentBlockHash": format!("0x{}", "11".repeat(32)),
                    "specVersion": 1031,
                    "transactionVersion": 12,
                    "metadataVersion": 15,
                    "metadata": state.metadata_hex,
                }
            }),
        ),
        "GetAccountNonce" => (
            200,
            json!({ "result": state.nonce.load(Ordering::SeqCst) as i64 }),
        ),
        "AuthenticatePusherSocket" => (
            200,
            json!({ "result": { "auth": "mock:signature", "channel": "private-daemon" } }),
        ),
        "GetPendingTransactions" => {
            let cursor = variables
                .get("cursor")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let key = if state.repeat_first_page.load(Ordering::SeqCst) {
                String::new()
            } else {
                cursor
            };
            let pages = state.tx_pages.lock().unwrap();
            match pages.by_cursor.get(&key) {
                Some((rows, next)) => (
                    200,
                    json!({
                        "result": {
                            "data": rows.iter().map(PendingTx::to_json).collect::<Vec<_>>(),
                            "perPage": 25,
                            "previousCursor": null,
                            "nextCursor": next,
                        }
                    }),
                ),
                // No script for this cursor: behave like an idle platform.
                None => (200, json!({ "result": null })),
            }
        }
        "GetPendingManagedWalletCreations" => {
            let cursor = variables
                .get("cursor")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let pages = state.wallet_pages.lock().unwrap();
            match pages.get(&cursor) {
                Some((ids, next)) => (
                    200,
                    json!({
                        "result": {
                            "data": ids.iter()
                                .map(|id| json!({ "publicKey": null, "externalId": id }))
                                .collect::<Vec<_>>(),
                            "perPage": 100,
                            "previousCursor": null,
                            "nextCursor": next,
                        }
                    }),
                ),
                None => (200, json!({ "result": null })),
            }
        }
        "SignTransactions" => {
            let uuids: Vec<String> = variables
                .get("transactions")
                .and_then(Value::as_array)
                .map(|txs| {
                    txs.iter()
                        .filter_map(|tx| tx.get("uuid").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();

            match *state.sign_behaviour.lock().unwrap() {
                SignBehaviour::Accept => {
                    let count = uuids.len();
                    state.signed.lock().unwrap().extend(uuids);
                    state.nonce.fetch_add(count, Ordering::SeqCst);
                    (200, json!({ "result": true }))
                }
                SignBehaviour::HttpError => (502, Value::Null),
            }
        }
        "PopulateManagedWallets" => match *state.populate_behaviour.lock().unwrap() {
            PopulateBehaviour::Accept => (200, json!({ "result": true })),
            PopulateBehaviour::RejectFalse => (200, json!({ "result": false })),
        },
        _ => (200, json!({ "result": null })),
    };

    let _ = state.started;
    let response = if status == 200 {
        Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(json!({ "data": data }).to_string())))
            .unwrap()
    } else {
        Response::builder()
            .status(status)
            .body(Full::new(Bytes::from("upstream failure")))
            .unwrap()
    };
    Ok(response)
}

/// The daemon binary, running against a mock platform.
pub struct Daemon {
    child: Child,
    _seed_dir: tempfile::TempDir,
    pub logs: Arc<Mutex<Vec<String>>>,
}

impl Daemon {
    pub fn start(platform: &MockPlatform) -> Self {
        let seed_dir = tempfile::tempdir().unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_wallet-daemon"))
            .env("PLATFORM_URL", platform.url())
            .env("PLATFORM_KEY", "mock-token")
            .env("KEY_PASS", "mock-key-pass")
            .env("SEED_PATH", seed_dir.path())
            .env("RUST_LOG", "wallet_daemon=debug")
            // Point Pusher at a cluster that cannot resolve. The subscription
            // fails fast, which is the documented fallback path and keeps these
            // tests off the public internet.
            .env("PUSHER_CLUSTER", "invalid-cluster-for-tests")
            .env("PUSHER_APP_KEY", "mockappkey")
            // The daemon looks for a `.env` next to the working directory;
            // run it somewhere empty so a developer's real one is never read.
            .current_dir(seed_dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn the daemon binary");

        let logs = Arc::new(Mutex::new(Vec::new()));
        for stream in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let sink = logs.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    sink.lock().unwrap().push(line);
                }
            });
        }

        Self {
            child,
            _seed_dir: seed_dir,
            logs,
        }
    }

    pub fn log_contains(&self, needle: &str) -> bool {
        self.logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(needle))
    }

    pub fn dump_logs(&self) -> String {
        self.logs.lock().unwrap().join("\n")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}
