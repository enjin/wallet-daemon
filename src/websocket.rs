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

#[derive(Debug, Deserialize)]
struct TransactionCreated {
    uuid: String,
}

#[derive(Debug, Deserialize)]
struct ManagedWalletRequested {
    #[serde(rename = "externalId")]
    external_id: String,
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
                    failures = 0;
                    self.status.set_connected(true);
                    tracing::info!(
                        "Pusher WebSocket subscribed successfully; using five-minute safety polling"
                    );

                    // Pusher does not replay messages missed while disconnected.
                    self.transaction_trigger.force();
                    self.wallet_trigger.force();

                    let listen_result = self.listen(socket, activity_timeout, &channel).await;
                    self.status.set_connected(false);
                    if let Err(error) = listen_result {
                        tracing::warn!(
                            "Pusher WebSocket disconnected; enabling six-second fallback polling: {error}"
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
            Duration::from_secs(established.activity_timeout.max(1)),
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
                                "TransactionCreated" => {
                                    match decode_data::<TransactionCreated>(frame.data) {
                                        Ok(event) => self.transaction_trigger.record_event(event.uuid),
                                        Err(error) => tracing::warn!(
                                            "Ignoring malformed TransactionCreated event: {error}"
                                        ),
                                    }
                                }
                                "ManagedWalletRequested" => {
                                    match decode_data::<ManagedWalletRequested>(frame.data) {
                                        Ok(event) => self.wallet_trigger.record_event(event.external_id),
                                        Err(error) => tracing::warn!(
                                            "Ignoring malformed ManagedWalletRequested event: {error}"
                                        ),
                                    }
                                }
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
    format!("Pusher protocol error: {data}").into()
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_json_encoded_pusher_payloads() {
        let payload = Value::String(r#"{"uuid":"transaction-1"}"#.to_string());
        let event = decode_data::<TransactionCreated>(payload).unwrap();
        assert_eq!(event.uuid, "transaction-1");
    }

    #[test]
    fn decodes_object_pusher_payloads() {
        let payload = json!({"externalId": "wallet-1"});
        let event = decode_data::<ManagedWalletRequested>(payload).unwrap();
        assert_eq!(event.external_id, "wallet-1");
    }

    #[test]
    fn validates_configurable_pusher_components() {
        assert!(valid_component("8ab7ab8c519e8f59b635"));
        assert!(valid_component("us2"));
        assert!(!valid_component(""));
        assert!(!valid_component("us2/path"));
    }
}
