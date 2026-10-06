use std::io::Cursor;

use image::{ImageFormat, ImageReader};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Format {
    #[default]
    Png,
    Jpeg,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Crop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct ImageOptions {
    /// Maximum output edge in pixels (1..=4096); no upscaling.
    pub max_dimension: Option<u32>,
    #[serde(default)]
    pub format: Format,
    /// JPEG quality (1..=100), default 80. Only valid with JPEG.
    pub quality: Option<u8>,
    /// Crop in source screenshot pixels, before resizing.
    pub crop: Option<Crop>,
}

impl ImageOptions {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(max) = self.max_dimension {
            anyhow::ensure!(
                (1..=4096).contains(&max),
                "max_dimension must be between 1 and 4096"
            );
        }
        if let Some(quality) = self.quality {
            anyhow::ensure!(
                (1..=100).contains(&quality),
                "quality must be between 1 and 100"
            );
            anyhow::ensure!(
                matches!(self.format, Format::Jpeg),
                "quality requires JPEG format"
            );
        }
        if let Some(crop) = &self.crop {
            anyhow::ensure!(
                crop.width > 0 && crop.height > 0,
                "crop dimensions must be positive"
            );
        }
        Ok(())
    }

    pub fn is_default(&self) -> bool {
        self.max_dimension.is_none()
            && matches!(self.format, Format::Png)
            && self.quality.is_none()
            && self.crop.is_none()
    }
}

pub(crate) struct Observation {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub metadata: serde_json::Value,
}

// Keep CPU jobs bounded even if the async caller times out during encoding.
static IMAGE_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

pub(crate) async fn prepare(bytes: Vec<u8>, options: ImageOptions) -> anyhow::Result<Observation> {
    options.validate()?;
    let started = std::time::Instant::now();
    let input_bytes = bytes.len();
    let permit = IMAGE_WORKERS.acquire().await?;
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        transform(bytes, options)
    })
    .await?;
    tracing::debug!(
        elapsed_ms = started.elapsed().as_millis(),
        input_bytes,
        output_bytes = result.as_ref().map(|image| image.bytes.len()).unwrap_or(0),
        success = result.is_ok(),
        "image processing completed"
    );
    result
}

fn transform(bytes: Vec<u8>, options: ImageOptions) -> anyhow::Result<Observation> {
    anyhow::ensure!(
        bytes.len() <= crate::process::MAX_OUTPUT_BYTES,
        "image exceeded byte limit"
    );
    let mut reader = ImageReader::new(Cursor::new(&bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let mut image = reader.decode()?;
    let source_width = image.width();
    let source_height = image.height();
    if let Some(crop) = &options.crop {
        anyhow::ensure!(
            crop.x
                .checked_add(crop.width)
                .is_some_and(|end| end <= source_width)
                && crop
                    .y
                    .checked_add(crop.height)
                    .is_some_and(|end| end <= source_height),
            "crop exceeds screenshot bounds"
        );
        image = image.crop_imm(crop.x, crop.y, crop.width, crop.height);
    }
    if let Some(max) = options.max_dimension
        && (image.width() > max || image.height() > max)
    {
        image = image.resize(max, max, image::imageops::FilterType::Triangle);
    }
    let width = image.width();
    let height = image.height();
    let crop = options.crop.clone().unwrap_or(Crop {
        x: 0,
        y: 0,
        width: source_width,
        height: source_height,
    });
    let source_x = f64::from(source_width.saturating_sub(1).max(1));
    let source_y = f64::from(source_height.saturating_sub(1).max(1));
    let mut output = Cursor::new(Vec::new());
    let mime = match options.format {
        Format::Png => {
            image.write_to(&mut output, ImageFormat::Png)?;
            "image/png"
        }
        Format::Jpeg => {
            image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut output,
                options.quality.unwrap_or(80),
            )
            .encode_image(&image.to_rgb8())?;
            "image/jpeg"
        }
    };
    let bytes = output.into_inner();
    anyhow::ensure!(
        bytes.len() <= crate::process::MAX_OUTPUT_BYTES,
        "encoded image exceeded byte limit"
    );
    Ok(Observation {
        bytes,
        mime,
        metadata: serde_json::json!({
            "width": width, "height": height, "source_width": source_width,
            "source_height": source_height, "crop": options.crop,
            "coordinate_space": "normalized_full_device",
            "image_to_device_normalized": {
                "x_offset": f64::from(crop.x) / source_x,
                "y_offset": f64::from(crop.y) / source_y,
                "x_scale": f64::from(crop.width.saturating_sub(1)) / source_x,
                "y_scale": f64::from(crop.height.saturating_sub(1)) / source_y,
            },
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(100, 200)
            .write_to(&mut output, ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    #[test]
    fn validates_options_before_capture() {
        let options = ImageOptions {
            max_dimension: Some(0),
            ..Default::default()
        };
        assert!(options.validate().is_err());
        let options = ImageOptions {
            quality: Some(80),
            ..Default::default()
        };
        assert!(options.validate().is_err());
    }

    #[test]
    fn crops_then_scales_and_encodes_jpeg() {
        let options = ImageOptions {
            format: Format::Jpeg,
            max_dimension: Some(20),
            crop: Some(Crop {
                x: 0,
                y: 10,
                width: 50,
                height: 100,
            }),
            ..Default::default()
        };
        let result = transform(png(), options).unwrap();
        assert_eq!(result.mime, "image/jpeg");
        assert_eq!(result.metadata["width"], 10);
        assert_eq!(result.metadata["height"], 20);
        assert!(image::load_from_memory(&result.bytes).is_ok());
    }

    #[test]
    fn rejects_out_of_bounds_crops_and_overflow() {
        let options = ImageOptions {
            crop: Some(Crop {
                x: u32::MAX,
                y: 0,
                width: 2,
                height: 2,
            }),
            ..Default::default()
        };
        assert!(transform(png(), options).is_err());
    }
}
