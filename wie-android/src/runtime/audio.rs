//! Sound and music clips the Clet bridge registers as Ogg Vorbis ranges of the packed asset.

use alloc::{format, vec::Vec};

use bytemuck::{Pod, Zeroable};

use wie_core_arm::ArmCore;
use wie_util::{Result, WieError, read_generic, write_generic};

use super::AndroidContext;

pub const MAX_CLIPS: u32 = 256;

/// Guest clip table entry mapping a native clip pointer to a backend audio handle.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct ClipEntry {
    clip: u32,
    /// Audio handle plus one; zero marks a free entry.
    handle: u32,
    /// Whether the last play request looped, so a resume restarts it the same way.
    repeat: u32,
}

fn entry_address(core: &ArmCore, context: &AndroidContext, index: u32) -> Result<u32> {
    Ok(context.globals(core)?.clip_table + index * size_of::<ClipEntry>() as u32)
}

fn find(core: &ArmCore, context: &AndroidContext, clip: u32) -> Result<Option<(u32, ClipEntry)>> {
    for index in 0..MAX_CLIPS {
        let address = entry_address(core, context, index)?;
        let entry: ClipEntry = read_generic(core, address)?;
        if entry.handle != 0 && entry.clip == clip {
            return Ok(Some((address, entry)));
        }
    }
    Ok(None)
}

fn decode_ogg(data: &[u8]) -> Result<(u8, u32, Vec<i16>)> {
    extern crate std;
    use std::{boxed::Box, io::Cursor};
    use symphonia::core::{
        audio::SampleBuffer, codecs::DecoderOptions, errors::Error, formats::FormatOptions, io::MediaSourceStream, meta::MetadataOptions, probe::Hint,
    };

    let error = |x: Error| WieError::FatalError(format!("Ogg Vorbis decode failed: {x}"));
    let source = MediaSourceStream::new(Box::new(Cursor::new(data.to_vec())), Default::default());
    let mut probed = symphonia::default::get_probe()
        .format(&Hint::new(), source, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(error)?;
    let track = probed
        .format
        .default_track()
        .ok_or_else(|| WieError::FatalError("Ogg stream has no track".into()))?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(error)?;

    let (mut channels, mut sampling_rate, mut samples) = (0, 0, Vec::new());
    // The stream ends with an end-of-file error once every packet is read.
    while let Ok(packet) = probed.format.next_packet() {
        // A corrupt packet only drops its own samples, as on the original player.
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(Error::DecodeError(_)) => continue,
            Err(other) => return Err(error(other)),
        };
        let spec = *decoded.spec();
        channels = spec.channels.count() as u8;
        sampling_rate = spec.rate;
        let mut buffer = SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
        buffer.copy_interleaved_ref(decoded);
        samples.extend_from_slice(buffer.samples());
    }

    Ok((channels, sampling_rate, samples))
}

/// Decodes `length` bytes at `offset` of the packed asset and binds them to `clip`.
pub fn put_clip(core: &mut ArmCore, context: &AndroidContext, clip: u32, offset: u64, length: u64) -> Result<i32> {
    free_clip(core, context, clip)?;

    let range = offset as usize..(offset + length) as usize;
    let Some(data) = context.vfs.get(range) else {
        tracing::warn!("Sound clip {clip:#x} range {offset:#x}+{length:#x} is outside the asset");
        return Ok(-1);
    };
    let (channels, sampling_rate, samples) = match decode_ogg(data) {
        Ok(decoded) => decoded,
        Err(error) => {
            tracing::warn!("Sound clip {clip:#x}: {error}");
            return Ok(-1);
        }
    };
    let handle = context.system.audio().load_wave(channels, sampling_rate, samples);

    for index in 0..MAX_CLIPS {
        let address = entry_address(core, context, index)?;
        let entry: ClipEntry = read_generic(core, address)?;
        if entry.handle == 0 {
            write_generic(
                core,
                address,
                ClipEntry {
                    clip,
                    handle: handle + 1,
                    repeat: 0,
                },
            )?;
            return Ok(0);
        }
    }

    context.system.audio().close(handle).ok();
    Err(WieError::FatalError("Sound clip table full".into()))
}

pub fn play(core: &mut ArmCore, context: &AndroidContext, clip: u32, repeat: bool) -> Result<i32> {
    let Some((address, mut entry)) = find(core, context, clip)? else {
        return Ok(-1);
    };
    entry.repeat = repeat as u32;
    write_generic(core, address, entry)?;
    Ok(if context.system.audio().play(entry.handle - 1, repeat).is_ok() {
        0
    } else {
        -1
    })
}

/// The backend cannot seek, so resuming restarts the clip.
pub fn resume(core: &mut ArmCore, context: &AndroidContext, clip: u32) -> Result<i32> {
    let Some((_, entry)) = find(core, context, clip)? else {
        return Ok(-1);
    };
    play(core, context, clip, entry.repeat != 0)
}

pub fn stop(core: &ArmCore, context: &AndroidContext, clip: u32) -> Result<i32> {
    let Some((_, entry)) = find(core, context, clip)? else {
        return Ok(-1);
    };
    context.system.audio().stop(entry.handle - 1);
    Ok(0)
}

pub fn free_clip(core: &mut ArmCore, context: &AndroidContext, clip: u32) -> Result<i32> {
    let Some((address, entry)) = find(core, context, clip)? else {
        return Ok(-1);
    };
    context.system.audio().close(entry.handle - 1).ok();
    write_generic(core, address, ClipEntry::zeroed())?;
    Ok(0)
}
