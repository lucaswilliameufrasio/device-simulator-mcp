use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone)]
pub(crate) struct Frame {
    pub bytes: Arc<Vec<u8>>,
    pub description: String,
    pub received: Instant,
    pub received_at: SystemTime,
}

#[derive(Default)]
pub(crate) struct Cache {
    entry: Option<(String, Frame)>,
}

impl Cache {
    pub fn get(&self, key: &str, max_age: Duration) -> Option<Frame> {
        self.entry
            .as_ref()
            .filter(|(source, frame)| {
                source == key && !max_age.is_zero() && frame.received.elapsed() <= max_age
            })
            .map(|(_, frame)| frame.clone())
    }

    pub fn store(&mut self, key: String, frame: Frame) {
        if frame.bytes.len() <= crate::process::MAX_OUTPUT_BYTES {
            self.entry = Some((key, frame));
        } else {
            self.clear();
        }
    }

    pub fn clear(&mut self) {
        self.entry = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Frame {
        Frame {
            bytes: Arc::new(vec![1]),
            description: "test".to_owned(),
            received: Instant::now(),
            received_at: SystemTime::now(),
        }
    }

    #[test]
    fn cache_reuse_requires_matching_source_and_positive_age() {
        let mut cache = Cache::default();
        cache.store("ios".to_owned(), frame());
        assert!(cache.get("ios", Duration::from_secs(1)).is_some());
        assert!(cache.get("android", Duration::from_secs(1)).is_none());
        assert!(cache.get("ios", Duration::ZERO).is_none());
        cache.clear();
        assert!(cache.get("ios", Duration::from_secs(1)).is_none());
    }

    #[test]
    fn expired_frames_are_not_relabelled_fresh() {
        let mut cache = Cache::default();
        let mut old = frame();
        old.received = Instant::now() - Duration::from_secs(10);
        cache.store("ios".to_owned(), old);
        assert!(cache.get("ios", Duration::from_secs(5)).is_none());
        cache.store("android".to_owned(), frame());
        assert!(cache.get("ios", Duration::from_secs(20)).is_none());
    }
}
