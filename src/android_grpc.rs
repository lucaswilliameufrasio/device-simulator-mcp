//! Experimental Android Emulator gRPC backend. Wire fields follow the Android
//! SDK emulator/lib/emulator_controller.proto (android.emulation.control).
//! Only a small compatible subset is decoded; protobuf ignores unknown fields.
//! No authentication is disabled and no credentials are printed.
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::io::AsyncReadExt;

use prost::Message;
use tonic::{
    Request, client::Grpc, codegen::http::uri::PathAndQuery, metadata::MetadataValue,
    transport::Channel,
};

pub(crate) struct Backend {
    config: Result<Option<Config>, String>,
    channel: tokio::sync::Mutex<Option<Channel>>,
    input: Arc<tokio::sync::Semaphore>,
    next_touch: AtomicU32,
}

struct Config {
    endpoint: String,
    token_file: PathBuf,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            config: configuration().map_err(|error| error.to_string()),
            channel: Default::default(),
            input: Arc::new(tokio::sync::Semaphore::new(1)),
            next_touch: AtomicU32::new(100),
        }
    }
}

fn configuration() -> anyhow::Result<Option<Config>> {
    match std::env::var("DEVICE_ANDROID_BACKEND").as_deref() {
        Err(_) | Ok("adb") => return Ok(None),
        Ok("grpc") => {}
        _ => anyhow::bail!("DEVICE_ANDROID_BACKEND must be adb or grpc"),
    }
    let endpoint = std::env::var("ANDROID_GRPC_ENDPOINT")
        .map_err(|_| anyhow::anyhow!("experimental Android gRPC requires ANDROID_GRPC_ENDPOINT"))?;
    crate::ios::local_url(&endpoint)?;
    let token_file = std::env::var_os("ANDROID_GRPC_TOKEN_FILE").ok_or_else(|| {
        anyhow::anyhow!(
            "Android gRPC requires ANDROID_GRPC_TOKEN_FILE; authentication is never disabled"
        )
    })?;
    Ok(Some(Config {
        endpoint,
        token_file: token_file.into(),
    }))
}

impl Backend {
    pub fn enabled(&self) -> anyhow::Result<bool> {
        self.config
            .as_ref()
            .map(|config| config.is_some())
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    async fn call<T, U>(&self, method: &'static str, message: T) -> anyhow::Result<U>
    where
        T: Message + Default + Send + Sync + 'static,
        U: Message + Default + Send + Sync + 'static,
    {
        let config = self
            .config
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Android gRPC is not enabled"))?;
        // Load credentials at runtime only. Bounded reads and generic errors
        // keep values out of diagnostics and MCP results.
        let file = tokio::fs::File::open(&config.token_file)
            .await
            .map_err(|_| anyhow::anyhow!("could not load Android gRPC credential file"))?;
        let mut token = Vec::new();
        file.take(8193)
            .read_to_end(&mut token)
            .await
            .map_err(|_| anyhow::anyhow!("could not load Android gRPC credential file"))?;
        anyhow::ensure!(
            !token.is_empty() && token.len() <= 8192,
            "invalid Android gRPC credential file"
        );
        let token = std::str::from_utf8(&token)
            .map_err(|_| anyhow::anyhow!("invalid Android gRPC credential encoding"))?;
        let authorization: MetadataValue<_> = format!("Bearer {}", token.trim())
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid Android gRPC authorization metadata"))?;
        let mut connection = self.channel.lock().await;
        let channel = match connection.as_ref() {
            Some(channel) => channel.clone(),
            None => {
                let channel = Channel::from_shared(config.endpoint.clone())?
                    .connect_timeout(Duration::from_secs(3)).timeout(Duration::from_secs(10))
                    .connect().await.map_err(|_| anyhow::anyhow!("Android gRPC endpoint unavailable; verify emulator endpoint or select adb"))?;
                *connection = Some(channel.clone());
                channel
            }
        };
        drop(connection);
        let mut client =
            Grpc::new(channel).max_decoding_message_size(crate::process::MAX_OUTPUT_BYTES + 4096);
        client
            .ready()
            .await
            .map_err(|_| anyhow::anyhow!("Android gRPC transport unavailable"))?;
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert("authorization", authorization);
        let path = PathAndQuery::from_static(method);
        let codec = tonic_prost::ProstCodec::<T, U>::default();
        client
            .unary(request, path, codec)
            .await
            .map(|response| response.into_inner())
            .map_err(|status| {
                anyhow::anyhow!(
                    "Android gRPC operation failed ({:?}); no automatic input retry",
                    status.code()
                )
            })
    }

    async fn image(&self) -> anyhow::Result<Image> {
        let image: Image = self
            .call(
                "/android.emulation.control.EmulatorController/getScreenshot",
                ImageFormat::default(),
            )
            .await?;
        let (width, height) = image.dimensions()?;
        anyhow::ensure!(
            image
                .format
                .as_ref()
                .is_none_or(|format| format.format == 0),
            "Android gRPC did not return PNG"
        );
        anyhow::ensure!(
            width <= 8192
                && height <= 8192
                && !image.image.is_empty()
                && image.image.len() <= crate::process::MAX_OUTPUT_BYTES,
            "invalid Android gRPC screenshot"
        );
        Ok(image)
    }

    pub async fn capture(&self) -> anyhow::Result<Vec<u8>> {
        Ok(self.image().await?.image)
    }

    pub async fn status(&self) -> anyhow::Result<String> {
        let image = self.image().await?;
        let (width, height) = image.dimensions()?;
        Ok(
            serde_json::json!({"backend":"grpc", "experimental":true, "width":width,
            "height":height, "timestamp_us":image.timestamp_us, "ownership":"external"})
            .to_string(),
        )
    }

    async fn touch(&self, x: i32, y: i32, identifier: i32, pressure: i32) -> anyhow::Result<()> {
        let _: Empty = self
            .call(
                "/android.emulation.control.EmulatorController/sendTouch",
                TouchEvent {
                    touches: vec![Touch {
                        x,
                        y,
                        identifier,
                        pressure,
                    }],
                    display: 0,
                },
            )
            .await?;
        Ok(())
    }

    pub async fn tap(self: &Arc<Self>, x: f64, y: f64) -> anyhow::Result<String> {
        let permit = self.input.clone().acquire_owned().await?;
        let dimensions = self.image().await?.dimensions()?;
        let (x, y) = pixels(x, y, dimensions);
        let mut touch = TouchGuard {
            backend: self.clone(),
            x,
            y,
            identifier: (self.next_touch.fetch_add(1, Ordering::Relaxed) % 1_000_000_000) as i32,
            permit: Some(permit),
            armed: true,
        };
        self.touch(x, y, touch.identifier, 1).await?;
        tokio::time::sleep(Duration::from_millis(40)).await;
        self.touch(x, y, touch.identifier, 0).await?;
        touch.armed = false;
        Ok("Tap scheduled; application rendering is not acknowledged".to_owned())
    }

    pub async fn swipe(self: &Arc<Self>, points: (f64, f64, f64, f64)) -> anyhow::Result<String> {
        let permit = self.input.clone().acquire_owned().await?;
        let dimensions = self.image().await?.dimensions()?;
        let (x1, y1, x2, y2) = points;
        let (x, y) = pixels(x1, y1, dimensions);
        let mut touch = TouchGuard {
            backend: self.clone(),
            x,
            y,
            identifier: (self.next_touch.fetch_add(1, Ordering::Relaxed) % 1_000_000_000) as i32,
            permit: Some(permit),
            armed: true,
        };
        self.touch(x, y, touch.identifier, 1).await?;
        for step in 1..=10 {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let fraction = f64::from(step) / 10.0;
            let (x, y) = pixels(
                x1 + (x2 - x1) * fraction,
                y1 + (y2 - y1) * fraction,
                dimensions,
            );
            touch.x = x;
            touch.y = y;
            self.touch(x, y, touch.identifier, 1).await?;
        }
        let (x, y) = pixels(x2, y2, dimensions);
        self.touch(x, y, touch.identifier, 0).await?;
        touch.armed = false;
        Ok("Swipe scheduled; application rendering is not acknowledged".to_owned())
    }

    pub async fn type_text(self: &Arc<Self>, text: &str) -> anyhow::Result<String> {
        validate_text(text)?;
        let _permit = self.input.clone().acquire_owned().await?;
        let _: Empty = self
            .call(
                "/android.emulation.control.EmulatorController/sendKey",
                KeyboardEvent {
                    text: text.to_owned(),
                },
            )
            .await?;
        Ok("Text scheduled; application rendering is not acknowledged".to_owned())
    }
}

struct TouchGuard {
    backend: Arc<Backend>,
    x: i32,
    y: i32,
    identifier: i32,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    armed: bool,
}

impl Drop for TouchGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let backend = self.backend.clone();
        let (x, y, identifier) = (self.x, self.y, self.identifier);
        let permit = self.permit.take();
        // Only release our contact; never replay a gesture. The input permit
        // prevents a following gesture from racing this bounded cleanup.
        tokio::spawn(async move {
            let _permit = permit;
            let _ =
                tokio::time::timeout(Duration::from_secs(1), backend.touch(x, y, identifier, 0))
                    .await;
        });
    }
}

pub(crate) fn validate_text(text: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        text.chars()
            .all(|ch| ch.is_ascii_graphic() || matches!(ch, ' ' | '\n' | '\t')),
        "experimental Android gRPC typing supports US-keyboard ASCII only"
    );
    Ok(())
}

fn pixels(x: f64, y: f64, (width, height): (u32, u32)) -> (i32, i32) {
    (
        (x * f64::from(width - 1)).round() as i32,
        (y * f64::from(height - 1)).round() as i32,
    )
}

#[derive(Clone, PartialEq, Message)]
struct Empty {}

#[derive(Clone, PartialEq, Message)]
struct ImageFormat {
    #[prost(int32, tag = "1")]
    format: i32,
    #[prost(uint32, tag = "3")]
    width: u32,
    #[prost(uint32, tag = "4")]
    height: u32,
}

#[derive(Clone, PartialEq, Message)]
struct Image {
    #[prost(message, optional, tag = "1")]
    format: Option<ImageFormat>,
    #[prost(uint32, tag = "2")]
    width: u32,
    #[prost(uint32, tag = "3")]
    height: u32,
    #[prost(bytes = "vec", tag = "4")]
    image: Vec<u8>,
    #[prost(uint64, tag = "6")]
    timestamp_us: u64,
}

impl Image {
    fn dimensions(&self) -> anyhow::Result<(u32, u32)> {
        let dimensions = self
            .format
            .as_ref()
            .map(|format| (format.width, format.height))
            .filter(|(width, height)| *width > 0 && *height > 0)
            .unwrap_or((self.width, self.height));
        anyhow::ensure!(
            dimensions.0 > 0 && dimensions.1 > 0,
            "Android display is inactive"
        );
        Ok(dimensions)
    }
}

#[derive(Clone, PartialEq, Message)]
struct Touch {
    #[prost(int32, tag = "1")]
    x: i32,
    #[prost(int32, tag = "2")]
    y: i32,
    #[prost(int32, tag = "3")]
    identifier: i32,
    #[prost(int32, tag = "4")]
    pressure: i32,
}

#[derive(Clone, PartialEq, Message)]
struct TouchEvent {
    #[prost(message, repeated, tag = "1")]
    touches: Vec<Touch>,
    #[prost(int32, tag = "2")]
    display: i32,
}

#[derive(Clone, PartialEq, Message)]
struct KeyboardEvent {
    #[prost(string, tag = "5")]
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_published_keyboard_and_touch_wire_tags() {
        assert_eq!(
            KeyboardEvent {
                text: "a".to_owned()
            }
            .encode_to_vec(),
            vec![0x2a, 1, b'a']
        );
        assert_eq!(
            Touch {
                x: 1,
                y: 2,
                identifier: 9,
                pressure: 1
            }
            .encode_to_vec(),
            vec![8, 1, 16, 2, 24, 9, 32, 1]
        );
    }

    #[test]
    fn decodes_image_fields_and_ignores_unknown_fields() {
        let image = Image::decode([16, 100, 24, 100, 34, 3, 1, 2, 3, 40, 8].as_slice()).unwrap();
        assert_eq!(image.dimensions().unwrap(), (100, 100));
        assert_eq!(image.image, [1, 2, 3]);
    }

    #[test]
    fn maps_normalized_edges_to_valid_pixels() {
        assert_eq!(pixels(1.0, 1.0, (1080, 2400)), (1079, 2399));
        assert!(Image::default().dimensions().is_err());
        assert!(validate_text("á").is_err());
    }
}
