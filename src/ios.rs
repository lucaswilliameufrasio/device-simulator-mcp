//! Opt-in serve-sim 0.1.47 transport. The backend is externally provisioned;
//! we own only our sockets and never stop another client's server.
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(crate) const SERVE_SIM_PACKAGE: &str = "serve-sim@0.1.47";

pub(crate) fn command(arguments: Vec<String>) -> anyhow::Result<(String, Vec<String>)> {
    if let Ok(binary) = std::env::var("SERVE_SIM_BINARY") {
        anyhow::ensure!(
            !binary.trim().is_empty(),
            "SERVE_SIM_BINARY must not be empty"
        );
        return Ok((binary, arguments));
    }
    let mut command_arguments = vec!["--yes".to_owned(), SERVE_SIM_PACKAGE.to_owned()];
    command_arguments.extend(arguments);
    Ok(("npx".to_owned(), command_arguments))
}

pub(crate) struct Backend {
    config: Result<Option<Config>, String>,
    socket: tokio::sync::Mutex<Option<Socket>>,
    input: std::sync::Arc<tokio::sync::Semaphore>,
}

struct Config {
    base: reqwest::Url,
    client: reqwest::Client,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            config: configuration().map_err(|error| error.to_string()),
            socket: Default::default(),
            input: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
}

fn configuration() -> anyhow::Result<Option<Config>> {
    match std::env::var("DEVICE_IOS_BACKEND").as_deref() {
        Err(_) | Ok("cli") => return Ok(None),
        Ok("serve-sim") => {}
        _ => anyhow::bail!("DEVICE_IOS_BACKEND must be cli or serve-sim"),
    }
    let device = std::env::var("IOS_SIMULATOR_UDID")
        .map_err(|_| anyhow::anyhow!("persistent iOS requires IOS_SIMULATOR_UDID"))?;
    validate_udid(&device)?;
    let base = if let Ok(helper) = std::env::var("SERVE_SIM_HELPER_URL") {
        let base = local_url(&helper)?;
        anyhow::ensure!(
            base.path()
                .trim_end_matches('/')
                .ends_with(&format!("/helper/{device}")),
            "SERVE_SIM_HELPER_URL must identify the configured simulator"
        );
        base
    } else {
        let mut base = local_url(
            &std::env::var("SERVE_SIM_URL").unwrap_or_else(|_| "http://127.0.0.1:3200".to_owned()),
        )?;
        base.set_path(&format!(
            "{}/helper/{device}",
            base.path().trim_end_matches('/')
        ));
        base
    };
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    Ok(Some(Config { base, client }))
}

pub(crate) fn validate_udid(device: &str) -> anyhow::Result<()> {
    let groups = device.split('-').collect::<Vec<_>>();
    anyhow::ensure!(
        groups.len() == 5
            && groups
                .iter()
                .zip([8, 4, 4, 4, 12])
                .all(|(group, length)| group.len() == length
                    && group.chars().all(|ch| ch.is_ascii_hexdigit())),
        "IOS_SIMULATOR_UDID must be a simulator UUID"
    );
    Ok(())
}

pub(crate) fn local_url(value: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    anyhow::ensure!(
        url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "SERVE_SIM_URL must be a loopback HTTP URL without credentials, query or fragment"
    );
    Ok(url)
}

impl Config {
    fn url(&self, endpoint: &str) -> reqwest::Url {
        let mut url = self.base.clone();
        url.set_path(&format!(
            "{}/{endpoint}",
            self.base.path().trim_end_matches('/')
        ));
        url
    }

    async fn response(&self, endpoint: &str) -> anyhow::Result<reqwest::Response> {
        let result = self.client.get(self.url(endpoint)).send().await;
        let response = result.map_err(|error| {
            let reason = if error.is_timeout() {
                "request timed out"
            } else if error.is_connect() {
                "connection failed"
            } else {
                "request failed"
            };
            anyhow::anyhow!("serve-sim {reason}; start serve-sim@0.1.47 for the configured simulator or select cli backend")
        })?;
        anyhow::ensure!(
            response.status().is_success(),
            "serve-sim endpoint {endpoint} is unavailable"
        );
        Ok(response)
    }
}

async fn limited_body(mut response: reqwest::Response, limit: usize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            bytes.len().saturating_add(chunk.len()) <= limit,
            "serve-sim response exceeded byte limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

impl Backend {
    pub(crate) fn for_helper(helper: &str) -> anyhow::Result<Self> {
        let base = local_url(helper)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            config: Ok(Some(Config { base, client })),
            socket: Default::default(),
            input: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }
    pub fn validate_text(&self, text: &str) -> anyhow::Result<()> {
        let mut count = 0;
        for ch in text.chars().filter(|ch| *ch != '\r') {
            key(ch)?;
            count += 1;
        }
        anyhow::ensure!(
            count <= 512,
            "persistent iOS text is limited to 512 US-keyboard characters"
        );
        Ok(())
    }
    pub fn enabled(&self) -> anyhow::Result<bool> {
        self.config
            .as_ref()
            .map(|config| config.is_some())
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    fn config(&self) -> anyhow::Result<&Config> {
        self.config
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("persistent iOS is not enabled"))
    }

    pub async fn status(&self) -> anyhow::Result<String> {
        let config = self.config()?;
        let health: serde_json::Value =
            serde_json::from_slice(&limited_body(config.response("health").await?, 4096).await?)?;
        anyhow::ensure!(health["status"] == "ok", "serve-sim health check failed");
        let mut dimensions: serde_json::Value =
            serde_json::from_slice(&limited_body(config.response("config").await?, 4096).await?)?;
        if dimensions["width"].as_u64() == Some(0) || dimensions["height"].as_u64() == Some(0) {
            // A newly opened native capture can publish its dimensions only
            // after the first subscriber obtains the seed frame.
            self.latest_frame().await?;
            dimensions = serde_json::from_slice(
                &limited_body(config.response("config").await?, 4096).await?,
            )?;
        }
        anyhow::ensure!(
            dimensions["width"].as_u64().is_some_and(|width| width > 0)
                && dimensions["height"]
                    .as_u64()
                    .is_some_and(|height| height > 0),
            "serve-sim capture is not ready"
        );
        Ok(
            json!({ "backend": "serve-sim", "dimensions": dimensions, "ownership": "external" })
                .to_string(),
        )
    }

    pub async fn stop(&self) -> anyhow::Result<String> {
        // Drop instead of waiting for a close handshake with a broken server.
        self.socket.lock().await.take();
        Ok("Disconnected MCP input socket; external serve-sim was not stopped".to_owned())
    }

    async fn connect(&self) -> anyhow::Result<Socket> {
        let mut url = self.config()?.url("ws");
        url.set_scheme("ws")
            .map_err(|_| anyhow::anyhow!("invalid input URL"))?;
        let mut config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
        config.max_message_size = Some(64 * 1024);
        config.max_frame_size = Some(64 * 1024);
        let (socket, _) =
            tokio_tungstenite::connect_async_with_config(url.as_str(), Some(config), false).await?;
        Ok(socket)
    }

    async fn ready_socket(&self) -> anyhow::Result<InputGuard> {
        let permit = self.input.clone().acquire_owned().await?;
        let existing = self.socket.lock().await.take();
        if let Some(mut socket) = existing {
            let probe = tokio::time::timeout(Duration::from_secs(2), async {
                socket.send(Message::Ping(Vec::new().into())).await?;
                while let Some(message) = socket.next().await {
                    match message? {
                        Message::Pong(_) => return Ok::<_, anyhow::Error>(()),
                        Message::Close(_) => anyhow::bail!("input socket closed"),
                        _ => {}
                    }
                }
                anyhow::bail!("input socket disconnected")
            })
            .await;
            if matches!(probe, Ok(Ok(()))) {
                return Ok(InputGuard::new(socket, permit));
            }
            // Safe reconnect: no input has been sent for this operation yet.
        }
        Ok(InputGuard::new(self.connect().await?, permit))
    }

    async fn send(socket: &mut Socket, tag: u8, payload: serde_json::Value) -> anyhow::Result<()> {
        let mut bytes = vec![tag];
        bytes.extend(serde_json::to_vec(&payload)?);
        socket
            .send(Message::Binary(bytes.into()))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "input submission failed; action may have applied, do not retry blindly"
                )
            })
    }

    pub async fn tap(&self, x: f64, y: f64) -> anyhow::Result<String> {
        let mut input = self.ready_socket().await?;
        input.touch = Some((x, y));
        Self::send(input.socket(), 3, json!({"type":"begin", "x":x, "y":y})).await?;
        tokio::time::sleep(Duration::from_millis(40)).await;
        Self::send(input.socket(), 3, json!({"type":"end", "x":x, "y":y})).await?;
        input.touch = None;
        *self.socket.lock().await = input.socket.take();
        Ok("Tap submitted; application rendering is not acknowledged".to_owned())
    }

    pub async fn swipe(&self, points: (f64, f64, f64, f64)) -> anyhow::Result<String> {
        let (x1, y1, x2, y2) = points;
        let mut input = self.ready_socket().await?;
        input.touch = Some((x1, y1));
        Self::send(input.socket(), 3, json!({"type":"begin", "x":x1, "y":y1})).await?;
        for step in 1..=10 {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let fraction = f64::from(step) / 10.0;
            input.touch = Some((x1 + (x2 - x1) * fraction, y1 + (y2 - y1) * fraction));
            Self::send(
                input.socket(),
                3,
                json!({"type":"move", "x":x1+(x2-x1)*fraction, "y":y1+(y2-y1)*fraction}),
            )
            .await?;
        }
        Self::send(input.socket(), 3, json!({"type":"end", "x":x2, "y":y2})).await?;
        input.touch = None;
        *self.socket.lock().await = input.socket.take();
        Ok("Swipe submitted; application rendering is not acknowledged".to_owned())
    }

    pub async fn type_text(&self, text: &str) -> anyhow::Result<String> {
        self.validate_text(text)?;
        let keys = text
            .chars()
            .filter(|ch| *ch != '\r')
            .map(key)
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(
            keys.len() <= 512,
            "persistent iOS text is limited to 512 US-keyboard characters"
        );
        let mut input = self.ready_socket().await?;
        for (usage, shift) in keys {
            let events = if shift {
                vec![("down", 0xe1), ("down", usage), ("up", usage), ("up", 0xe1)]
            } else {
                vec![("down", usage), ("up", usage)]
            };
            for (phase, usage) in events {
                if phase == "down" {
                    input.keys.push(usage);
                }
                Self::send(input.socket(), 6, json!({"type":phase,"usage":usage})).await?;
                if phase == "up" {
                    input.keys.retain(|key| *key != usage);
                }
                tokio::time::sleep(Duration::from_millis(4)).await;
            }
        }
        *self.socket.lock().await = input.socket.take();
        Ok("Text submitted; application rendering is not acknowledged".to_owned())
    }

    pub async fn rotate(
        &self,
        orientation: crate::platform::Orientation,
    ) -> anyhow::Result<String> {
        let mut input = self.ready_socket().await?;
        Self::send(
            input.socket(),
            7,
            json!({"orientation": orientation.serve_sim_value()}),
        )
        .await?;
        *self.socket.lock().await = input.socket.take();
        Ok("Orientation submitted; simulator acknowledgement is not available".to_owned())
    }

    pub async fn accessibility(&self) -> anyhow::Result<serde_json::Value> {
        let config = self.config()?;
        Ok(serde_json::from_slice(
            &limited_body(config.response("ax").await?, 1024 * 1024).await?,
        )?)
    }

    /// Read one frame and close the subscription: no permanent stream/CPU load
    /// in this MCP process. serve-sim may replay cached pixels on static screens.
    pub async fn latest_frame(&self) -> anyhow::Result<Vec<u8>> {
        let config = self.config()?;
        let mut response = config.response("stream.mjpeg").await?;
        let mut buffer = Vec::new();
        loop {
            let chunk = response
                .chunk()
                .await?
                .ok_or_else(|| anyhow::anyhow!("MJPEG stream ended before a frame"))?;
            anyhow::ensure!(
                buffer.len().saturating_add(chunk.len()) <= crate::process::MAX_OUTPUT_BYTES,
                "MJPEG frame exceeded byte limit"
            );
            buffer.extend_from_slice(&chunk);
            if let Some(frame) = parse_frame(&buffer)? {
                return Ok(frame.to_vec());
            }
        }
    }
}

struct InputGuard {
    socket: Option<Socket>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    touch: Option<(f64, f64)>,
    keys: Vec<u8>,
}

impl InputGuard {
    fn new(socket: Socket, permit: tokio::sync::OwnedSemaphorePermit) -> Self {
        Self {
            socket: Some(socket),
            permit: Some(permit),
            touch: None,
            keys: Vec::new(),
        }
    }
    fn socket(&mut self) -> &mut Socket {
        self.socket.as_mut().expect("input guard owns its socket")
    }
}

impl Drop for InputGuard {
    fn drop(&mut self) {
        if self.touch.is_none() && self.keys.is_empty() {
            return;
        }
        let Some(mut socket) = self.socket.take() else {
            return;
        };
        let permit = self.permit.take();
        let touch = self.touch.take();
        let keys = std::mem::take(&mut self.keys);
        tokio::spawn(async move {
            let _permit = permit;
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                if let Some((x, y)) = touch {
                    let _ = Backend::send(&mut socket, 3, json!({"type":"end","x":x,"y":y})).await;
                }
                for usage in keys {
                    let _ = Backend::send(&mut socket, 6, json!({"type":"up","usage":usage})).await;
                }
                let _ = socket.close(None).await;
            })
            .await;
        });
    }
}

fn parse_frame(buffer: &[u8]) -> anyhow::Result<Option<&[u8]>> {
    let Some(header_end) = buffer.windows(4).position(|part| part == b"\r\n\r\n") else {
        anyhow::ensure!(
            buffer.len() <= 64 * 1024,
            "MJPEG headers exceeded byte limit"
        );
        return Ok(None);
    };
    anyhow::ensure!(header_end <= 64 * 1024, "MJPEG headers exceeded byte limit");
    let header = std::str::from_utf8(&buffer[..header_end])?;
    let length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("Content-Length")
                .then_some(value.trim())
        })
        .ok_or_else(|| anyhow::anyhow!("MJPEG frame has no Content-Length"))?
        .parse::<usize>()?;
    anyhow::ensure!(
        length > 0 && length <= crate::process::MAX_OUTPUT_BYTES - header_end - 4,
        "invalid MJPEG frame length"
    );
    Ok(buffer.get(header_end + 4..header_end + 4 + length))
}

fn key(ch: char) -> anyhow::Result<(u8, bool)> {
    if ch.is_ascii_alphabetic() {
        return Ok((
            ch.to_ascii_lowercase() as u8 - b'a' + 4,
            ch.is_ascii_uppercase(),
        ));
    }
    if let Some(index) = "1234567890".chars().position(|value| value == ch) {
        return Ok((0x1e + index as u8, false));
    }
    if let Some(index) = "!@#$%^&*()".chars().position(|value| value == ch) {
        return Ok((0x1e + index as u8, true));
    }
    for (plain, shifted, usage) in [
        ('-', '_', 0x2d),
        ('=', '+', 0x2e),
        ('[', '{', 0x2f),
        (']', '}', 0x30),
        ('\\', '|', 0x31),
        (';', ':', 0x33),
        ('\'', '"', 0x34),
        ('`', '~', 0x35),
        (',', '<', 0x36),
        ('.', '>', 0x37),
        ('/', '?', 0x38),
    ] {
        if ch == plain {
            return Ok((usage, false));
        }
        if ch == shifted {
            return Ok((usage, true));
        }
    }
    match ch {
        ' ' => Ok((0x2c, false)),
        '\n' => Ok((0x28, false)),
        '\t' => Ok((0x2b, false)),
        _ => anyhow::bail!(
            "persistent iOS typing supports US-keyboard ASCII only; no input was submitted"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mock_backend() -> (
        Backend,
        tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = connections.clone();
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let events = events.clone();
                tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(message)) = socket.next().await {
                        match message {
                            Message::Binary(bytes) => {
                                let tag = bytes[0];
                                let mut value: serde_json::Value =
                                    serde_json::from_slice(&bytes[1..]).unwrap();
                                value["_tag"] = tag.into();
                                let _ = events.send(value);
                            }
                            Message::Ping(bytes) => {
                                let _ = socket.send(Message::Pong(bytes)).await;
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                });
            }
        });
        let backend = Backend {
            config: Ok(Some(Config {
                base: local_url(&format!("http://{address}")).unwrap(),
                client: reqwest::Client::builder().no_proxy().build().unwrap(),
            })),
            socket: Default::default(),
            input: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        };
        (backend, receiver, connections, server)
    }

    #[tokio::test]
    async fn reuses_one_websocket_for_taps_and_typing() {
        let (backend, mut events, connections, server) = mock_backend().await;
        backend.tap(0.1, 0.2).await.unwrap();
        backend.tap(0.3, 0.4).await.unwrap();
        backend.type_text("Aa!").await.unwrap();
        let mut received = Vec::new();
        for _ in 0..14 {
            received.push(
                tokio::time::timeout(Duration::from_secs(1), events.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(received[0]["type"], "begin");
        assert_eq!(received[1]["type"], "end");
        assert_eq!(received[4]["usage"], 0xe1);
        backend.stop().await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn submits_orientation_over_the_existing_input_websocket() {
        let (backend, mut events, connections, server) = mock_backend().await;

        backend
            .rotate(crate::platform::Orientation::LandscapeRight)
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(event["_tag"], 7);
        assert_eq!(event["orientation"], "landscape_right");
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 1);
        backend.stop().await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn cancellation_releases_touch_before_the_next_input() {
        let (backend, mut events, _, server) = mock_backend().await;
        let result = tokio::time::timeout(
            Duration::from_millis(50),
            backend.swipe((0.1, 0.2, 0.8, 0.9)),
        )
        .await;
        assert!(result.is_err());
        backend.tap(0.5, 0.5).await.unwrap();
        let mut received = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            let finished = event["type"] == "end" && event["x"] == 0.5;
            received.push(event);
            if finished {
                break;
            }
        }
        let next_begin = received
            .iter()
            .position(|event| event["type"] == "begin" && event["x"] == 0.5)
            .unwrap();
        assert!(
            received[..next_begin]
                .iter()
                .any(|event| event["type"] == "end")
        );
        backend.stop().await.unwrap();
        server.abort();
    }

    #[test]
    fn rejects_nonlocal_or_credentialed_urls() {
        for url in [
            "http://example.com",
            "https://localhost",
            "http://user:pass@localhost",
            "http://localhost?query=1",
        ] {
            assert!(local_url(url).is_err());
        }
        assert!(local_url("http://127.0.0.1:3100").is_ok());
        assert!(validate_udid("07883E8D-CABD-4D9B-8B16-09B4A8987EEF").is_ok());
        assert!(validate_udid("------------------------------------").is_err());
    }

    #[test]
    fn parses_split_mjpeg_frames_and_rejects_large_lengths() {
        assert!(
            parse_frame(b"--frame\r\nContent-Length: 3\r\n\r\na")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            parse_frame(b"--frame\r\nContent-Length: 3\r\n\r\nabc\r\n").unwrap(),
            Some(b"abc".as_slice())
        );
        assert!(parse_frame(b"--frame\r\nContent-Length: 999999999\r\n\r\n").is_err());
    }

    #[test]
    fn maps_us_keys_without_exposing_unsupported_text() {
        assert_eq!(key('A').unwrap(), (4, true));
        assert_eq!(key('0').unwrap(), (0x27, false));
        assert_eq!(key('?').unwrap(), (0x38, true));
        assert!(key('é').is_err());
    }
}
