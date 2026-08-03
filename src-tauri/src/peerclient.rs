//! The outbound half: everything this device initiates against another one.
//!
//! `hyper` + `hyper-util` rather than `reqwest`: both are already compiled into
//! the binary as axum's dependencies, so this costs no build time, and reqwest
//! would drag in a TLS stack that plaintext LAN traffic has no use for.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, Method, Request};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    auth,
    models::{OfferedFile, Peer},
    peers::{commit_of, pair_code},
    utils::now_ms,
};

/// A LAN round trip is milliseconds. Anything approaching this means the device
/// went away, and waiting the OS default (~21 s on Windows) makes the UI feel
/// broken.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

type HttpClient = Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>>;

fn client() -> HttpClient {
    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.set_connect_timeout(Some(CONNECT_TIMEOUT));
    connector.set_nodelay(true);
    Client::builder(TokioExecutor::new()).build(connector)
}

fn url(address: &str, path: &str) -> String {
    format!("http://{address}{path}")
}

/// POST JSON, parse JSON back. Every non-2xx is an error carrying the body's
/// `error` field, so the UI can say "declined" rather than "500".
async fn post_json<T: DeserializeOwned>(address: &str, path: &str, body: &Value) -> Result<T> {
    let client = client();
    let request = Request::builder()
        .method(Method::POST)
        .uri(url(address, path))
        .header(hyper::header::CONTENT_TYPE, "application/json")
        // The receiving end's host guard only accepts IP literals; we always
        // dial one, so this is consistent by construction.
        .header(hyper::header::HOST, address)
        .body(Full::new(Bytes::from(serde_json::to_vec(body)?)))?;

    let response = tokio::time::timeout(REQUEST_TIMEOUT, client.request(request))
        .await
        .map_err(|_| anyhow!("the device did not respond"))?
        .map_err(|e| anyhow!("could not reach the device: {e}"))?;

    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|e| anyhow!("could not read the reply: {e}"))?
        .to_bytes();

    if !status.is_success() {
        let detail = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| status.to_string());
        return Err(anyhow!("{detail}"));
    }
    serde_json::from_slice(&bytes).context("the device sent a reply we could not read")
}

async fn get_json<T: DeserializeOwned>(address: &str, path: &str, token: Option<&str>) -> Result<T> {
    let client = client();
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(url(address, path))
        .header(hyper::header::HOST, address);
    if let Some(token) = token {
        builder = builder.header(hyper::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder.body(Full::new(Bytes::new()))?;

    let response = tokio::time::timeout(REQUEST_TIMEOUT, client.request(request))
        .await
        .map_err(|_| anyhow!("the device did not respond"))?
        .map_err(|e| anyhow!("could not reach the device: {e}"))?;

    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|e| anyhow!("could not read the reply: {e}"))?
        .to_bytes();
    if !status.is_success() {
        return Err(anyhow!("the device refused the request ({status})"));
    }
    serde_json::from_slice(&bytes).context("the device sent a reply we could not read")
}

// ---------------------------------------------------------------------------
// hello
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct Hello {
    #[serde(rename = "deviceId")]
    pub(crate) device_id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) platform: String,
    #[serde(default)]
    pub(crate) port: u16,
}

pub(crate) async fn hello(address: &str) -> Result<Hello> {
    get_json(address, "/api/peer/hello", None).await
}

/// A raw authenticated GET, handed straight to the UI.
///
/// Used for peer browse: the shape of `/api/list` is the receiver UI's
/// contract, and re-declaring it here would be a second definition to keep in
/// step for no gain.
pub(crate) async fn get_authed(address: &str, path: &str, token: &str) -> Result<Value> {
    get_json(address, path, Some(token)).await
}

// ---------------------------------------------------------------------------
// Pairing (initiator side)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct PairRequestReply {
    pair_id: String,
    nonce_b: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    device_id: String,
}

#[derive(Debug, serde::Deserialize)]
struct PairPollReply {
    state: String,
    #[serde(default)]
    token: String,
    #[serde(default, rename = "deviceId")]
    device_id: String,
    #[serde(default)]
    name: String,
}

/// What the initiator learns as soon as the code is computable, so the UI can
/// show it while the user walks to the other device.
pub(crate) struct PairHandshake {
    pub(crate) pair_id: String,
    pub(crate) code: String,
    pub(crate) address: String,
    pub(crate) remote_id: String,
    pub(crate) remote_name: String,
    /// The token WE issued for THEM.
    pub(crate) issued_out_token: String,
    nonce_a: String,
}

/// Steps 1-3: commit, exchange nonces, reveal. Returns once both sides can
/// display the code; the user then compares and accepts on the other device.
pub(crate) async fn begin_pairing(
    address: &str,
    my_id: &str,
    my_name: &str,
    my_port: u16,
) -> Result<PairHandshake> {
    let nonce_a = auth::random_token();
    let commit = commit_of(&nonce_a);
    // The token the remote will present to US. Generated here and handed over
    // in the request, so the exchange needs no extra round trip.
    let issued_out_token = auth::random_token();

    let reply: PairRequestReply = post_json(
        address,
        "/api/peer/pair/request",
        &serde_json::json!({
            "device_id": my_id,
            "name": my_name,
            "platform": crate::models::platform_name(),
            "port": my_port,
            "commit": commit,
            "out_token": issued_out_token,
        }),
    )
    .await?;

    let code = pair_code(
        &commit,
        &nonce_a,
        &reply.nonce_b,
        my_id,
        &reply.device_id,
    );

    // Reveal only after computing the code locally: until this call the other
    // device cannot derive it, which is what stops an attacker running both
    // halves of the exchange from grinding the two codes into agreement.
    let echoed: Value = post_json(
        address,
        "/api/peer/pair/reveal",
        &serde_json::json!({ "pair_id": reply.pair_id, "nonce": nonce_a }),
    )
    .await?;

    // Both sides must have derived the same digits. If not, something is
    // between us -- fail loudly rather than showing the user two codes to
    // compare that were never going to match.
    if echoed.get("code").and_then(|c| c.as_str()) != Some(code.as_str()) {
        return Err(anyhow!(
            "the two devices computed different codes -- cancel and try again"
        ));
    }

    Ok(PairHandshake {
        pair_id: reply.pair_id,
        code,
        address: address.to_string(),
        remote_id: reply.device_id,
        remote_name: reply.name,
        issued_out_token,
        nonce_a,
    })
}

/// Step 4: wait for the far side's Accept. Returns the finished `Peer`.
pub(crate) async fn await_pairing(
    handshake: &PairHandshake,
    cancel: &Arc<AtomicBool>,
) -> Result<Peer> {
    let _ = &handshake.nonce_a;
    let deadline = now_ms() + crate::models::PAIR_TTL_MS;

    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(anyhow!("cancelled"));
        }
        if now_ms() > deadline {
            return Err(anyhow!("the other device did not answer in time"));
        }

        let reply: PairPollReply = post_json(
            &handshake.address,
            "/api/peer/pair/poll",
            &serde_json::json!({ "pair_id": handshake.pair_id }),
        )
        .await?;

        match reply.state.as_str() {
            "accepted" => {
                if reply.token.is_empty() {
                    return Err(anyhow!("the other device sent an empty token"));
                }
                return Ok(Peer {
                    device_id: if reply.device_id.is_empty() {
                        handshake.remote_id.clone()
                    } else {
                        reply.device_id
                    },
                    name: crate::config::clean_device_name(if reply.name.is_empty() {
                        &handshake.remote_name
                    } else {
                        &reply.name
                    }),
                    platform: String::new(),
                    // Mirror image of the far side: what they present to us is
                    // what we issued; what we present to them is what they
                    // just sent back.
                    in_token: handshake.issued_out_token.clone(),
                    out_token: reply.token,
                    added_ms: now_ms(),
                    last_seen_ms: now_ms(),
                    last_address: Some(handshake.address.clone()),
                    auto_accept: false,
                    blocked: false,
                    note: None,
                });
            }
            "declined" => return Err(anyhow!("the other device declined")),
            _ => {}
        }

        tokio::time::sleep(Duration::from_millis(600)).await;
    }
}

// ---------------------------------------------------------------------------
// Sending files
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct OfferReply {
    #[serde(rename = "offerId")]
    offer_id: String,
    state: String,
}

#[derive(Debug, serde::Deserialize)]
struct StateReply {
    state: String,
}

pub(crate) struct SendPlan {
    pub(crate) files: Vec<(PathBuf, OfferedFile)>,
    pub(crate) total_bytes: u64,
}

/// Stat the chosen paths into an offer. Unreadable files are dropped here
/// rather than failing the whole batch mid-flight.
pub(crate) fn plan_send(paths: &[String]) -> Result<SendPlan> {
    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    for raw in paths {
        let path = PathBuf::from(raw);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        // Validated with the same rule the receiver applies, so a name that
        // would be refused there is caught before anything is sent.
        if crate::shares::check_segment(&name).is_err() {
            continue;
        }
        total_bytes += meta.len();
        files.push((
            path,
            OfferedFile {
                name,
                size: meta.len(),
            },
        ));
    }
    if files.is_empty() {
        return Err(anyhow!("none of those files can be sent"));
    }
    Ok(SendPlan { files, total_bytes })
}

/// Make the offer and wait for the far side's answer.
pub(crate) async fn offer_and_wait(
    address: &str,
    token: &str,
    plan: &SendPlan,
    cancel: &Arc<AtomicBool>,
) -> Result<String> {
    let manifest: Vec<&OfferedFile> = plan.files.iter().map(|(_, f)| f).collect();
    let reply: OfferReply = post_json_auth(
        address,
        "/api/peer/offer",
        token,
        &serde_json::json!({ "files": manifest }),
    )
    .await?;

    if reply.state == "accepted" {
        return Ok(reply.offer_id);
    }

    // No deadline of our own: the receiver expires the offer, and a sender
    // that gave up first would leave the far side staring at a dead prompt.
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(anyhow!("cancelled"));
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
        let status: StateReply = get_json(
            address,
            &format!("/api/peer/offer/{}", reply.offer_id),
            Some(token),
        )
        .await?;
        match status.state.as_str() {
            "accepted" => return Ok(reply.offer_id),
            "declined" => return Err(anyhow!("declined")),
            _ => {}
        }
    }
}

async fn post_json_auth<T: DeserializeOwned>(
    address: &str,
    path: &str,
    token: &str,
    body: &Value,
) -> Result<T> {
    let client = client();
    let request = Request::builder()
        .method(Method::POST)
        .uri(url(address, path))
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .header(hyper::header::HOST, address)
        .header(hyper::header::AUTHORIZATION, format!("Bearer {token}"))
        // The same assertion the receiver page makes: this came from our own
        // client, not from a form somewhere. Routes that check for it (the play
        // link) then need no peer-shaped exception.
        .header(crate::models::CSRF_HEADER, "1")
        .body(Full::new(Bytes::from(serde_json::to_vec(body)?)))?;

    let response = tokio::time::timeout(REQUEST_TIMEOUT, client.request(request))
        .await
        .map_err(|_| anyhow!("the device did not respond"))?
        .map_err(|e| anyhow!("could not reach the device: {e}"))?;

    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|e| anyhow!("could not read the reply: {e}"))?
        .to_bytes();
    if !status.is_success() {
        let detail = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| status.to_string());
        return Err(anyhow!("{detail}"));
    }
    serde_json::from_slice(&bytes).context("the device sent a reply we could not read")
}

// ---------------------------------------------------------------------------
// Play links
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct PlayLink {
    pub(crate) token: String,
    pub(crate) name: String,
}

/// Ask a paired device for a link its file can be played from.
///
/// No new endpoint on their side: a peer holds a real session scope, so this is
/// the same `/api/play/link` the browser uses. A local player cannot present
/// our pair token any more than it can present a cookie, which is exactly the
/// problem the play token exists to solve -- here for a file on their disk.
pub(crate) async fn play_link(
    address: &str,
    token: &str,
    share_id: &str,
    path: &str,
) -> Result<PlayLink> {
    post_json_auth(
        address,
        "/api/play/link",
        token,
        &serde_json::json!({ "share": share_id, "path": path }),
    )
    .await
}

/// Upload one file, reporting bytes as they leave.
///
/// The body is read in chunks off the blocking pool and pushed through a
/// bounded channel, so a slow receiver applies backpressure instead of letting
/// us buffer a 4 GB file in memory.
pub(crate) async fn send_file(
    address: &str,
    token: &str,
    offer_id: &str,
    index: usize,
    path: &Path,
    size: u64,
    cancel: Arc<AtomicBool>,
    mut on_progress: impl FnMut(u64) + Send + 'static,
) -> Result<()> {
    use http_body_util::StreamBody;
    use hyper::body::Frame;

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(4);
    let path = path.to_path_buf();
    let reader_cancel = Arc::clone(&cancel);

    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(err) => {
                let _ = tx.blocking_send(Err(err));
                return;
            }
        };
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if reader_cancel.load(Ordering::SeqCst) {
                let _ = tx.blocking_send(Err(std::io::Error::other("cancelled")));
                return;
            }
            match file.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    if tx
                        .blocking_send(Ok(Frame::data(Bytes::copy_from_slice(&buf[..n]))))
                        .is_err()
                    {
                        return; // receiver gone
                    }
                }
                Err(err) => {
                    let _ = tx.blocking_send(Err(err));
                    return;
                }
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = StreamBody::new(stream);

    let client: Client<_, StreamBody<tokio_stream::wrappers::ReceiverStream<_>>> = {
        let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
        connector.set_connect_timeout(Some(CONNECT_TIMEOUT));
        connector.set_nodelay(true);
        Client::builder(TokioExecutor::new()).build(connector)
    };

    let request = Request::builder()
        .method(Method::PUT)
        .uri(url(
            address,
            &format!("/api/peer/file/{offer_id}/{index}"),
        ))
        .header(hyper::header::HOST, address)
        .header(hyper::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(hyper::header::CONTENT_TYPE, "application/octet-stream")
        // Explicit, not chunked: the receiver requires it, because it is the
        // only way to tell a finished transfer from an abandoned one.
        .header(hyper::header::CONTENT_LENGTH, size)
        .body(body)?;

    let response = client
        .request(request)
        .await
        .map_err(|e| anyhow!("the transfer failed: {e}"))?;

    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|e| anyhow!("could not read the reply: {e}"))?
        .to_bytes();

    if !status.is_success() {
        let detail = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| status.to_string());
        return Err(anyhow!("{detail}"));
    }

    on_progress(size);
    Ok(())
}
