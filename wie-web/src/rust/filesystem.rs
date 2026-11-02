use alloc::{
    boxed::Box,
    collections::BTreeMap,
    rc::Rc,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::cell::RefCell;
use core::cmp::{max, min};

use wie_backend::Filesystem;

use crate::indexed_db_store::{Store, StoreKey};

const DB_NAME: &str = "wie_filesystem";
const STORE_NAME: &str = "files";

fn make_key(pid: &str, path: &str) -> StoreKey {
    StoreKey::Pair(pid.to_string(), path.to_string())
}

/// Contents of a file as last read or written; `None` records that the file does not exist.
type CachedFile = Option<Arc<Vec<u8>>>;

/// Files live in IndexedDB, where every request waits for the browser's event loop. Games read saves in
/// many small pieces, so contents are cached after the first read and writes go to both.
pub struct WebFilesystem {
    store: Rc<RefCell<Option<Store>>>,
    files: Rc<RefCell<BTreeMap<(String, String), CachedFile>>>,
}

// single threaded wasm; RefCell + Rc are only touched sequentially.
unsafe impl Send for WebFilesystem {}
unsafe impl Sync for WebFilesystem {}

impl WebFilesystem {
    pub fn new() -> Self {
        Self {
            store: Rc::new(RefCell::new(None)),
            files: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }

    async fn file(&self, pid: &str, path: &str) -> CachedFile {
        let cache_key = (pid.to_string(), path.to_string());
        if let Some(file) = self.files.borrow().get(&cache_key) {
            return file.clone();
        }
        let file = self.store().await.get(make_key(pid, path)).await.map(Arc::new);
        self.files.borrow_mut().insert(cache_key, file.clone());
        file
    }

    async fn set_file(&self, pid: &str, path: &str, data: Vec<u8>) {
        let data = Arc::new(data);
        self.files.borrow_mut().insert((pid.to_string(), path.to_string()), Some(data.clone()));
        self.store().await.set(make_key(pid, path), &data).await;
    }

    async fn store(&self) -> Store {
        if let Some(store) = self.store.borrow().as_ref() {
            return store.clone();
        }
        let store = Store::open(DB_NAME, STORE_NAME).await;
        *self.store.borrow_mut() = Some(store.clone());
        store
    }
}

#[async_trait::async_trait]
impl Filesystem for WebFilesystem {
    async fn remove(&self, pid: &str, path: &str) -> bool {
        if self.file(pid, path).await.is_none() {
            return false;
        }
        self.files.borrow_mut().insert((pid.to_string(), path.to_string()), None);
        self.store().await.delete(make_key(pid, path)).await;
        true
    }

    async fn usage(&self, pid: &str) -> u64 {
        let store = self.store().await;
        let mut usage = 0;
        for key in store.get_all_keys().await {
            if matches!(&key, StoreKey::Pair(owner, _) if owner == pid)
                && let Some(data) = store.get(key).await
            {
                usage += data.len() as u64;
            }
        }
        usage
    }

    async fn exists(&self, pid: &str, path: &str) -> bool {
        self.file(pid, path).await.is_some()
    }

    async fn size(&self, pid: &str, path: &str) -> Option<usize> {
        self.file(pid, path).await.map(|data| data.len())
    }

    async fn read(&self, pid: &str, path: &str, offset: usize, count: usize, buf: &mut [u8]) -> Option<usize> {
        let data = self.file(pid, path).await?;

        if offset >= data.len() {
            return Some(0);
        }

        let size_to_read = min(count, data.len() - offset);
        buf[..size_to_read].copy_from_slice(&data[offset..offset + size_to_read]);
        Some(size_to_read)
    }

    async fn write(&self, pid: &str, path: &str, offset: usize, data: &[u8]) -> usize {
        let existing = self.file(pid, path).await.unwrap_or_default();
        let new_len = max(existing.len(), offset + data.len());

        let mut next = vec![0u8; new_len];
        next[..existing.len()].copy_from_slice(&existing);
        next[offset..offset + data.len()].copy_from_slice(data);

        self.set_file(pid, path, next).await;
        data.len()
    }

    async fn truncate(&self, pid: &str, path: &str, len: usize) {
        let existing = self.file(pid, path).await.unwrap_or_default();

        let mut next = vec![0u8; len];
        let copy_len = min(existing.len(), len);
        next[..copy_len].copy_from_slice(&existing[..copy_len]);

        self.set_file(pid, path, next).await;
    }
}
