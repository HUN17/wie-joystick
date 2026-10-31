use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};
use core::pin::Pin;

use wie_backend::{Emulator, Event, Options, Platform, System, TaskRunner};
use wie_core_arm::{Allocator, ArmCore};
use wie_util::{Result, WieError};

use crate::runtime::{DisplayConfig, start};

const LIBRARY_DIRECTORY: &str = "lib/armeabi/";
/// Repackaged APKs keep the original 32-bit game library as an asset next to their own loader.
const ASSET_DIRECTORY: &str = "assets/";
const VFS_ASSET: &str = "assets/data.vfs.mp3";

struct AndroidTaskRunner {
    core: ArmCore,
}

#[async_trait::async_trait]
impl TaskRunner for AndroidTaskRunner {
    async fn run(&self, future: Pin<Box<dyn Future<Output = Result<()>> + Send>>) -> Result<()> {
        self.core.run_in_thread(move || future)?.await
    }
}

pub struct AndroidEmulator {
    core: ArmCore,
    system: System,
}

impl AndroidEmulator {
    /// APKs whose game logic lives in an armeabi library driven through the EA Mobile Clet bridge.
    pub fn loadable_apk(files: &BTreeMap<String, Vec<u8>>) -> bool {
        Self::library_name(files).is_some() && files.contains_key(VFS_ASSET)
    }

    /// Path of the 32-bit ARM game library and its name without `lib` and `.so`.
    fn library(files: &BTreeMap<String, Vec<u8>>) -> Option<(String, String)> {
        let is_arm32_elf = |data: &[u8]| data.len() > 20 && data.starts_with(b"\x7fELF") && data[4] == 1 && data[18] == 40;
        let candidate = |directory: &str| {
            files.iter().find_map(|(path, data)| {
                let file = path.strip_prefix(directory)?;
                let name = file.strip_suffix(".so")?;
                (!file.contains('/') && is_arm32_elf(data)).then(|| (path.clone(), name.strip_prefix("lib").unwrap_or(name).to_string()))
            })
        };
        candidate(LIBRARY_DIRECTORY).or_else(|| candidate(ASSET_DIRECTORY))
    }

    fn library_name(files: &BTreeMap<String, Vec<u8>>) -> Option<String> {
        Self::library(files).map(|(_, name)| name)
    }

    pub fn from_apk(platform: Box<dyn Platform>, mut files: BTreeMap<String, Vec<u8>>, options: Options) -> Result<Self> {
        let (path, name) = Self::library(&files).ok_or_else(|| WieError::FatalError("APK has no 32-bit ARM game library".into()))?;
        let library = files.remove(&path).ok_or_else(|| WieError::FatalError("Missing native library".into()))?;
        let vfs = files
            .remove(VFS_ASSET)
            .ok_or_else(|| WieError::FatalError("Missing data.vfs.mp3".into()))?;

        tracing::info!("Loading Android app {name}");

        let mut core = ArmCore::new(options)?;
        let system = System::new(platform, &name, &name, AndroidTaskRunner { core: core.clone() });
        for (path, data) in files {
            system.filesystem().add_virtual(&path, data);
        }
        Allocator::init(&mut core)?;

        let mut core_clone = core.clone();
        let mut system_clone = system.clone();
        let display = DisplayConfig {
            screen_width: 400,
            screen_height: 240,
            wipi_width: 400,
            wipi_height: 240,
            magnification: 1.0,
        };
        system.spawn(async move || start(&mut core_clone, &mut system_clone, &library, vfs, display).await);

        Ok(Self { core, system })
    }

    /// Stable app ID; it also namespaces the app's saves, so it must not change.
    pub fn archive_id(files: &BTreeMap<String, Vec<u8>>) -> Option<String> {
        Self::library_name(files)
    }

    /// The APK's launcher label, falling back to the game library name.
    pub fn title(files: &BTreeMap<String, Vec<u8>>) -> Option<String> {
        crate::apk_info::label(files).or_else(|| Self::library_name(files))
    }

    pub fn icon(files: &BTreeMap<String, Vec<u8>>) -> Option<Vec<u8>> {
        crate::apk_info::icon(files)
    }
}

impl Emulator for AndroidEmulator {
    fn is_preparing(&self) -> bool {
        self.core.is_preparing()
    }

    fn handle_event(&mut self, event: Event) {
        self.system.event_queue().push(event)
    }

    fn tick(&mut self) -> Result<()> {
        self.system.tick()
    }
}
