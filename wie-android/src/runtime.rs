mod audio;
mod file;
mod format;
mod gl;
mod hle;
mod imports;
mod java;
mod jni;
mod loader;
mod net;

use alloc::{format, string::String, sync::Arc, vec::Vec};

use bytemuck::{Pod, Zeroable};

use wie_backend::{Event, KeyCode, System, TouchPhase};
use wie_core_arm::{Allocator, ArmCore};
use wie_util::{ByteWrite, Result, WieError, read_generic, write_generic};

use self::{
    imports::{register_import_svc_handler, resolve_import},
    jni::{JniEnv, register_jni_svc_handler},
    loader::load_shared_library,
};

const SVC_CATEGORY_IMPORT: u32 = 1;
const SVC_CATEGORY_JNI: u32 = 2;
const SVC_CATEGORY_JAVA_VM: u32 = 3;
const SVC_CATEGORY_HLE: u32 = 4;

const LIBRARY_BASE: u32 = 0x100000;

/// File descriptor handed to the native VFS reader for the packed game asset.
const VFS_FD: u32 = 100;

const MAX_ASSETS: u32 = 4096;

/// Guest-resident runtime state shared by the emulated libc and JNI.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct RuntimeGlobals {
    errno: u32,
    stack_chk_guard: u32,
    next_texture_id: u32,
    vfs_position: u32,
    env: u32,
    vm: u32,
    /// Table of guest string pointers indexed by asset ID - 1.
    asset_table: u32,
    asset_count: u32,
    fd_table: u32,
    cwd: u32,
    gl_state: u32,
    clip_table: u32,
    /// Content ID string of a carrier purchase waiting for its result, or zero.
    pending_purchase: u32,
    socket_table: u32,
}

#[derive(Clone)]
pub struct AndroidContext {
    system: System,
    vfs: Arc<Vec<u8>>,
    ptr_globals: u32,
}

impl AndroidContext {
    fn globals(&self, core: &ArmCore) -> Result<RuntimeGlobals> {
        read_generic(core, self.ptr_globals)
    }

    fn update_globals(&self, core: &mut ArmCore, f: impl FnOnce(&mut RuntimeGlobals)) -> Result<()> {
        let mut globals = self.globals(core)?;
        f(&mut globals);
        write_generic(core, self.ptr_globals, globals)
    }
}

pub struct DisplayConfig {
    pub screen_width: u32,
    pub screen_height: u32,
    pub wipi_width: u32,
    pub wipi_height: u32,
    pub magnification: f32,
}

pub async fn start(core: &mut ArmCore, system: &mut System, library: &[u8], vfs: Vec<u8>, display: DisplayConfig) -> Result<()> {
    let ptr_globals = Allocator::alloc(core, size_of::<RuntimeGlobals>() as u32)?;
    let asset_table = Allocator::alloc(core, MAX_ASSETS * 4)?;
    let clip_table_size = audio::MAX_CLIPS * size_of::<audio::ClipEntry>() as u32;
    let clip_table = Allocator::alloc(core, clip_table_size)?;
    core.write_bytes(clip_table, &alloc::vec![0; clip_table_size as usize])?;
    let gl_state = Allocator::alloc(core, size_of::<gl::GlState>() as u32)?;
    write_generic(core, gl_state, <gl::GlState as Zeroable>::zeroed())?;
    let fd_table = Allocator::alloc(core, file::MAX_FILES * size_of::<file::OpenFile>() as u32)?;
    core.write_bytes(fd_table, &alloc::vec![0; (file::MAX_FILES as usize) * size_of::<file::OpenFile>()])?;
    let socket_table_size = net::MAX_SOCKETS as usize * size_of::<net::SocketEntry>();
    let socket_table = Allocator::alloc(core, socket_table_size as u32)?;
    core.write_bytes(socket_table, &alloc::vec![0; socket_table_size])?;
    write_generic(
        core,
        ptr_globals,
        RuntimeGlobals {
            next_texture_id: 1,
            stack_chk_guard: 0xdead_beef,
            asset_table,
            fd_table,
            socket_table,
            gl_state,
            clip_table,
            ..Zeroable::zeroed()
        },
    )?;

    let context = AndroidContext {
        system: system.clone(),
        vfs: Arc::new(vfs),
        ptr_globals,
    };

    register_import_svc_handler(core, &context)?;
    register_jni_svc_handler(core, &context)?;

    let library = load_shared_library(core, library, LIBRARY_BASE, |core, name| resolve_import(core, &context, name))?;
    hle::install(core, &context, &library.exports)?;
    core.prepare_execution().await?;

    let env = JniEnv::create(core, &context).await?;

    for function in &library.init_array {
        tracing::debug!("Calling init_array entry {function:#x}");
        let _: () = core.run_function(*function, &[]).await.map_err(|x| with_arm_context(core, x))?;
    }

    let native = |name: &str| -> Result<u32> {
        library
            .exports
            .get(&format!("Java_co_kr_eamobile_CletEntry_{name}"))
            .copied()
            .ok_or_else(|| WieError::FatalError(format!("Missing native method {name}")))
    };

    let entry = env.new_object(core, "co/kr/eamobile/CletEntry")?;

    tracing::info!("nativeInitKernel");
    let _: u32 = call(core, native("nativeInitKernel")?, &[env.ptr(), entry]).await?;

    tracing::info!("nativeInitDisplay");
    let _: u32 = call(
        core,
        native("nativeInitDisplay")?,
        &[
            env.ptr(),
            entry,
            display.screen_width,
            display.screen_height,
            0,
            0,
            0,
            255,
            display.wipi_width,
            display.wipi_height,
            0,
            0,
            0,
            display.magnification.to_bits(),
            1,
            1,
            0,
            0,
        ],
    )
    .await?;

    tracing::info!("nativeInitFileStorage");
    let internal = env.new_string(core, &format!("/data/data/{}/files", system.pid()))?;
    let external = env.new_string(core, &format!("/sdcard/{}", system.pid()))?;
    let _: u32 = call(core, native("nativeInitFileStorage")?, &[env.ptr(), entry, internal, external]).await?;

    tracing::info!("nativeInitVFS");
    let fd = env.new_object(core, "java/io/FileDescriptor")?;
    let length = context.vfs.len() as u32;
    // jlong arguments start on an even slot, so r3 is skipped.
    let _: u32 = call(core, native("nativeInitVFS")?, &[env.ptr(), entry, fd, 0, 0, 0, length, 0]).await?;

    tracing::info!("nativeInitAudioConfiguration");
    let _: u32 = call(core, native("nativeInitAudioConfiguration")?, &[env.ptr(), entry, 0, 0, 202400, 0]).await?;

    tracing::info!("nativeStartApp");
    let _: u32 = call(core, native("nativeStartApp")?, &[env.ptr(), entry]).await?;
    tracing::info!("nativeResumeApp");
    let _: u32 = call(core, native("nativeResumeApp")?, &[env.ptr(), entry]).await?;

    let native_loop = native("nativeLoop")?;
    let native_key = native("nativeKeyHandleEvent")?;
    let native_touch = native("nativeTouchHandleEvent")?;
    // The carrier billing service is gone, so purchases are confirmed locally (result 0 is success).
    let native_purchase_result = library
        .exports
        .get("Java_co_kr_eamobile_mtx_CarrierFuncActivity_nativeResultMTX")
        .copied();
    let activity = env.new_object(core, "co/kr/eamobile/mtx/CarrierFuncActivity")?;
    let mut frame: u64 = 0;
    loop {
        if frame.is_multiple_of(600) {
            tracing::info!("nativeLoop frame {frame}");
        }

        let pending_purchase = context.globals(core)?.pending_purchase;
        if pending_purchase != 0 {
            context.update_globals(core, |x| x.pending_purchase = 0)?;
            if let Some(native_purchase_result) = native_purchase_result {
                tracing::info!("Confirming purchase {}", jni::read_java_string(core, pending_purchase)?);
                let _: u32 = call(core, native_purchase_result, &[env.ptr(), activity, pending_purchase, 0, 0]).await?;
            }
        }

        loop {
            let event = system.event_queue().pop();
            let Some(event) = event else { break };
            let (r#type, key) = match event {
                Event::Keydown(key) => (KEY_PRESS, key),
                Event::Keyup(key) => (KEY_RELEASE, key),
                Event::Touch { phase, x, y } => {
                    let r#type = match phase {
                        TouchPhase::Down => TOUCH_PRESS,
                        TouchPhase::Move => TOUCH_MOVE,
                        TouchPhase::Up => TOUCH_RELEASE,
                    };
                    tracing::debug!("nativeTouchHandleEvent({type}, {x}, {y})", type = r#type);
                    let _: u32 = call(core, native_touch, &[env.ptr(), entry, r#type, x as u32, y as u32, 0]).await?;
                    continue;
                }
                _ => continue,
            };
            let code = wipi_key_code(key);
            tracing::debug!("nativeKeyHandleEvent({type}, {code})", type = r#type);
            let _: u32 = call(core, native_key, &[env.ptr(), entry, r#type, code as u32, 0]).await?;
        }

        let _: u32 = call(core, native_loop, &[env.ptr(), entry]).await?;
        gl::present(core, &context)?;
        frame += 1;
        system.sleep(16).await;
    }
}

/// `nativeKeyHandleEvent` event types for key down and up.
const KEY_PRESS: u32 = 2;
const KEY_RELEASE: u32 = 3;

/// `nativeTouchHandleEvent` event types for a single pointer.
const TOUCH_PRESS: u32 = 19;
const TOUCH_RELEASE: u32 = 20;
const TOUCH_MOVE: u32 = 21;

/// WIPI-C key codes the Clet bridge expects.
fn wipi_key_code(key: KeyCode) -> i32 {
    match key {
        KeyCode::UP => -1,
        KeyCode::DOWN => -2,
        KeyCode::LEFT => -3,
        KeyCode::RIGHT => -4,
        KeyCode::OK => -5,
        KeyCode::LEFT_SOFT_KEY => -6,
        KeyCode::RIGHT_SOFT_KEY => -7,
        KeyCode::CALL => -10,
        KeyCode::HANGUP => -11,
        KeyCode::VOLUME_UP => -13,
        KeyCode::VOLUME_DOWN => -14,
        KeyCode::CLEAR => -16,
        KeyCode::NUM0 => b'0' as i32,
        KeyCode::NUM1 => b'1' as i32,
        KeyCode::NUM2 => b'2' as i32,
        KeyCode::NUM3 => b'3' as i32,
        KeyCode::NUM4 => b'4' as i32,
        KeyCode::NUM5 => b'5' as i32,
        KeyCode::NUM6 => b'6' as i32,
        KeyCode::NUM7 => b'7' as i32,
        KeyCode::NUM8 => b'8' as i32,
        KeyCode::NUM9 => b'9' as i32,
        KeyCode::HASH => b'#' as i32,
        KeyCode::STAR => b'*' as i32,
    }
}

async fn call(core: &mut ArmCore, function: u32, params: &[u32]) -> Result<u32> {
    core.run_function(function, params).await.map_err(|x| with_arm_context(core, x))
}

fn with_arm_context(core: &ArmCore, error: WieError) -> WieError {
    let context = core.dump_reg_stack(LIBRARY_BASE);
    match error {
        WieError::Unimplemented(message) => WieError::Unimplemented(format!("{message}{context}")),
        WieError::FatalError(message) => WieError::FatalError(format!("{message}{context}")),
        error => WieError::FatalError(format!("{error}{context}")),
    }
}

fn read_string(core: &ArmCore, address: u32) -> Result<String> {
    let bytes = wie_util::read_null_terminated_string_bytes(core, address)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
