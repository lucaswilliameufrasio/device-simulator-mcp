use image::ImageReader;
use std::io::Cursor;

#[derive(Clone, Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Condition {
    ElementPresent {
        label: Option<String>,
        identifier: Option<String>,
    },
    VisualChange,
    VisualStability {
        /// Number of consecutive equal fresh samples (2..=8), default 3.
        stable_samples: Option<u8>,
    },
}

impl Condition {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Self::VisualStability { stable_samples } = self {
            anyhow::ensure!(
                (2..=8).contains(&stable_samples.unwrap_or(3)),
                "stable_samples must be between 2 and 8"
            );
        }
        if let Self::ElementPresent { label, identifier } = self {
            anyhow::ensure!(
                label.is_some() || identifier.is_some(),
                "element_present requires label or identifier"
            );
            for selector in [label, identifier].into_iter().flatten() {
                anyhow::ensure!(
                    !selector.is_empty() && selector.len() <= 256,
                    "element selectors must contain between 1 and 256 bytes"
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Signature {
    dimensions: (u32, u32),
    pixels: Vec<u8>,
}

pub(crate) async fn signature(bytes: Vec<u8>) -> anyhow::Result<Signature> {
    static WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let permit = WORKERS.acquire().await?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        let image = reader.decode()?;
        Ok(Signature {
            dimensions: (image.width(), image.height()),
            pixels: image.thumbnail_exact(64, 64).to_luma8().into_raw(),
        })
    })
    .await?
}

pub(crate) struct Tracker {
    condition: Condition,
    previous: Option<Signature>,
    equal_samples: u8,
}

impl Tracker {
    pub fn new(condition: Condition, baseline: Option<Signature>) -> Self {
        Self {
            condition,
            previous: baseline,
            equal_samples: 0,
        }
    }

    pub fn observe(&mut self, signature: Signature) -> bool {
        match self.condition {
            Condition::ElementPresent { .. } => false,
            Condition::VisualChange => self.previous.as_ref().is_some_and(|old| old != &signature),
            Condition::VisualStability { stable_samples } => {
                self.equal_samples = if self.previous.as_ref() == Some(&signature) {
                    self.equal_samples + 1
                } else {
                    1
                };
                self.previous = Some(signature);
                self.equal_samples >= stable_samples.unwrap_or(3)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(value: u8) -> Signature {
        Signature {
            dimensions: (100, 200),
            pixels: vec![value],
        }
    }

    #[test]
    fn stability_requires_consecutive_post_action_samples() {
        let mut tracker = Tracker::new(
            Condition::VisualStability {
                stable_samples: Some(3),
            },
            None,
        );
        assert!(!tracker.observe(sample(1)));
        assert!(!tracker.observe(sample(1)));
        assert!(!tracker.observe(sample(2)));
        assert!(!tracker.observe(sample(2)));
        assert!(tracker.observe(sample(2)));
    }

    #[test]
    fn change_requires_baseline_and_detects_rotation() {
        let mut tracker = Tracker::new(Condition::VisualChange, Some(sample(1)));
        assert!(!tracker.observe(sample(1)));
        assert!(tracker.observe(sample(2)));
        let mut rotated = sample(1);
        rotated.dimensions = (200, 100);
        assert!(tracker.observe(rotated));
        assert!(!Tracker::new(Condition::VisualChange, None).observe(sample(2)));
    }
}
