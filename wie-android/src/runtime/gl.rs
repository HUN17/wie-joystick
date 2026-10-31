use alloc::{format, vec, vec::Vec};

use bytemuck::{Pod, Zeroable, cast_slice, cast_slice_mut};

use wie_backend::canvas::{ArgbPixel, VecImageBuffer};
use wie_core_arm::{Allocator, ArmCore};
use wie_util::{ByteRead, ByteWrite, Result, read_generic, write_generic};

use super::AndroidContext;

pub const MAX_TEXTURES: u32 = 64;

const GL_TRIANGLE_STRIP: u32 = 5;
const GL_TRIANGLE_FAN: u32 = 6;
const GL_BLEND: u32 = 0x0be2;

const GL_SHORT: u32 = 0x1402;
const GL_FLOAT: u32 = 0x1406;
const GL_FIXED: u32 = 0x140c;

const GL_RGB: u32 = 0x1907;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_UNSIGNED_SHORT_4_4_4_4: u32 = 0x8033;
const GL_UNSIGNED_SHORT_5_5_5_1: u32 = 0x8034;
const GL_UNSIGNED_SHORT_5_6_5: u32 = 0x8363;

/// Default `GL_UNPACK_ALIGNMENT`; the game never changes it.
const UNPACK_ALIGNMENT: u32 = 4;

#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct ArrayPointer {
    size: u32,
    r#type: u32,
    stride: u32,
    pointer: u32,
}

/// Texture storage converted to ARGB8888 in guest memory.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct Texture {
    width: u32,
    height: u32,
    data: u32,
}

/// OpenGL ES 1.x state for the fixed 2D textured-quad pipeline the Clet bridge uses.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct GlState {
    viewport: [i32; 4],
    ortho: [f32; 4],
    bound_texture: u32,
    blend: u32,
    vertex: ArrayPointer,
    texcoord: ArrayPointer,
    framebuffer: u32,
    framebuffer_width: u32,
    framebuffer_height: u32,
    dirty: u32,
    textures: [Texture; MAX_TEXTURES as usize],
}

fn state(core: &ArmCore, context: &AndroidContext) -> Result<GlState> {
    read_generic(core, context.globals(core)?.gl_state)
}

fn store(core: &mut ArmCore, context: &AndroidContext, state: GlState) -> Result<()> {
    let address = context.globals(core)?.gl_state;
    write_generic(core, address, state)
}

fn param(core: &ArmCore, index: usize) -> Result<u32> {
    core.read_param(index)
}

pub async fn call(core: &mut ArmCore, context: &AndroidContext, name: &str) -> Result<()> {
    let mut state = state(core, context)?;

    match name {
        "glViewport" => {
            let viewport = [
                param(core, 0)? as i32,
                param(core, 1)? as i32,
                param(core, 2)? as i32,
                param(core, 3)? as i32,
            ];
            tracing::debug!("glViewport({viewport:?})");
            state.viewport = viewport;

            let (width, height) = (viewport[2] as u32, viewport[3] as u32);
            if state.framebuffer_width != width || state.framebuffer_height != height {
                if state.framebuffer != 0 {
                    Allocator::free(core, state.framebuffer, state.framebuffer_width * state.framebuffer_height * 4)?;
                }
                state.framebuffer = Allocator::alloc(core, width * height * 4)?;
                core.write_bytes(state.framebuffer, &vec![0; (width * height * 4) as usize])?;
                state.framebuffer_width = width;
                state.framebuffer_height = height;
                context.system.platform().screen().resize(width, height)?;
            }
        }
        "glOrthof" => {
            let ortho = [0, 1, 2, 3].map(|x| f32::from_bits(core.read_param(x).unwrap_or(0)));
            tracing::debug!("glOrthof({ortho:?})");
            state.ortho = ortho;
        }
        "glEnable" | "glDisable" => {
            let capability = param(core, 0)?;
            tracing::trace!("{name}({capability:#x})");
            if capability == GL_BLEND {
                state.blend = (name == "glEnable") as u32;
            }
        }
        "glBindTexture" => {
            state.bound_texture = param(core, 1)?;
            tracing::trace!("glBindTexture({})", state.bound_texture);
        }
        "glVertexPointer" | "glTexCoordPointer" => {
            let pointer = ArrayPointer {
                size: param(core, 0)?,
                r#type: param(core, 1)?,
                stride: param(core, 2)?,
                pointer: param(core, 3)?,
            };
            if name == "glVertexPointer" {
                state.vertex = pointer;
            } else {
                state.texcoord = pointer;
            }
        }
        "glTexImage2D" => {
            let (width, height, format, r#type, pixels) = (param(core, 3)?, param(core, 4)?, param(core, 6)?, param(core, 7)?, param(core, 8)?);
            tracing::debug!("glTexImage2D({}, {width}x{height}, {format:#x}, {type:#x}, {pixels:#x})", state.bound_texture, type = r#type);
            let Some(texture) = state.textures.get_mut(state.bound_texture as usize) else {
                return Err(wie_util::WieError::FatalError(format!("Texture id {} out of range", state.bound_texture)));
            };
            if texture.data != 0 {
                Allocator::free(core, texture.data, texture.width * texture.height * 4)?;
            }
            *texture = Texture {
                width,
                height,
                data: Allocator::alloc(core, width * height * 4)?,
            };
            let texture = *texture;
            core.write_bytes(texture.data, &vec![0; (width * height * 4) as usize])?;
            if pixels != 0 {
                upload(core, texture, 0, 0, width, height, format, r#type, pixels)?;
            }
        }
        "glTexSubImage2D" => {
            let (x, y, width, height, format, r#type, pixels) = (
                param(core, 2)?,
                param(core, 3)?,
                param(core, 4)?,
                param(core, 5)?,
                param(core, 6)?,
                param(core, 7)?,
                param(core, 8)?,
            );
            tracing::trace!("glTexSubImage2D({}, {x}, {y}, {width}x{height}, {format:#x}, {type:#x})", state.bound_texture, type = r#type);
            if let Some(texture) = state.textures.get(state.bound_texture as usize).copied().filter(|x| x.data != 0) {
                upload(core, texture, x, y, width, height, format, r#type, pixels)?;
            }
        }
        "glDeleteTextures" => {
            let (count, ids) = (param(core, 0)?, param(core, 1)?);
            for index in 0..count {
                let id: u32 = read_generic(core, ids + index * 4)?;
                if let Some(texture) = state.textures.get_mut(id as usize) {
                    if texture.data != 0 {
                        Allocator::free(core, texture.data, texture.width * texture.height * 4)?;
                    }
                    *texture = Texture::zeroed();
                }
            }
        }
        "glDrawArrays" => {
            let (mode, first, count) = (param(core, 0)?, param(core, 1)?, param(core, 2)?);
            tracing::trace!("glDrawArrays({mode}, {first}, {count})");
            if (mode == GL_TRIANGLE_STRIP || mode == GL_TRIANGLE_FAN) && count == 4 {
                draw_quad(core, &state, first)?;
                state.dirty = 1;
            } else {
                tracing::warn!("stub glDrawArrays({mode}, {first}, {count})");
            }
        }
        _ => {
            tracing::trace!("stub {name}({:#x}, {:#x}, {:#x})", param(core, 0)?, param(core, 1)?, param(core, 2)?);
        }
    }

    store(core, context, state)
}

#[allow(clippy::too_many_arguments)]
fn upload(core: &mut ArmCore, texture: Texture, x: u32, y: u32, width: u32, height: u32, format: u32, r#type: u32, pixels: u32) -> Result<()> {
    let bytes_per_pixel = match (format, r#type) {
        (GL_RGB, GL_UNSIGNED_BYTE) => 3,
        (GL_RGBA, GL_UNSIGNED_BYTE) => 4,
        (_, GL_UNSIGNED_SHORT_5_6_5 | GL_UNSIGNED_SHORT_4_4_4_4 | GL_UNSIGNED_SHORT_5_5_5_1) => 2,
        _ => {
            tracing::warn!("Unsupported texture format {format:#x}/{type:#x}", type = r#type);
            return Ok(());
        }
    };
    let pitch = (width * bytes_per_pixel).next_multiple_of(UNPACK_ALIGNMENT);
    let width = width.min(texture.width.saturating_sub(x));
    let height = height.min(texture.height.saturating_sub(y));

    let mut source = vec![0u8; (pitch * height) as usize];
    core.read_bytes(pixels, &mut source)?;

    let mut row = vec![0u32; width as usize];
    for line in 0..height {
        let src = &source[(line * pitch) as usize..];
        for (column, pixel) in row.iter_mut().enumerate() {
            let offset = column * bytes_per_pixel as usize;
            *pixel = match (format, r#type) {
                (GL_RGB, GL_UNSIGNED_BYTE) => 0xff00_0000 | (src[offset] as u32) << 16 | (src[offset + 1] as u32) << 8 | src[offset + 2] as u32,
                (GL_RGBA, GL_UNSIGNED_BYTE) => {
                    (src[offset + 3] as u32) << 24 | (src[offset] as u32) << 16 | (src[offset + 1] as u32) << 8 | src[offset + 2] as u32
                }
                _ => {
                    let value = u16::from_le_bytes([src[offset], src[offset + 1]]) as u32;
                    match r#type {
                        GL_UNSIGNED_SHORT_5_6_5 => {
                            0xff00_0000 | expand(value >> 11, 5) << 16 | expand((value >> 5) & 0x3f, 6) << 8 | expand(value & 0x1f, 5)
                        }
                        GL_UNSIGNED_SHORT_4_4_4_4 => {
                            expand(value & 0xf, 4) << 24
                                | expand(value >> 12, 4) << 16
                                | expand((value >> 8) & 0xf, 4) << 8
                                | expand((value >> 4) & 0xf, 4)
                        }
                        _ => {
                            (if value & 1 != 0 { 0xff00_0000 } else { 0 })
                                | expand(value >> 11, 5) << 16
                                | expand((value >> 6) & 0x1f, 5) << 8
                                | expand((value >> 1) & 0x1f, 5)
                        }
                    }
                }
            };
        }
        let address = texture.data + ((y + line) * texture.width + x) * 4;
        core.write_bytes(address, cast_slice(&row))?;
    }

    Ok(())
}

fn expand(value: u32, bits: u32) -> u32 {
    (value << (8 - bits)) | (value >> (2 * bits - 8).min(bits))
}

fn read_array(core: &ArmCore, array: &ArrayPointer, index: u32) -> Result<[f32; 2]> {
    let element_size = match array.r#type {
        GL_SHORT => 2,
        _ => 4,
    };
    let stride = if array.stride == 0 { array.size * element_size } else { array.stride };
    let address = array.pointer + index * stride;

    let component = |offset: u32| -> Result<f32> {
        Ok(match array.r#type {
            GL_FLOAT => f32::from_bits(read_generic(core, address + offset * 4)?),
            GL_FIXED => read_generic::<i32, _>(core, address + offset * 4)? as f32 / 65536.0,
            GL_SHORT => read_generic::<i16, _>(core, address + offset * 2)? as f32,
            _ => 0.0,
        })
    };

    Ok([component(0)?, component(1)?])
}

/// Draws an axis-aligned textured quad by nearest-neighbor sampling into the guest framebuffer.
fn draw_quad(core: &mut ArmCore, state: &GlState, first: u32) -> Result<()> {
    let Some(texture) = state.textures.get(state.bound_texture as usize).copied().filter(|x| x.data != 0) else {
        return Ok(());
    };
    if state.framebuffer == 0 {
        return Ok(());
    }

    let [left, right, bottom, top] = state.ortho;
    let [vx, vy, vw, vh] = state.viewport.map(|x| x as f32);
    let fb_height = state.framebuffer_height as f32;

    // Window coordinates with a top-left origin, paired with texel coordinates.
    let mut corners = [([0f32; 2], [0f32; 2]); 4];
    for (index, corner) in corners.iter_mut().enumerate() {
        let [x, y] = read_array(core, &state.vertex, first + index as u32)?;
        let [u, v] = read_array(core, &state.texcoord, first + index as u32)?;
        let window_x = vx + (x - left) / (right - left) * vw;
        let window_y = fb_height - (vy + (y - bottom) / (top - bottom) * vh);
        *corner = ([window_x, window_y], [u * texture.width as f32, v * texture.height as f32]);
    }

    let min_x = corners.iter().map(|x| x.0[0]).fold(f32::MAX, f32::min);
    let max_x = corners.iter().map(|x| x.0[0]).fold(f32::MIN, f32::max);
    let min_y = corners.iter().map(|x| x.0[1]).fold(f32::MAX, f32::min);
    let max_y = corners.iter().map(|x| x.0[1]).fold(f32::MIN, f32::max);
    if max_x - min_x < 0.5 || max_y - min_y < 0.5 {
        return Ok(());
    }

    // Texel coordinate at the left/right and top/bottom window edges.
    let edge = |axis: usize, value: f32| {
        corners
            .iter()
            .min_by(|a, b| (a.0[axis] - value).abs().total_cmp(&(b.0[axis] - value).abs()))
            .map(|x| x.1[axis])
            .unwrap_or(0.0)
    };
    let (u0, u1, v0, v1) = (edge(0, min_x), edge(0, max_x), edge(1, min_y), edge(1, max_y));

    let x_start = min_x.round().max(0.0) as u32;
    let x_end = (max_x.round() as u32).min(state.framebuffer_width);
    let y_start = min_y.round().max(0.0) as u32;
    let y_end = (max_y.round() as u32).min(state.framebuffer_height);
    if x_start >= x_end || y_start >= y_end {
        return Ok(());
    }

    let mut texels = vec![0u32; (texture.width * texture.height) as usize];
    core.read_bytes(texture.data, cast_slice_mut(&mut texels))?;

    let width = (x_end - x_start) as usize;
    let mut row = vec![0u32; width];
    for y in y_start..y_end {
        let ty = v0 + (y as f32 + 0.5 - min_y) / (max_y - min_y) * (v1 - v0);
        let ty = (ty as i32).clamp(0, texture.height as i32 - 1) as u32;
        let address = state.framebuffer + (y * state.framebuffer_width + x_start) * 4;
        if state.blend != 0 {
            core.read_bytes(address, cast_slice_mut(&mut row))?;
        }
        for (column, pixel) in row.iter_mut().enumerate() {
            let x = x_start as f32 + column as f32 + 0.5;
            let tx = u0 + (x - min_x) / (max_x - min_x) * (u1 - u0);
            let tx = (tx as i32).clamp(0, texture.width as i32 - 1) as u32;
            let texel = texels[(ty * texture.width + tx) as usize];
            *pixel = if state.blend != 0 { blend(texel, *pixel) } else { texel | 0xff00_0000 };
        }
        core.write_bytes(address, cast_slice(&row))?;
    }

    Ok(())
}

fn blend(source: u32, destination: u32) -> u32 {
    let alpha = source >> 24;
    let mix = |shift: u32| (((source >> shift) & 0xff) * alpha + ((destination >> shift) & 0xff) * (255 - alpha)) / 255;
    0xff00_0000 | mix(16) << 16 | mix(8) << 8 | mix(0)
}

/// Paints the framebuffer if any quad was drawn since the last present.
pub fn present(core: &mut ArmCore, context: &AndroidContext) -> Result<()> {
    let mut state = state(core, context)?;
    if state.dirty == 0 {
        return Ok(());
    }
    state.dirty = 0;
    store(core, context, state)?;

    let mut pixels: Vec<u32> = vec![0; (state.framebuffer_width * state.framebuffer_height) as usize];
    core.read_bytes(state.framebuffer, cast_slice_mut(&mut pixels))?;
    let image = VecImageBuffer::<ArgbPixel>::from_raw(state.framebuffer_width, state.framebuffer_height, pixels);
    context.system.platform().screen().paint(&image);

    Ok(())
}
