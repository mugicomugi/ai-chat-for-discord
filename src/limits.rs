use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub struct Limits {
    channels: Arc<Mutex<HashSet<u64>>>,
    semaphore: Arc<Semaphore>,
}

pub struct Lease {
    channel: u64,
    channels: Arc<Mutex<HashSet<u64>>>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Busy {
    Channel,
    Global,
}

impl Limits {
    pub fn new(max: usize) -> Self {
        Self {
            channels: Arc::default(),
            semaphore: Arc::new(Semaphore::new(max)),
        }
    }

    pub fn enter(&self, channel: u64) -> Result<Lease, Busy> {
        let mut channels = self.channels.lock().expect("channel mutex poisoned");
        if channels.contains(&channel) {
            return Err(Busy::Channel);
        }
        let permit = self
            .semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| Busy::Global)?;
        channels.insert(channel);
        Ok(Lease {
            channel,
            channels: self.channels.clone(),
            _permit: permit,
        })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.channels
            .lock()
            .expect("channel mutex poisoned")
            .remove(&self.channel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn concurrency_and_cancellation_release() {
        let limits = Limits::new(4);
        let first = limits.enter(1).unwrap();
        assert!(matches!(limits.enter(1), Err(Busy::Channel)));
        let others: Vec<_> = (2..=4).map(|id| limits.enter(id).unwrap()).collect();
        assert!(matches!(limits.enter(5), Err(Busy::Global)));
        drop(first);
        assert!(limits.enter(5).is_ok());
        drop(others);
        let result = tokio::time::timeout(std::time::Duration::from_millis(1), async {
            let _lease = limits.enter(1).unwrap();
            std::future::pending::<()>().await;
        })
        .await;
        assert!(result.is_err());
        assert!(limits.enter(1).is_ok());
    }
}
