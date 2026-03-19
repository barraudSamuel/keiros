use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct Debouncer {
    window: Duration,
    pending: HashMap<PathBuf, Instant>,
}

impl Debouncer {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: HashMap::new(),
        }
    }

    pub fn record(&mut self, path: PathBuf, seen_at: Instant) {
        self.pending.insert(path, seen_at);
    }

    pub fn drain_ready(&mut self, now: Instant) -> Vec<PathBuf> {
        let ready: Vec<PathBuf> = self
            .pending
            .iter()
            .filter_map(|(path, seen_at)| {
                if now.duration_since(*seen_at) >= self.window {
                    Some(path.clone())
                } else {
                    None
                }
            })
            .collect();

        for path in &ready {
            self.pending.remove(path);
        }

        let mut ready = ready;
        ready.sort();
        ready
    }
}
