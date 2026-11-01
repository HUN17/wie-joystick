//! Host implementations of hot guest functions: pixel loops in the Clet bridge's software renderer
//! and the VFS asset lookup.
//!
//! Each hooked function keeps its original prologue in a trampoline, so calls the host fast path
//! cannot reproduce exactly (blending, pixel-op callbacks, overlapping copies) run the guest code.

use alloc::{collections::BTreeMap, format, string::String, vec};

use wie_core_arm::{ArmCore, JumpTo, SvcId};
use wie_util::{ByteRead, ByteWrite, Result, WieError, read_generic, read_null_terminated_string_bytes, write_generic};

use super::{AndroidContext, SVC_CATEGORY_HLE, VFS_FD};

/// Executable scratch region for trampolines, below the library image.
const TRAMPOLINE_BASE: u32 = 0x80000;
const TRAMPOLINE_SIZE: u32 = 20;
/// Instructions copied from each hooked prologue; must not read PC.
const PROLOGUE_WORDS: u32 = 3;

/// `MC_GrpContext` fields read by the fast paths.
const GC_CLIP_LEFT: u32 = 0x4;
const GC_CLIP_TOP: u32 = 0x8;
const GC_CLIP_RIGHT: u32 = 0xc;
const GC_CLIP_BOTTOM: u32 = 0x10;
const GC_FOREGROUND: u32 = 0x14;
const GC_TRANSPARENT: u32 = 0x1c;
const GC_ALPHA: u32 = 0x20;
const GC_PIXEL_OP: u32 = 0x2c;
const OPAQUE_ALPHA: i32 = 254;

/// `MIDASKernelManager` fields set by `setVFSInfo(fd, offset, length)`.
const KM_VFS_FD: u32 = 0x610;
const KM_VFS_OFFSET: u32 = 0x614;
const KM_VFS_LENGTH: u32 = 0x618;

#[derive(Clone, Copy)]
enum Hook {
    FillRect,
    CopyFrameBuffer,
    GetAssetSizeFromVfs,
}

const HOOKS: [(&str, Hook); 3] = [
    ("MC_grpFillRect", Hook::FillRect),
    ("_Z26internalGrpCopyFrameBufferiiiiiiiiP14_MC_GrpContexth", Hook::CopyFrameBuffer),
    ("_ZN18MIDASKernelManager19getAssetSizeFromVFSEPKcPm", Hook::GetAssetSizeFromVfs),
];

/// `MC_GrpFrameBuffer`: width, height, bytes per line, bits per pixel, pixels.
#[derive(Clone, Copy)]
struct FrameBuffer {
    width: i32,
    height: i32,
    pitch: u32,
    bpp: u32,
    buffer: u32,
}

impl FrameBuffer {
    fn read(core: &ArmCore, address: u32) -> Result<Self> {
        let [width, height, pitch, bpp, buffer]: [u32; 5] = read_generic(core, address)?;
        Ok(Self {
            width: width as i32,
            height: height as i32,
            pitch,
            bpp,
            buffer,
        })
    }

    fn bytes_per_pixel(&self) -> Option<u32> {
        match self.bpp {
            16 => Some(2),
            32 => Some(4),
            _ => None,
        }
    }
}

struct Clip {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Clip {
    fn read(core: &ArmCore, gc: u32) -> Result<Self> {
        Ok(Self {
            left: read_generic(core, gc + GC_CLIP_LEFT)?,
            top: read_generic(core, gc + GC_CLIP_TOP)?,
            right: read_generic(core, gc + GC_CLIP_RIGHT)?,
            bottom: read_generic(core, gc + GC_CLIP_BOTTOM)?,
        })
    }
}

/// Fast paths only cover opaque drawing without a custom pixel operation.
fn plain_drawing(core: &ArmCore, gc: u32) -> Result<bool> {
    let alpha: i32 = read_generic(core, gc + GC_ALPHA)?;
    let pixel_op: u32 = read_generic(core, gc + GC_PIXEL_OP)?;
    Ok(alpha > OPAQUE_ALPHA && pixel_op == 0)
}

fn trampoline(index: u32) -> u32 {
    TRAMPOLINE_BASE + index * TRAMPOLINE_SIZE
}

/// Redirects each hooked function to an SVC stub and keeps its prologue in a trampoline.
pub fn install(core: &mut ArmCore, context: &AndroidContext, exports: &BTreeMap<String, u32>) -> Result<()> {
    core.register_svc_handler(SVC_CATEGORY_HLE, handle_hle_svc, context)?;
    core.load(&vec![0; (TRAMPOLINE_SIZE * HOOKS.len() as u32) as usize], TRAMPOLINE_BASE, 0x1000)?;

    for (index, (name, _)) in HOOKS.iter().enumerate() {
        let address = *exports
            .get(*name)
            .ok_or_else(|| WieError::FatalError(format!("Missing HLE target {name}")))?;
        let trampoline = trampoline(index as u32);

        for word in 0..PROLOGUE_WORDS {
            let instruction: u32 = read_generic(core, address + word * 4)?;
            if (instruction >> 16) & 0xf == 15 || instruction & 0xf == 15 {
                return Err(WieError::FatalError(format!("{name} prologue reads PC")));
            }
            write_generic(core, trampoline + word * 4, instruction)?;
        }
        // b <original + prologue>
        let branch_from = trampoline + PROLOGUE_WORDS * 4;
        let offset = (address + PROLOGUE_WORDS * 4).wrapping_sub(branch_from + 8) as i32 >> 2;
        write_generic(core, branch_from, 0xea00_0000 | (offset as u32 & 0x00ff_ffff))?;

        let stub = core.make_svc_stub(SVC_CATEGORY_HLE, index as u32)?;
        // ldr r12, [pc, #0]; bx r12; .word stub
        write_generic(core, address, [0xe59f_c000u32, 0xe12f_ff1c, stub])?;

        tracing::debug!("HLE {name} at {address:#x}, trampoline {trampoline:#x}");
    }

    Ok(())
}

async fn handle_hle_svc(core: &mut ArmCore, context: &mut AndroidContext, id: SvcId) -> Result<JumpTo> {
    let (_, lr) = core.read_pc_lr()?;
    let handled = match HOOKS[id.0 as usize].1 {
        Hook::FillRect => fill_rect(core)?,
        Hook::CopyFrameBuffer => copy_frame_buffer(core)?,
        Hook::GetAssetSizeFromVfs => get_asset_size_from_vfs(core, context)?,
    };

    Ok(JumpTo(if handled { lr } else { trampoline(id.0) }))
}

fn param(core: &ArmCore, index: usize) -> Result<i32> {
    Ok(core.read_param(index)? as i32)
}

/// `MC_grpFillRect(dst, x, y, w, h, gc)`.
fn fill_rect(core: &mut ArmCore) -> Result<bool> {
    let (dst, mut x, mut y, mut w, mut h, gc) = (
        param(core, 0)? as u32,
        param(core, 1)?,
        param(core, 2)?,
        param(core, 3)?,
        param(core, 4)?,
        param(core, 5)? as u32,
    );
    tracing::trace!("MC_grpFillRect({dst:#x}, {x}, {y}, {w}, {h}, {gc:#x})");
    if !plain_drawing(core, gc)? {
        return Ok(false);
    }

    let fb = FrameBuffer::read(core, dst)?;
    let Some(size) = fb.bytes_per_pixel() else {
        return Ok(true);
    };

    if w < 0 {
        x += w;
        w = -w;
    }
    if h < 0 {
        y += h;
        h = -h;
    }
    if x < 0 {
        w += x;
        x = 0;
    }
    if y < 0 {
        h += y;
        y = 0;
    }
    let clip = Clip::read(core, gc)?;
    let x_start = x.max(clip.left);
    let x_end = (x + w).min(fb.width).min(clip.right);
    let y_start = y.max(clip.top);
    let y_end = (y + h).min(fb.height).min(clip.bottom);
    if x_start >= x_end || y_start >= y_end {
        return Ok(true);
    }

    let color: u32 = read_generic(core, gc + GC_FOREGROUND)?;
    let pixel = &color.to_le_bytes()[..size as usize];
    let row = pixel.repeat((x_end - x_start) as usize);
    for line in y_start..y_end {
        core.write_bytes(fb.buffer + line as u32 * fb.pitch + x_start as u32 * size, &row)?;
    }

    Ok(true)
}

/// `internalGrpCopyFrameBuffer(dst, dx, dy, w, h, src, sx, sy, gc, transparent)`.
fn copy_frame_buffer(core: &mut ArmCore) -> Result<bool> {
    let (dst, dx, dy, mut w, mut h, src, mut sx, mut sy, gc, transparent) = (
        param(core, 0)? as u32,
        param(core, 1)?,
        param(core, 2)?,
        param(core, 3)?,
        param(core, 4)?,
        param(core, 5)? as u32,
        param(core, 6)?,
        param(core, 7)?,
        param(core, 8)? as u32,
        param(core, 9)? as u8 != 0,
    );
    tracing::trace!("internalGrpCopyFrameBuffer({dst:#x}, {dx}, {dy}, {w}, {h}, {src:#x}, {sx}, {sy}, {gc:#x}, {transparent})");
    if !plain_drawing(core, gc)? {
        return Ok(false);
    }

    let target = FrameBuffer::read(core, dst)?;
    let source = FrameBuffer::read(core, src)?;
    // Pixels are copied in place one by one, so overlapping copies stay with the guest code.
    if target.buffer == source.buffer {
        return Ok(false);
    }
    let Some(size) = target.bytes_per_pixel() else {
        return Ok(true);
    };

    // Mirrors the guest's own clamping, including adjusting the width for a negative sy.
    if sx < 0 {
        w += sx;
        sx = 0;
    }
    if sy < 0 {
        w += sy;
        sy = 0;
    }
    if dx + w > target.width {
        w = target.width - dx;
    }
    if dy + h > target.height {
        h = target.height - dy;
    }

    let clip = Clip::read(core, gc)?;
    // Column offsets i where every horizontal bound holds.
    let i_start = 0.max(-dx).max(clip.left - dx);
    let i_end = w.min(source.width - sx).min(target.width - dx).min(clip.right - dx);
    if i_start >= i_end {
        return Ok(true);
    }
    let count = (i_end - i_start) as usize;
    let transparent_color: u32 = read_generic(core, gc + GC_TRANSPARENT)?;

    let mut source_row = vec![0u8; count * size as usize];
    let mut target_row = vec![0u8; count * size as usize];
    for j in 0..h {
        let (source_y, target_y) = (sy + j, dy + j);
        if source_y < 0 || source_y >= source.height || target_y < 0 || target_y >= target.height || target_y < clip.top || target_y >= clip.bottom {
            continue;
        }

        let source_address = source.buffer + source_y as u32 * source.pitch + (sx + i_start) as u32 * size;
        let target_address = target.buffer + target_y as u32 * target.pitch + (dx + i_start) as u32 * size;
        core.read_bytes(source_address, &mut source_row)?;

        if transparent {
            core.read_bytes(target_address, &mut target_row)?;
            for (source_pixel, target_pixel) in source_row.chunks_exact(size as usize).zip(target_row.chunks_exact_mut(size as usize)) {
                let value = if size == 2 {
                    u16::from_le_bytes([source_pixel[0], source_pixel[1]]) as u32
                } else {
                    u32::from_le_bytes([source_pixel[0], source_pixel[1], source_pixel[2], source_pixel[3]])
                };
                if value != transparent_color {
                    target_pixel.copy_from_slice(source_pixel);
                }
            }
            core.write_bytes(target_address, &target_row)?;
        } else {
            core.write_bytes(target_address, &source_row)?;
        }
    }

    Ok(true)
}

/// `MIDASKernelManager::hash(name)`: djb2 over the asset name.
fn asset_hash(name: &[u8]) -> u32 {
    name.iter().fold(5381u32, |hash, &c| hash.wrapping_mul(33).wrapping_add(c as u32))
}

/// Contents of the VFS asset `name`, walking the same `(name hash, size)` headers as the game.
pub fn vfs_asset<'a>(vfs: &'a [u8], name: &str) -> Option<&'a [u8]> {
    let hash = asset_hash(name.as_bytes());
    let mut position = 0usize;
    while let Some(header) = vfs.get(position..position.checked_add(8)?) {
        let entry_hash = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let data = vfs.get(position + 8..position + 8 + size)?;
        if entry_hash == hash {
            return Some(data);
        }
        position += 8 + size;
    }
    None
}

/// `MIDASKernelManager::getAssetSizeFromVFS(name, offset_out)`.
///
/// The VFS is a list of `(name hash, size)` headers each followed by its data. The guest walks it
/// with two 4-byte `fread`s and an `fseek` per entry, i.e. thousands of libc calls per asset; this
/// walks the same list in host memory. Returns the asset size and stores the data offset (relative
/// to the VFS start), or returns -1 when the name is missing.
fn get_asset_size_from_vfs(core: &mut ArmCore, context: &AndroidContext) -> Result<bool> {
    let (manager, name, offset_out) = (param(core, 0)? as u32, param(core, 1)? as u32, param(core, 2)? as u32);
    let fd: u32 = read_generic(core, manager + KM_VFS_FD)?;
    if fd != VFS_FD {
        return Ok(false);
    }
    let base: u32 = read_generic(core, manager + KM_VFS_OFFSET)?;
    let length: u32 = read_generic(core, manager + KM_VFS_LENGTH)?;
    let hash = asset_hash(&read_null_terminated_string_bytes(core, name)?);

    let vfs = &context.vfs;
    let header = |position: u32| -> Option<(u32, u32)> {
        let start = (base as usize).checked_add(position as usize)?;
        let bytes = vfs.get(start..start.checked_add(8)?)?;
        Some((
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        ))
    };

    let mut position = 0u32;
    let result = loop {
        // A truncated header makes the guest log an error; let it handle that case itself.
        let Some((entry_hash, size)) = header(position) else {
            return Ok(false);
        };
        position = position.wrapping_add(8);
        if entry_hash == hash {
            write_generic(core, offset_out, position)?;
            break size;
        }
        position = position.wrapping_add(size);
        if position >= length {
            break u32::MAX;
        }
    };
    tracing::trace!("getAssetSizeFromVFS({hash:#x}) = {result:#x} at {position:#x}");

    // The guest's stream shares the VFS descriptor's position; leave it where the walk ended.
    context.update_globals(core, |x| x.vfs_position = base.wrapping_add(position))?;
    core.write_return_value(&[result])?;

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::asset_hash;

    #[test]
    fn asset_hash_matches_guest_djb2() {
        assert_eq!(asset_hash(b""), 5381);
        assert_eq!(asset_hash(b"a"), 5381 * 33 + 97);
        // Bytes are unsigned and the sum wraps like the guest's 32-bit registers.
        assert_eq!(asset_hash(&[0xff; 8]), (0..8).fold(5381u32, |h, _| h.wrapping_mul(33).wrapping_add(0xff)));
    }
}
