use alloc::{format, vec, vec::Vec};

use wie_core_arm::{Allocator, ArmCore, ResultWriter, SvcId};
use wie_util::{ByteRead, ByteWrite, Result, WieError, read_generic, read_null_terminated_string_bytes, write_generic};

use super::{
    AndroidContext, SVC_CATEGORY_IMPORT, file,
    format::{ArgReader, format},
    gl, read_string,
};

/// Symbols the Android system libraries (libc, libm, libGLESv1_CM, liblog, libstdc++) provide.
const IMPORTS: &[&str] = &[
    "ANativeWindow_getHeight",
    "ANativeWindow_getWidth",
    "_ZdlPv",
    "_Znwj",
    "__android_log_print",
    "__cxa_begin_cleanup",
    "__cxa_call_unexpected",
    "__cxa_type_match",
    "__errno",
    "__gnu_Unwind_Find_exidx",
    "__stack_chk_fail",
    "abort",
    "access",
    "calloc",
    "ceil",
    "chdir",
    "close",
    "connect",
    "dup",
    "fclose",
    "fcntl",
    "fdopen",
    "floor",
    "fread",
    "free",
    "fseek",
    "ftime",
    "getsockopt",
    "glBindTexture",
    "glColor4f",
    "glCullFace",
    "glDeleteTextures",
    "glDisable",
    "glDisableClientState",
    "glDrawArrays",
    "glEnable",
    "glEnableClientState",
    "glGenTextures",
    "glLoadIdentity",
    "glMatrixMode",
    "glOrthof",
    "glTexCoordPointer",
    "glTexImage2D",
    "glTexParameterf",
    "glTexSubImage2D",
    "glVertexPointer",
    "glViewport",
    "inet_addr",
    "lseek",
    "malloc",
    "memcpy",
    "memmove",
    "memset",
    "mkdir",
    "open",
    "pow",
    "pthread_mutex_lock",
    "pthread_mutex_unlock",
    "qsort",
    "read",
    "recv",
    "rename",
    "rmdir",
    "select",
    "setsockopt",
    "shutdown",
    "socket",
    "stat",
    "strcat",
    "strcmp",
    "strcpy",
    "strdup",
    "strerror",
    "strlen",
    "strncmp",
    "strncpy",
    "strstr",
    "unlink",
    "vsnprintf",
    "write",
];

/// Bionic `FILE` streams are opaque to the game; a stream is a guest cell holding its descriptor.
const FILE_SIZE: u32 = 4;

pub fn resolve_import(core: &mut ArmCore, context: &AndroidContext, name: &str) -> Result<u32> {
    if name == "__stack_chk_guard" {
        return Ok(context.ptr_globals + 4);
    }

    let index = IMPORTS
        .iter()
        .position(|x| *x == name)
        .ok_or_else(|| WieError::Unimplemented(format!("Unsupported import {name}")))?;

    core.make_svc_stub(SVC_CATEGORY_IMPORT, index as u32)
}

pub fn register_import_svc_handler(core: &mut ArmCore, context: &AndroidContext) -> Result<()> {
    core.register_svc_handler(SVC_CATEGORY_IMPORT, handle_import_svc, context)
}

fn param(core: &ArmCore, index: usize) -> Result<u32> {
    core.read_param(index)
}

fn param_f64(core: &ArmCore, index: usize) -> Result<f64> {
    Ok(f64::from_bits(param(core, index)? as u64 | ((param(core, index + 1)? as u64) << 32)))
}

async fn handle_import_svc(core: &mut ArmCore, context: &mut AndroidContext, id: SvcId) -> Result<()> {
    let (_, lr) = core.read_pc_lr()?;
    let name = IMPORTS[id.0 as usize];

    match name {
        "malloc" | "_Znwj" => {
            let size = param(core, 0)?;
            malloc(core, size)?.write(core, lr)
        }
        "calloc" => {
            let size = param(core, 0)? * param(core, 1)?;
            let ptr = malloc(core, size)?;
            core.write_bytes(ptr, &vec![0; size as usize])?;
            ptr.write(core, lr)
        }
        "free" | "_ZdlPv" => {
            let ptr = param(core, 0)?;
            if ptr != 0 {
                let base = read_generic(core, ptr - 8)?;
                let size: u32 = read_generic(core, ptr - 4)?;
                Allocator::free(core, base, size + 12)?;
            }
            ().write(core, lr)
        }
        "memcpy" | "memmove" => {
            let (dst, src, size) = (param(core, 0)?, param(core, 1)?, param(core, 2)?);
            let data = read_bytes(core, src, size)?;
            core.write_bytes(dst, &data)?;
            dst.write(core, lr)
        }
        "memset" => {
            let (dst, value, size) = (param(core, 0)?, param(core, 1)?, param(core, 2)?);
            core.write_bytes(dst, &vec![value as u8; size as usize])?;
            dst.write(core, lr)
        }
        "strlen" => {
            let ptr = param(core, 0)?;
            (read_null_terminated_string_bytes(core, ptr)?.len() as u32).write(core, lr)
        }
        "strcmp" | "strncmp" => {
            let a = read_null_terminated_string_bytes(core, param(core, 0)?)?;
            let b = read_null_terminated_string_bytes(core, param(core, 1)?)?;
            let limit = if name == "strncmp" { param(core, 2)? as usize } else { usize::MAX };
            let a = &a[..a.len().min(limit)];
            let b = &b[..b.len().min(limit)];
            (a.cmp(b) as i32 as u32).write(core, lr)
        }
        "strcpy" | "strcat" => {
            let (dst, src) = (param(core, 0)?, param(core, 1)?);
            let mut bytes = read_null_terminated_string_bytes(core, src)?;
            bytes.push(0);
            let offset = if name == "strcat" {
                read_null_terminated_string_bytes(core, dst)?.len() as u32
            } else {
                0
            };
            core.write_bytes(dst + offset, &bytes)?;
            dst.write(core, lr)
        }
        "strncpy" => {
            let (dst, src, size) = (param(core, 0)?, param(core, 1)?, param(core, 2)? as usize);
            let mut bytes = read_null_terminated_string_bytes(core, src)?;
            bytes.resize(size, 0);
            core.write_bytes(dst, &bytes[..size])?;
            dst.write(core, lr)
        }
        "strdup" => {
            let mut bytes = read_null_terminated_string_bytes(core, param(core, 0)?)?;
            bytes.push(0);
            let ptr = malloc(core, bytes.len() as u32)?;
            core.write_bytes(ptr, &bytes)?;
            ptr.write(core, lr)
        }
        "strstr" => {
            let haystack_ptr = param(core, 0)?;
            let haystack = read_null_terminated_string_bytes(core, haystack_ptr)?;
            let needle = read_null_terminated_string_bytes(core, param(core, 1)?)?;
            let found = if needle.is_empty() {
                Some(0)
            } else {
                haystack.windows(needle.len()).position(|x| x == needle)
            };
            found.map_or(0, |x| haystack_ptr + x as u32).write(core, lr)
        }
        "strerror" => {
            let message = format!("error {}\0", param(core, 0)?);
            let ptr = malloc(core, message.len() as u32)?;
            core.write_bytes(ptr, message.as_bytes())?;
            ptr.write(core, lr)
        }
        "vsnprintf" => {
            let (dst, size, fmt, va_list) = (param(core, 0)?, param(core, 1)?, param(core, 2)?, param(core, 3)?);
            let fmt = read_null_terminated_string_bytes(core, fmt)?;
            let text = format(core, &fmt, &mut ArgReader::Memory(va_list))?;
            if size > 0 {
                let count = text.len().min(size as usize - 1);
                core.write_bytes(dst, &text[..count])?;
                core.write_bytes(dst + count as u32, &[0])?;
            }
            (text.len() as u32).write(core, lr)
        }
        "__android_log_print" => {
            let (priority, tag, fmt) = (param(core, 0)?, param(core, 1)?, param(core, 2)?);
            let tag = read_string(core, tag)?;
            let fmt = read_null_terminated_string_bytes(core, fmt)?;
            let text = format(core, &fmt, &mut ArgReader::Params(3))?;
            tracing::info!("[android {priority}] {tag}: {}", alloc::string::String::from_utf8_lossy(&text));
            1u32.write(core, lr)
        }
        "ANativeWindow_getWidth" | "ANativeWindow_getHeight" => {
            let window = param(core, 0)?;
            let screen = context.system.platform().screen();
            let size = if name == "ANativeWindow_getWidth" {
                screen.width()
            } else {
                screen.height()
            };
            tracing::debug!("{name}({window:#x}) -> {size}");
            size.write(core, lr)
        }
        "__errno" => (context.ptr_globals).write(core, lr),
        "pthread_mutex_lock" | "pthread_mutex_unlock" => 0u32.write(core, lr),
        "ceil" => param_f64(core, 0)?.ceil_compat().write(core, lr),
        "floor" => param_f64(core, 0)?.floor_compat().write(core, lr),
        "pow" => {
            let (base, exponent) = (param_f64(core, 0)?, param_f64(core, 2)?);
            pow(base, exponent).write(core, lr)
        }
        "qsort" => {
            let (base, count, size, compare) = (param(core, 0)?, param(core, 1)?, param(core, 2)?, param(core, 3)?);
            tracing::debug!("qsort({base:#x}, {count}, {size}, {compare:#x})");
            qsort(core, base, count, size, compare).await?;
            ().write(core, lr)
        }
        "ftime" => {
            let ptr = param(core, 0)?;
            let now = context.system.platform().now().raw();
            write_generic(core, ptr, (now / 1000) as u32)?;
            write_generic(core, ptr + 4, (now % 1000) as u16)?;
            write_generic(core, ptr + 6, 0u32)?;
            0u32.write(core, lr)
        }
        "lseek" => {
            let (fd, offset, whence) = (param(core, 0)?, param(core, 1)? as i32, param(core, 2)?);
            tracing::debug!("lseek({fd}, {offset}, {whence})");
            file::lseek(core, context, fd, offset, whence).await?.write(core, lr)
        }
        "read" => {
            let (fd, buf, count) = (param(core, 0)?, param(core, 1)?, param(core, 2)?);
            tracing::trace!("read({fd}, {buf:#x}, {count})");
            file::read(core, context, fd, buf, count).await?.write(core, lr)
        }
        "write" => {
            let (fd, buf, count) = (param(core, 0)?, param(core, 1)?, param(core, 2)?);
            tracing::debug!("write({fd}, {buf:#x}, {count})");
            file::write(core, context, fd, buf, count).await?.write(core, lr)
        }
        "dup" => {
            let fd = param(core, 0)?;
            tracing::debug!("dup({fd})");
            file::dup(core, context, fd).await?.write(core, lr)
        }
        "close" => {
            let fd = param(core, 0)?;
            tracing::debug!("close({fd})");
            file::close(core, context, fd)?.write(core, lr)
        }
        "open" => {
            let (path, flags) = (read_string(core, param(core, 0)?)?, param(core, 1)?);
            let fd = file::open(core, context, &path, flags).await?;
            tracing::debug!("open({path}, {flags:#x}) -> {fd}");
            fd.write(core, lr)
        }
        "stat" => {
            let (path, buf) = (read_string(core, param(core, 0)?)?, param(core, 1)?);
            let result = file::stat(core, context, &path, buf).await?;
            tracing::debug!("stat({path}) -> {result:#x}");
            result.write(core, lr)
        }
        "access" => {
            let path = read_string(core, param(core, 0)?)?;
            let result = file::access(core, context, &path).await?;
            tracing::debug!("access({path}) -> {result:#x}");
            result.write(core, lr)
        }
        "unlink" => {
            let path = read_string(core, param(core, 0)?)?;
            tracing::debug!("unlink({path})");
            file::unlink(core, context, &path).await?.write(core, lr)
        }
        "rename" => {
            let (from, to) = (read_string(core, param(core, 0)?)?, read_string(core, param(core, 1)?)?);
            tracing::debug!("rename({from}, {to})");
            file::rename(core, context, &from, &to).await?.write(core, lr)
        }
        "chdir" => {
            let path = read_string(core, param(core, 0)?)?;
            tracing::debug!("chdir({path})");
            file::chdir(core, context, &path)?.write(core, lr)
        }
        "mkdir" | "rmdir" => {
            let path = read_string(core, param(core, 0)?)?;
            tracing::debug!("{name}({path})");
            0u32.write(core, lr)
        }
        "fdopen" => {
            let fd = param(core, 0)?;
            tracing::debug!("fdopen({fd})");
            let stream = Allocator::alloc(core, FILE_SIZE)?;
            write_generic(core, stream, fd)?;
            stream.write(core, lr)
        }
        "fseek" => {
            let (stream, offset, whence) = (param(core, 0)?, param(core, 1)? as i32, param(core, 2)?);
            tracing::debug!("fseek({stream:#x}, {offset}, {whence})");
            let fd = read_generic(core, stream)?;
            let result = file::lseek(core, context, fd, offset, whence).await?;
            if result == u32::MAX { result } else { 0 }.write(core, lr)
        }
        "fread" => {
            let (buf, size, count, stream) = (param(core, 0)?, param(core, 1)?, param(core, 2)?, param(core, 3)?);
            tracing::trace!("fread({buf:#x}, {size}, {count}, {stream:#x})");
            let fd = read_generic(core, stream)?;
            let read = file::read(core, context, fd, buf, size * count).await?;
            if read == u32::MAX || size == 0 { 0 } else { read / size }.write(core, lr)
        }
        "fclose" => {
            let stream = param(core, 0)?;
            tracing::debug!("fclose({stream:#x})");
            let fd = read_generic(core, stream)?;
            Allocator::free(core, stream, FILE_SIZE)?;
            file::close(core, context, fd)?.write(core, lr)
        }
        "socket" | "connect" | "recv" | "select" | "setsockopt" | "getsockopt" | "shutdown" | "fcntl" | "inet_addr" => {
            tracing::warn!("stub {name}({:#x}, {:#x}, {:#x})", param(core, 0)?, param(core, 1)?, param(core, 2)?);
            u32::MAX.write(core, lr)
        }
        "glGenTextures" => {
            let (count, ptr) = (param(core, 0)?, param(core, 1)?);
            tracing::debug!("glGenTextures({count}, {ptr:#x})");
            let first = context.globals(core)?.next_texture_id;
            for index in 0..count {
                write_generic(core, ptr + index * 4, first + index)?;
            }
            context.update_globals(core, |x| x.next_texture_id += count)?;
            ().write(core, lr)
        }
        name if name.starts_with("gl") => {
            gl::call(core, context, name).await?;
            ().write(core, lr)
        }
        "__stack_chk_fail" | "abort" => Err(WieError::FatalError(format!("Native code called {name}"))),
        _ => Err(WieError::Unimplemented(format!("Unimplemented import {name}"))),
    }
}

fn malloc(core: &mut ArmCore, size: u32) -> Result<u32> {
    // Keep the allocation base and size before an eight-byte-aligned guest pointer.
    let base = Allocator::alloc(core, size + 12)?;
    let ptr = (base + 12) & !7;
    write_generic(core, ptr - 8, base)?;
    write_generic(core, ptr - 4, size)?;
    // Fresh bionic heap pages are zeroed and the game relies on it, so hand out cleared memory.
    core.write_bytes(ptr, &vec![0; size as usize])?;
    Ok(ptr)
}

fn read_bytes(core: &ArmCore, address: u32, size: u32) -> Result<Vec<u8>> {
    let mut data = vec![0; size as usize];
    core.read_bytes(address, &mut data)?;
    Ok(data)
}

async fn qsort(core: &mut ArmCore, base: u32, count: u32, size: u32, compare: u32) -> Result<()> {
    // Insertion sort keeps the comparator calls in guest code.
    let temp = Allocator::alloc(core, size)?;
    for i in 1..count {
        let item = read_bytes(core, base + i * size, size)?;
        core.write_bytes(temp, &item)?;
        let mut j = i;
        while j > 0 {
            let previous = base + (j - 1) * size;
            let order: u32 = core.run_function(compare, &[previous, temp]).await?;
            if (order as i32) <= 0 {
                break;
            }
            let data = read_bytes(core, previous, size)?;
            core.write_bytes(previous + size, &data)?;
            j -= 1;
        }
        core.write_bytes(base + j * size, &item)?;
    }
    Allocator::free(core, temp, size)
}

/// `f64::ceil`/`floor`/`powf` need `std`; these keep the no_std crate self-contained.
trait FloatCompat {
    fn floor_compat(self) -> f64;
    fn ceil_compat(self) -> f64;
}

impl FloatCompat for f64 {
    fn floor_compat(self) -> f64 {
        if !self.is_finite() || self.abs() >= 4503599627370496.0 {
            return self;
        }
        let truncated = self as i64 as f64;
        if truncated > self { truncated - 1.0 } else { truncated }
    }

    fn ceil_compat(self) -> f64 {
        -(-self).floor_compat()
    }
}

fn pow(base: f64, exponent: f64) -> f64 {
    if exponent == exponent.floor_compat() && exponent.abs() < 64.0 {
        let mut result = 1.0;
        for _ in 0..exponent.abs() as u32 {
            result *= base;
        }
        return if exponent < 0.0 { 1.0 / result } else { result };
    }
    tracing::warn!("pow({base}, {exponent}) with fractional exponent");
    exp(exponent * ln(base))
}

fn ln(value: f64) -> f64 {
    if value <= 0.0 {
        return f64::NAN;
    }
    let mut exponent = 0;
    let mut mantissa = value;
    while mantissa > 2.0 {
        mantissa /= 2.0;
        exponent += 1;
    }
    while mantissa < 1.0 {
        mantissa *= 2.0;
        exponent -= 1;
    }
    let y = (mantissa - 1.0) / (mantissa + 1.0);
    let mut sum = 0.0;
    let mut term = y;
    for n in 0..40 {
        sum += term / (2 * n + 1) as f64;
        term *= y * y;
    }
    2.0 * sum + exponent as f64 * core::f64::consts::LN_2
}

fn exp(value: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    for n in 1..60 {
        term *= value / n as f64;
        sum += term;
    }
    sum
}
