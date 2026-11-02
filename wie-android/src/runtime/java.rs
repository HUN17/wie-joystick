use alloc::{string::String, vec::Vec};

use wie_core_arm::ArmCore;
use wie_util::{ByteWrite, Result, read_generic, write_generic};

use super::{
    AndroidContext, MAX_ASSETS, VFS_FD, audio,
    jni::{array_data, array_length, new_string, read_java_string},
    read_string,
};

/// Raw JNI value bits; 32-bit kinds occupy the low word.
#[derive(Clone, Copy, Debug)]
pub struct JavaValue(pub u64);

const MEMORY_SIZE: u64 = 64 * 1024 * 1024;
const STORAGE_SIZE: u64 = 1024 * 1024 * 1024;

pub async fn call_java_method(
    core: &mut ArmCore,
    context: &mut AndroidContext,
    class: &str,
    name: &str,
    signature: &str,
    _this: u32,
    args: &[JavaValue],
) -> Result<JavaValue> {
    let string = |core: &mut ArmCore, value: &str| -> Result<JavaValue> { Ok(JavaValue(new_string(core, value)? as u64)) };

    let result = match name {
        "getOSVersion" => JavaValue(8),
        "getModel" => string(core, "Nexus S")?,
        "getManufacturer" => string(core, "samsung")?,
        "getOpenGLESVersion" => string(core, "OpenGL ES-CM 1.1")?,
        "getIMEI" | "getIMSI" | "getMin" => string(core, "01000000000")?,
        "freeMemory" | "totalMemory" => JavaValue(MEMORY_SIZE),
        "getAvailableExternalMemorySize" | "getAvailableInternalMemorySize" | "getTotalExternalMemorySize" | "getTotalInternalMemorySize" => {
            JavaValue(STORAGE_SIZE)
        }
        "getVolume" | "getSoundVolume" | "getMusicVolume" => JavaValue(100),
        "isNetworkAvailable" => JavaValue(1),
        "isFBOSupported" | "isNetworkRoaming" | "isAirplaneModeOn" => JavaValue(0),
        "putSoundClip" | "putMusicClip" => {
            let (clip, offset, length) = (args[0].0 as u32, args[1].0, args[2].0);
            JavaValue(audio::put_clip(core, context, clip, offset, length)? as u32 as u64)
        }
        "clipPlaySound" | "clipPlayMusic" | "clipPlay" => {
            let (clip, repeat) = (args[0].0 as u32, args[1].0 != 0);
            JavaValue(audio::play(core, context, clip, repeat)? as u32 as u64)
        }
        "clipStopSound" | "clipStopMusic" | "clipStop" | "clipPauseSound" | "clipPauseMusic" | "clipPause" => {
            JavaValue(audio::stop(core, context, args[0].0 as u32)? as u32 as u64)
        }
        "clipResumeSound" | "clipResumeMusic" | "clipResume" => JavaValue(audio::resume(core, context, args[0].0 as u32)? as u32 as u64),
        "clipFreeSound" | "clipFreeMusic" | "clipFree" => JavaValue(audio::free_clip(core, context, args[0].0 as u32)? as u32 as u64),
        "requestMTX" => {
            // requestMTX(gameId, contentId, pid); the result is delivered before the next frame.
            let content_id = args[1].0 as u32;
            tracing::info!("Purchase requested: {}", read_java_string(core, content_id)?);
            context.update_globals(core, |x| x.pending_purchase = content_id)?;
            JavaValue(0)
        }
        "getUniqueNumberFromStrings" => {
            let name = read_java_string(core, args[0].0 as u32)?;
            JavaValue(asset_id(core, context, &name)? as u64)
        }
        "getUniqueAssetNameFromID" => match asset_name(core, context, args[0].0 as u32)? {
            Some(name) => string(core, &name)?,
            None => JavaValue(0),
        },
        "getAssetSize" => {
            let name = read_java_string(core, args[0].0 as u32)?;
            let size = context.system.filesystem().size(&asset_path(&name)).await;
            JavaValue(size.map_or(u32::MAX, |x| x as u32) as u64)
        }
        "loadAsset" => {
            let (id, array, length) = (args[0].0 as u32, args[1].0 as u32, args[2].0 as u32);
            let read = match asset_name(core, context, id)? {
                Some(name) => {
                    let length = length.min(array_length(core, array)?) as usize;
                    let mut data = alloc::vec![0; length];
                    let read = context.system.filesystem().read(&asset_path(&name), 0, length, &mut data).await;
                    if let Some(read) = read {
                        core.write_bytes(array_data(array), &data[..read])?;
                    }
                    read.map_or(u32::MAX, |x| x as u32)
                }
                None => u32::MAX,
            };
            JavaValue(read as u64)
        }
        "exitApp" => {
            context.system.platform().exit();
            JavaValue(0)
        }
        _ => {
            tracing::warn!("stub {class}.{name}{signature}({})", format_args(args));
            return Ok(JavaValue(0));
        }
    };

    tracing::debug!("{class}.{name}{signature}({}) -> {:#x}", format_args(args), result.0);
    Ok(result)
}

pub fn get_java_field(_core: &mut ArmCore, _context: &mut AndroidContext, class: &str, name: &str, object: u32) -> Result<JavaValue> {
    let value = match name {
        "descriptor" => JavaValue(VFS_FD as u64),
        _ => {
            tracing::warn!("stub get field {class}.{name} of {object:#x}");
            return Ok(JavaValue(0));
        }
    };

    tracing::debug!("get field {class}.{name} of {object:#x} -> {:#x}", value.0);
    Ok(value)
}

fn format_args(args: &[JavaValue]) -> String {
    args.iter().map(|x| alloc::format!("{:#x}", x.0)).collect::<Vec<_>>().join(", ")
}

fn asset_path(name: &str) -> String {
    alloc::format!("assets/{}", name.trim_start_matches('/'))
}

/// Assigns sequential IDs to asset names, keeping the name table in guest memory.
fn asset_id(core: &mut ArmCore, context: &AndroidContext, name: &str) -> Result<u32> {
    let globals = context.globals(core)?;
    for index in 0..globals.asset_count {
        let ptr: u32 = read_generic(core, globals.asset_table + index * 4)?;
        if read_string(core, ptr)? == name {
            return Ok(index + 1);
        }
    }

    if globals.asset_count >= MAX_ASSETS {
        return Err(wie_util::WieError::FatalError("Asset table full".into()));
    }
    let ptr = new_string(core, name)?;
    write_generic(core, globals.asset_table + globals.asset_count * 4, ptr + 16)?;
    context.update_globals(core, |x| x.asset_count += 1)?;
    Ok(globals.asset_count + 1)
}

fn asset_name(core: &ArmCore, context: &AndroidContext, id: u32) -> Result<Option<String>> {
    let globals = context.globals(core)?;
    if id == 0 || id > globals.asset_count {
        return Ok(None);
    }
    let ptr: u32 = read_generic(core, globals.asset_table + (id - 1) * 4)?;
    Ok(Some(read_string(core, ptr)?))
}
