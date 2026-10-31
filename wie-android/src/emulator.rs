use alloc::{
    borrow::ToOwned,
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

const LIBRARY_PREFIX: &str = "lib/armeabi/lib";
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

    fn library_name(files: &BTreeMap<String, Vec<u8>>) -> Option<String> {
        files
            .keys()
            .find_map(|x| x.strip_prefix(LIBRARY_PREFIX)?.strip_suffix(".so").map(ToString::to_string))
    }

    pub fn from_apk(platform: Box<dyn Platform>, mut files: BTreeMap<String, Vec<u8>>, options: Options) -> Result<Self> {
        let name = Self::library_name(&files).ok_or_else(|| WieError::FatalError("APK has no armeabi library".into()))?;
        let library = files
            .remove(&alloc::format!("{LIBRARY_PREFIX}{name}.so"))
            .ok_or_else(|| WieError::FatalError("Missing native library".into()))?;
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

    pub fn title(files: &BTreeMap<String, Vec<u8>>) -> Option<String> {
        Self::library_name(files).map(|x| x.to_owned())
    }

    pub fn icon(files: &BTreeMap<String, Vec<u8>>) -> Option<Vec<u8>> {
        files
            .get("res/drawable-hdpi-v4/icon.png")
            .or_else(|| files.get("res/drawable/icon.png"))
            .cloned()
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
