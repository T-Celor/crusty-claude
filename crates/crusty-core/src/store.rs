//! Originals of content an optimizer shortened, so nothing is lost: the model can fetch
//! them back with the `Retrieve` tool by handle.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

#[derive(Debug)]
struct Inner {
    items: HashMap<String, Arc<String>>,
    order: VecDeque<String>,
    bytes: usize,
}

/// Bounded in-memory store. Oldest entries are evicted first once `max_bytes` is reached.
#[derive(Debug)]
pub struct OriginalStore {
    inner: Mutex<Inner>,
    max_bytes: usize,
}

impl Default for OriginalStore {
    fn default() -> Self {
        OriginalStore::new(64 * 1024 * 1024)
    }
}

impl OriginalStore {
    pub fn new(max_bytes: usize) -> Self {
        OriginalStore { inner: Mutex::new(Inner { items: HashMap::new(), order: VecDeque::new(), bytes: 0 }), max_bytes }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stores `content` and returns its handle (`orig_` + 12 hex chars of its SHA-256).
    /// The same content always gets the same handle.
    pub fn put(&self, content: impl Into<String>) -> String {
        let content = content.into();
        let digest = Sha256::digest(content.as_bytes());
        let handle = format!("orig_{}", digest.iter().take(6).map(|b| format!("{b:02x}")).collect::<String>());
        let mut inner = self.lock();
        if inner.items.contains_key(&handle) {
            return handle;
        }
        inner.bytes += content.len();
        inner.items.insert(handle.clone(), Arc::new(content));
        inner.order.push_back(handle.clone());
        while inner.bytes > self.max_bytes && inner.order.len() > 1 {
            if let Some(old) = inner.order.pop_front()
                && let Some(v) = inner.items.remove(&old)
            {
                inner.bytes -= v.len();
            }
        }
        handle
    }

    pub fn get(&self, handle: &str) -> Option<Arc<String>> {
        self.lock().items.get(handle).cloned()
    }

    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_is_idempotent_and_retrievable() {
        let s = OriginalStore::default();
        let h1 = s.put("hello");
        let h2 = s.put("hello");
        assert_eq!(h1, h2);
        assert!(h1.starts_with("orig_") && h1.len() == 17);
        assert_eq!(s.get(&h1).unwrap().as_str(), "hello");
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn evicts_oldest_over_budget() {
        let s = OriginalStore::new(10);
        let a = s.put("aaaaaa");
        let b = s.put("bbbbbb");
        assert!(s.get(&a).is_none());
        assert!(s.get(&b).is_some());
    }
}
