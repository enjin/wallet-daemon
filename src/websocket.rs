use crate::platform_client;
use crate::retry::jittered_exponential_delay;
use crate::work_trigger::{PusherStatus, WorkTrigger};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const DEFAULT_PUSHER_APP_KEY: &str = "8ab7ab8c519e8f59b635";
const DEFAULT_PUSHER_CLUSTER: &str = "us2";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const PONG_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_STABLE_SUBSCRIPTION: Duration = Duration::from_secs(30);
/// Bounds on the server-supplied `activity_timeout`. The lower bound keeps a
/// zero from producing a hot ping loop; the upper bound keeps an absurd value
/// from overflowing `Instant + Duration` (a panic) or silently disabling the
/// keepalive so a half-open socket is never detected. Pusher's own value is
/// 120s.
const MIN_ACTIVITY_TIMEOUT_SECS: u64 = 1;
const MAX_ACTIVITY_TIMEOUT_SECS: u64 = 600;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Deserialize)]
struct PusherFrame {
    event: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    data: Value,
}

#[derive(Debug, Deserialize)]
struct ConnectionEstablished {
    socket_id: String,
    activity_timeout: u64,
}

#[derive(Debug)]
pub(crate) struct PusherConnection {
    transaction_trigger: WorkTrigger,
    wallet_trigger: WorkTrigger,
    status: PusherStatus,
    app_key: String,
    cluster: String,
}

impl PusherConnection {
    pub(crate) fn from_env(
        transaction_trigger: WorkTrigger,
        wallet_trigger: WorkTrigger,
        status: PusherStatus,
    ) -> Result<Self, BoxError> {
        let app_key =
            dotenvy::var("PUSHER_APP_KEY").unwrap_or_else(|_| DEFAULT_PUSHER_APP_KEY.to_string());
        let cluster =
            dotenvy::var("PUSHER_CLUSTER").unwrap_or_else(|_| DEFAULT_PUSHER_CLUSTER.to_string());

        if !valid_component(&app_key) {
            return Err("PUSHER_APP_KEY may only contain letters, numbers, '-' and '_'".into());
        }
        if !valid_component(&cluster) {
            return Err("PUSHER_CLUSTER may only contain letters, numbers, '-' and '_'".into());
        }

        Ok(Self {
            transaction_trigger,
            wallet_trigger,
            status,
            app_key,
            cluster,
        })
    }

    pub(crate) fn start(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }

    async fn run(self) {
        let mut failures = 0u32;

        loop {
            match self.connect_and_subscribe().await {
                Ok((socket, activity_timeout, channel)) => {
                    self.status.set_connected(true);
                    tracing::info!(
                        "Pusher WebSocket subscribed successfully; using three-minute safety polling"
                    );

                    // Pusher does not replay messages missed while disconnected.
                    self.transaction_trigger.force();
                    self.wallet_trigger.force();

                    let subscribed_at = Instant::now();
                    let listen_result = self.listen(socket, activity_timeout, &channel).await;
                    let connected_for = subscribed_at.elapsed();
                    failures = reconnect_backoff_attempt(failures, connected_for);
                    self.status.set_connected(false);
                    if let Err(error) = listen_result {
                        tracing::warn!(
                            "Pusher WebSocket disconnected after {:.1}s; enabling six-second fallback polling: {error}",
                            connected_for.as_secs_f64(),
                        );
                    }
                }
                Err(error) => {
                    self.status.set_connected(false);
                    tracing::warn!(
                        "Could not establish authenticated Pusher WebSocket; six-second fallback polling remains active: {error}"
                    );
                }
            }

            let delay = jittered_exponential_delay(failures);
            failures = failures.saturating_add(1);
            tracing::info!("Retrying Pusher WebSocket in {:.1}s", delay.as_secs_f64());
            sleep(delay).await;
        }
    }

    async fn connect_and_subscribe(&self) -> Result<(Socket, Duration, String), BoxError> {
        let url = format!(
            "wss://ws-{}.pusher.com/app/{}?protocol=7",
            self.cluster, self.app_key
        );
        let (mut socket, _) = timeout(HANDSHAKE_TIMEOUT, connect_async(&url))
            .await
            .map_err(|_| "timed out connecting to Pusher")??;

        let established = timeout(HANDSHAKE_TIMEOUT, async {
            loop {
                let frame = receive_protocol_frame(&mut socket).await?;
                match frame.event.as_str() {
                    "pusher:connection_established" => {
                        return decode_data::<ConnectionEstablished>(frame.data);
                    }
                    "pusher:error" => return Err(pusher_error(frame.data)),
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| "timed out waiting for Pusher connection establishment")??;

        let subscription =
            platform_client::authenticate_pusher_socket(established.socket_id).await?;
        let subscribe = json!({
            "event": "pusher:subscribe",
            "data": {
                "channel": subscription.channel,
                "auth": subscription.auth,
            }
        });
        socket
            .send(Message::Text(subscribe.to_string().into()))
            .await?;

        let expected_channel = subscription.channel;
        timeout(HANDSHAKE_TIMEOUT, async {
            loop {
                let frame = receive_protocol_frame(&mut socket).await?;
                match frame.event.as_str() {
                    "pusher_internal:subscription_succeeded"
                        if frame.channel.as_deref() == Some(expected_channel.as_str()) =>
                    {
                        return Ok::<(), BoxError>(());
                    }
                    "pusher:error" => return Err(pusher_error(frame.data)),
                    "pusher:ping" => send_pusher_pong(&mut socket).await?,
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| "timed out waiting for Pusher subscription confirmation")??;

        Ok((
            socket,
            Duration::from_secs(
                established
                    .activity_timeout
                    .clamp(MIN_ACTIVITY_TIMEOUT_SECS, MAX_ACTIVITY_TIMEOUT_SECS),
            ),
            expected_channel,
        ))
    }

    async fn listen(
        &self,
        mut socket: Socket,
        activity_timeout: Duration,
        channel: &str,
    ) -> Result<(), BoxError> {
        let mut activity_deadline = Instant::now() + activity_timeout;
        let mut pong_deadline: Option<Instant> = None;

        loop {
            let deadline = pong_deadline.unwrap_or(activity_deadline);
            tokio::select! {
                message = socket.next() => {
                    let message = message.ok_or("Pusher WebSocket stream ended")??;
                    // Any inbound traffic proves that the connection is alive.
                    activity_deadline = Instant::now() + activity_timeout;
                    pong_deadline = None;

                    match message {
                        Message::Text(text) => {
                            let frame: PusherFrame = serde_json::from_str(text.as_str())?;
                            if let Some(frame_channel) = frame.channel.as_deref()
                                && frame_channel != channel
                            {
                                continue;
                            }

                            match frame.event.as_str() {
                                "TransactionCreated" => self.transaction_trigger.record_event(),
                                "ManagedWalletRequested" => self.wallet_trigger.record_event(),
                                "pusher:ping" => send_pusher_pong(&mut socket).await?,
                                "pusher:pong" | "pusher_internal:subscription_succeeded" => {}
                                "pusher:error" => return Err(pusher_error(frame.data)),
                                _ => {}
                            }
                        }
                        Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
                        Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
                        Message::Close(frame) => {
                            return Err(format!("Pusher closed the connection: {frame:?}").into());
                        }
                    }
                }
                _ = sleep_until(deadline) => {
                    if pong_deadline.is_some() {
                        return Err("Pusher ping timed out".into());
                    }

                    let ping = json!({"event": "pusher:ping", "data": {}});
                    socket.send(Message::Text(ping.to_string().into())).await?;
                    pong_deadline = Some(Instant::now() + PONG_TIMEOUT.min(activity_timeout));
                }
            }
        }
    }
}

async fn receive_protocol_frame(socket: &mut Socket) -> Result<PusherFrame, BoxError> {
    loop {
        let message = socket
            .next()
            .await
            .ok_or("Pusher WebSocket stream ended")??;
        match message {
            Message::Text(text) => return Ok(serde_json::from_str(text.as_str())?),
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
            Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
            Message::Close(frame) => {
                return Err(format!("Pusher closed the connection: {frame:?}").into());
            }
        }
    }
}

async fn send_pusher_pong(socket: &mut Socket) -> Result<(), BoxError> {
    let pong = json!({"event": "pusher:pong", "data": {}});
    socket.send(Message::Text(pong.to_string().into())).await?;
    Ok(())
}

fn decode_data<T: for<'de> Deserialize<'de>>(data: Value) -> Result<T, BoxError> {
    match data {
        Value::String(encoded) => Ok(serde_json::from_str(&encoded)?),
        value => Ok(serde_json::from_value(value)?),
    }
}

fn pusher_error(data: Value) -> BoxError {
    // Never format the raw payload: a Pusher invalid-signature error echoes the
    // submitted `auth` string back, which would defeat `redact_auth` by writing
    // the signature to the logs at `warn!` level.
    // Pusher sends `data` either as an object or as a JSON-encoded string.
    // Keep the original around: a payload that is not valid JSON is still a
    // plain diagnostic message worth reporting.
    let decoded = match &data {
        Value::String(encoded) => serde_json::from_str::<Value>(encoded).unwrap_or(Value::Null),
        other => other.clone(),
    };
    let code = decoded
        .get("code")
        .and_then(Value::as_i64)
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let message = decoded
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| data.as_str())
        .map(redact_pusher_message)
        .unwrap_or_else(|| "no message".to_string());
    format!("Pusher protocol error (code {code}): {message}").into()
}

/// Pusher error messages are operator-facing text, but the invalid-signature
/// variant embeds the credentials that were submitted. Keep the wording and
/// drop anything that looks like an app-key-prefixed signature.
fn redact_pusher_message(message: &str) -> String {
    message
        .split_whitespace()
        .map(|word| {
            let candidate = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != ':');
            if candidate.contains(':') && candidate.len() >= 32 {
                "[redacted]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn reconnect_backoff_attempt(current_attempt: u32, connected_for: Duration) -> u32 {
    if connected_for >= MIN_STABLE_SUBSCRIPTION {
        0
    } else {
        current_attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn loopback_websocket_pair() -> (Socket, WebSocketStream<TcpStream>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_async(stream).await.unwrap()
        });
        let (client, _) = connect_async(format!("ws://{address}")).await.unwrap();
        (client, server.await.unwrap())
    }

    async fn send_application_event(
        socket: &mut WebSocketStream<TcpStream>,
        event: &str,
        channel: &str,
    ) {
        socket
            .send(Message::Text(
                json!({
                    "event": event,
                    "channel": channel,
                    "data": null,
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
    }

    #[test]
    fn decodes_json_encoded_pusher_protocol_payloads() {
        let payload = Value::String(r#"{"socket_id":"1.2","activity_timeout":30}"#.to_string());
        let established = decode_data::<ConnectionEstablished>(payload).unwrap();
        assert_eq!(established.socket_id, "1.2");
        assert_eq!(established.activity_timeout, 30);
    }

    #[test]
    fn application_event_payload_is_not_required_for_notification() {
        let frame: PusherFrame = serde_json::from_value(json!({
            "event": "TransactionCreated",
            "channel": "private-daemon",
            "data": null,
        }))
        .unwrap();
        assert_eq!(frame.event, "TransactionCreated");
    }

    #[tokio::test]
    async fn listen_routes_only_matching_channel_events_to_the_corresponding_trigger() {
        let transaction_trigger = WorkTrigger::new();
        let wallet_trigger = WorkTrigger::new();
        let connection = PusherConnection {
            transaction_trigger: transaction_trigger.clone(),
            wallet_trigger: wallet_trigger.clone(),
            status: PusherStatus::new(),
            app_key: "test-app".to_string(),
            cluster: "test-cluster".to_string(),
        };
        let (client, mut server) = loopback_websocket_pair().await;
        let listening = tokio::spawn(async move {
            connection
                .listen(client, Duration::from_secs(60), "private-daemon")
                .await
        });

        send_application_event(&mut server, "TransactionCreated", "private-other").await;
        assert!(
            timeout(
                Duration::from_millis(750),
                transaction_trigger.wait_until_ready()
            )
            .await
            .is_err(),
            "an event for another channel must not wake the transaction trigger",
        );
        assert!(
            timeout(Duration::from_millis(50), wallet_trigger.wait_until_ready())
                .await
                .is_err(),
            "an event for another channel must not wake the wallet trigger",
        );

        send_application_event(&mut server, "TransactionCreated", "private-daemon").await;
        timeout(
            Duration::from_secs(2),
            transaction_trigger.wait_until_ready(),
        )
        .await
        .expect("a matching transaction event must wake its trigger");
        assert!(
            timeout(Duration::from_millis(50), wallet_trigger.wait_until_ready())
                .await
                .is_err(),
            "a transaction event must not wake the wallet trigger",
        );
        transaction_trigger.begin_fresh_lookup();
        transaction_trigger.finish_empty_lookup();

        send_application_event(&mut server, "ManagedWalletRequested", "private-daemon").await;
        timeout(Duration::from_secs(2), wallet_trigger.wait_until_ready())
            .await
            .expect("a matching wallet event must wake its trigger");
        assert!(
            timeout(
                Duration::from_millis(50),
                transaction_trigger.wait_until_ready()
            )
            .await
            .is_err(),
            "a wallet event must not wake the transaction trigger",
        );

        server.close(None).await.unwrap();
        let listen_result = timeout(Duration::from_secs(2), listening)
            .await
            .expect("listener must stop after the server closes")
            .expect("listener task must not panic");
        assert!(listen_result.is_err());
    }

    #[test]
    fn pusher_errors_report_the_code_without_echoing_the_submitted_auth() {
        // Pusher's invalid-signature error quotes the auth string back at us.
        // Formatting the raw payload would write it to the logs at warn!,
        // defeating the redaction applied on the request side.
        let auth =
            "8ab7ab8c519e8f59b635:0d1f4a9c7b3e2d5a8f6c0b9e4d7a2c5f8b1e3d6a9c2f5b8e1d4a7c0f3b6e9d2a";
        let error = pusher_error(json!({
            "code": 4009,
            "message": format!("Invalid signature: Expected HMAC SHA256 hex digest of ..., but received {auth}"),
        }))
        .to_string();

        assert!(error.contains("4009"), "the code must survive: {error}");
        assert!(
            !error.contains(auth),
            "the submitted auth must not reach the logs: {error}",
        );
    }

    #[test]
    fn a_plain_string_pusher_error_keeps_its_message_but_still_redacts() {
        let auth =
            "8ab7ab8c519e8f59b635:0d1f4a9c7b3e2d5a8f6c0b9e4d7a2c5f8b1e3d6a9c2f5b8e1d4a7c0f3b6e9d2a";
        let error =
            pusher_error(Value::String(format!("connection refused for {auth}"))).to_string();
        assert!(error.contains("connection refused"));
        assert!(!error.contains(auth), "{error}");
    }

    #[test]
    fn pusher_errors_survive_a_missing_code_or_message() {
        let error = pusher_error(json!({})).to_string();
        assert!(error.contains("unknown"));
        assert!(error.contains("no message"));
    }

    #[test]
    fn pusher_errors_decode_json_encoded_payloads() {
        let error = pusher_error(Value::String(
            r#"{"code":4001,"message":"App key not found"}"#.to_string(),
        ))
        .to_string();
        assert!(error.contains("4001"));
        assert!(error.contains("App key not found"));
    }

    #[test]
    fn activity_timeout_is_clamped_at_both_ends() {
        assert_eq!(
            0u64.clamp(MIN_ACTIVITY_TIMEOUT_SECS, MAX_ACTIVITY_TIMEOUT_SECS),
            1
        );
        assert_eq!(
            120u64.clamp(MIN_ACTIVITY_TIMEOUT_SECS, MAX_ACTIVITY_TIMEOUT_SECS),
            120
        );
        // Without the upper bound this overflows `Instant + Duration`.
        assert_eq!(
            u64::MAX.clamp(MIN_ACTIVITY_TIMEOUT_SECS, MAX_ACTIVITY_TIMEOUT_SECS),
            MAX_ACTIVITY_TIMEOUT_SECS,
        );
        let _ = Instant::now() + Duration::from_secs(MAX_ACTIVITY_TIMEOUT_SECS);
    }

    #[test]
    fn validates_configurable_pusher_components() {
        assert!(valid_component("8ab7ab8c519e8f59b635"));
        assert!(valid_component("us2"));
        assert!(!valid_component(""));
        assert!(!valid_component("us2/path"));
    }

    #[test]
    fn short_lived_subscriptions_preserve_reconnect_escalation() {
        assert_eq!(
            reconnect_backoff_attempt(5, MIN_STABLE_SUBSCRIPTION - Duration::from_millis(1)),
            5
        );
    }

    #[test]
    fn stable_subscriptions_reset_reconnect_escalation() {
        assert_eq!(reconnect_backoff_attempt(5, MIN_STABLE_SUBSCRIPTION), 0);
    }
}
