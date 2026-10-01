use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// One generation at a time per Discord channel and per web user; Discord and the web share the
/// global limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Channel(u64),
    User(u64),
}

pub struct Limits {
    keys: Arc<Mutex<HashSet<Key>>>,
    semaphore: Arc<Semaphore>,
}

pub struct Lease {
    key: Key,
    keys: Arc<Mutex<HashSet<Key>>>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Busy {
    /// The same channel or user already has a generation running.
    Key,
    Global,
}

impl Limits {
    pub fn new(max: usize) -> Self {
        Self {
            keys: Arc::default(),
            semaphore: Arc::new(Semaphore::new(max)),
        }
    }

    pub fn enter(&self, key: Key) -> Result<Lease, Busy> {
        let mut keys = self.keys.lock().expect("limits mutex poisoned");
        if keys.contains(&key) {
            return Err(Busy::Key);
        }
        let permit = self
            .semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| Busy::Global)?;
        keys.insert(key);
        Ok(Lease {
            key,
            keys: self.keys.clone(),
            _permit: permit,
        })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.keys
            .lock()
            .expect("limits mutex poisoned")
            .remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn concurrency_and_cancellation_release() {
        let limits = Limits::new(4);
        let first = limits.enter(Key::Channel(1)).unwrap();
        assert!(matches!(limits.enter(Key::Channel(1)), Err(Busy::Key)));
        let others: Vec<_> = (2..=4)
            .map(|id| limits.enter(Key::Channel(id)).unwrap())
            .collect();
        assert!(matches!(limits.enter(Key::Channel(5)), Err(Busy::Global)));
        drop(first);
        assert!(limits.enter(Key::Channel(5)).is_ok());
        drop(others);
        let result = tokio::time::timeout(std::time::Duration::from_millis(1), async {
            let _lease = limits.enter(Key::Channel(1)).unwrap();
            std::future::pending::<()>().await;
        })
        .await;
        assert!(result.is_err());
        assert!(limits.enter(Key::Channel(1)).is_ok());
    }

    #[test]
    fn users_and_channels_are_separate_keys_sharing_the_global_limit() {
        let limits = Limits::new(2);
        let channel = limits.enter(Key::Channel(7)).unwrap();
        // The same number as a user ID is a different key.
        let user = limits.enter(Key::User(7)).unwrap();
        assert!(matches!(limits.enter(Key::User(7)), Err(Busy::Key)));
        assert!(matches!(limits.enter(Key::User(8)), Err(Busy::Global)));
        drop(channel);
        assert!(limits.enter(Key::User(8)).is_ok());
        drop(user);
    }
}
