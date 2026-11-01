extern crate std; // zip needs std::io

use alloc::{string::String, sync::Arc, vec, vec::Vec};
use std::io::{Cursor, Read};

use jvm::{Jvm, runtime::JavaLangString};
use spin::Mutex;
use zip::ZipArchive;

use wie_backend::System;

enum State {
    Pending,
    Ready(ZipArchive<Cursor<Vec<u8>>>),
    Unavailable,
}

/// Serves WIPI-C resources straight from the application jar.
///
/// The Java class loader reopens the jar and allocates a pile of Java objects (then forces a full
/// garbage collection) for every lookup, which made screen changes in resource-heavy games slow.
/// Anything this index cannot answer falls back to the class loader.
#[derive(Clone)]
pub struct JarResources {
    state: Arc<Mutex<State>>,
}

impl JarResources {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::Pending)),
        }
    }

    /// Uncompressed size of `name`, or `None` when the fast path cannot answer.
    pub async fn size(&self, system: &System, jvm: &Jvm, name: &str) -> Option<usize> {
        self.prepare(system, jvm).await;

        let mut state = self.state.lock();
        let State::Ready(archive) = &mut *state else { return None };
        let file = archive.by_name(Self::entry_name(name)?).ok()?;

        file.is_file().then(|| file.size() as usize)
    }

    /// Contents of `name`, or `None` when the fast path cannot answer.
    pub async fn read(&self, system: &System, jvm: &Jvm, name: &str) -> Option<Vec<u8>> {
        self.prepare(system, jvm).await;

        let mut state = self.state.lock();
        let State::Ready(archive) = &mut *state else { return None };
        let mut file = archive.by_name(Self::entry_name(name)?).ok()?;
        if !file.is_file() {
            return None;
        }

        let mut data = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut data).ok()?;

        Some(data)
    }

    // Mirrors URLClassLoader.findResource for jar class path entries.
    fn entry_name(name: &str) -> Option<&str> {
        let name = name.trim_start_matches('/');

        (!name.is_empty()).then_some(name)
    }

    async fn prepare(&self, system: &System, jvm: &Jvm) {
        if !matches!(*self.state.lock(), State::Pending) {
            return;
        }

        let archive = Self::open(system, jvm).await;
        if archive.is_none() {
            tracing::warn!("Resource fast path unavailable; using the class loader");
        }

        let mut state = self.state.lock();
        if matches!(*state, State::Pending) {
            *state = archive.map_or(State::Unavailable, State::Ready);
        }
    }

    async fn open(system: &System, jvm: &Jvm) -> Option<ZipArchive<Cursor<Vec<u8>>>> {
        let class_path = Self::class_path(jvm).await?;
        // The class path is "wie.rustjar<sep>app.jar"; resources only live in the app jar.
        let jar_name = class_path.split([':', ';']).nth(1)?;

        let filesystem = system.filesystem();
        let size = filesystem.size(jar_name).await?;
        let mut data = vec![0; size];
        let read = filesystem.read(jar_name, 0, size, &mut data).await?;
        if read != size {
            return None;
        }

        ZipArchive::new(Cursor::new(data)).ok()
    }

    async fn class_path(jvm: &Jvm) -> Option<String> {
        let key = JavaLangString::from_rust_string(jvm, "java.class.path").await.ok()?;
        let value = jvm
            .invoke_static("java/lang/System", "getProperty", "(Ljava/lang/String;)Ljava/lang/String;", (key,))
            .await
            .ok()?;

        JavaLangString::to_rust_string(jvm, &value).await.ok()
    }
}
