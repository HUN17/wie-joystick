use alloc::{format, string::String, vec};

use bytemuck::{Pod, Zeroable};

use wie_core_arm::{Allocator, ArmCore};
use wie_util::{ByteRead, ByteWrite, Result, read_generic, write_generic};

use super::{AndroidContext, VFS_FD, read_string};

pub const MAX_FILES: u32 = 64;
const FIRST_FD: u32 = 3;

const O_ACCMODE: u32 = 3;
const O_CREAT: u32 = 0x40;
const O_TRUNC: u32 = 0x200;
const O_APPEND: u32 = 0x400;

const ENOENT: u32 = 2;
const EBADF: u32 = 9;
const EMFILE: u32 = 24;

const STAT_SIZE: usize = 104;
const STAT_MODE_OFFSET: u32 = 16;
const STAT_SIZE_OFFSET: u32 = 48;
const S_IFREG: u32 = 0o100644;
const S_IFDIR: u32 = 0o040755;

/// Open file description stored in the guest descriptor table; a zero path marks a free slot.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct OpenFile {
    path: u32,
    position: u32,
    flags: u32,
}

fn set_errno(core: &mut ArmCore, context: &AndroidContext, errno: u32) -> Result<u32> {
    context.update_globals(core, |x| x.errno = errno)?;
    Ok(u32::MAX)
}

/// Maps Android storage paths onto the per-app filesystem namespace.
pub fn host_path(core: &ArmCore, context: &AndroidContext, path: &str) -> Result<String> {
    let pid = context.system.pid();
    let internal = format!("/data/data/{pid}/files");
    let external = format!("/sdcard/{pid}");

    let absolute = if path.starts_with('/') {
        String::from(path)
    } else {
        let cwd = context.globals(core)?.cwd;
        let cwd = if cwd == 0 { internal.clone() } else { read_string(core, cwd)? };
        format!("{cwd}/{path}")
    };

    Ok(if let Some(rest) = absolute.strip_prefix(&internal) {
        format!("files/{rest}")
    } else if let Some(rest) = absolute.strip_prefix(&external) {
        format!("sdcard/{rest}")
    } else {
        absolute
    })
}

fn entry_address(context: &AndroidContext, core: &ArmCore, fd: u32) -> Result<Option<u32>> {
    if !(FIRST_FD..FIRST_FD + MAX_FILES).contains(&fd) {
        return Ok(None);
    }
    let address = context.globals(core)?.fd_table + (fd - FIRST_FD) * size_of::<OpenFile>() as u32;
    let entry: OpenFile = read_generic(core, address)?;
    Ok((entry.path != 0).then_some(address))
}

fn open_entry(core: &ArmCore, context: &AndroidContext, fd: u32) -> Result<Option<(u32, OpenFile, String)>> {
    let Some(address) = entry_address(context, core, fd)? else {
        return Ok(None);
    };
    let entry: OpenFile = read_generic(core, address)?;
    let path = read_string(core, entry.path)?;
    Ok(Some((address, entry, path)))
}

pub async fn open(core: &mut ArmCore, context: &AndroidContext, path: &str, flags: u32) -> Result<u32> {
    let path = host_path(core, context, path)?;
    let filesystem = context.system.filesystem();

    if !filesystem.exists(&path).await {
        if flags & O_CREAT == 0 {
            return set_errno(core, context, ENOENT);
        }
        filesystem.write(&path, 0, &[]).await;
    } else if flags & O_TRUNC != 0 && flags & O_ACCMODE != 0 {
        filesystem.truncate(&path, 0).await;
    }

    let table = context.globals(core)?.fd_table;
    for index in 0..MAX_FILES {
        let address = table + index * size_of::<OpenFile>() as u32;
        let entry: OpenFile = read_generic(core, address)?;
        if entry.path == 0 {
            let mut bytes = path.into_bytes();
            bytes.push(0);
            let ptr_path = Allocator::alloc(core, bytes.len() as u32)?;
            core.write_bytes(ptr_path, &bytes)?;
            write_generic(
                core,
                address,
                OpenFile {
                    path: ptr_path,
                    position: 0,
                    flags,
                },
            )?;
            return Ok(index + FIRST_FD);
        }
    }

    set_errno(core, context, EMFILE)
}

pub fn close(core: &mut ArmCore, context: &AndroidContext, fd: u32) -> Result<u32> {
    if fd == VFS_FD {
        return Ok(0);
    }
    let Some((address, entry, path)) = open_entry(core, context, fd)? else {
        return set_errno(core, context, EBADF);
    };
    Allocator::free(core, entry.path, path.len() as u32 + 1)?;
    write_generic(core, address, OpenFile::zeroed())?;
    Ok(0)
}

pub async fn dup(core: &mut ArmCore, context: &AndroidContext, fd: u32) -> Result<u32> {
    if fd == VFS_FD {
        return Ok(fd);
    }
    let Some((_, entry, path)) = open_entry(core, context, fd)? else {
        return set_errno(core, context, EBADF);
    };
    let new_fd = open(core, context, &format!("/{path}"), entry.flags & !(O_CREAT | O_TRUNC)).await?;
    if let Some(address) = entry_address(context, core, new_fd)? {
        let mut new_entry: OpenFile = read_generic(core, address)?;
        new_entry.position = entry.position;
        write_generic(core, address, new_entry)?;
    }
    Ok(new_fd)
}

pub async fn read(core: &mut ArmCore, context: &AndroidContext, fd: u32, buf: u32, count: u32) -> Result<u32> {
    if fd == VFS_FD {
        let position = (context.globals(core)?.vfs_position as usize).min(context.vfs.len());
        let end = (position + count as usize).min(context.vfs.len());
        core.write_bytes(buf, &context.vfs[position..end])?;
        context.update_globals(core, |x| x.vfs_position = end as u32)?;
        return Ok((end - position) as u32);
    }

    let Some((address, mut entry, path)) = open_entry(core, context, fd)? else {
        return set_errno(core, context, EBADF);
    };
    let mut data = vec![0; count as usize];
    let read = context
        .system
        .filesystem()
        .read(&path, entry.position as usize, count as usize, &mut data)
        .await
        .unwrap_or(0);
    core.write_bytes(buf, &data[..read])?;
    entry.position += read as u32;
    write_generic(core, address, entry)?;
    Ok(read as u32)
}

pub async fn write(core: &mut ArmCore, context: &AndroidContext, fd: u32, buf: u32, count: u32) -> Result<u32> {
    let Some((address, mut entry, path)) = open_entry(core, context, fd)? else {
        return set_errno(core, context, EBADF);
    };
    let mut data = vec![0; count as usize];
    core.read_bytes(buf, &mut data)?;

    let filesystem = context.system.filesystem();
    if entry.flags & O_APPEND != 0 {
        entry.position = filesystem.size(&path).await.unwrap_or(0) as u32;
    }
    let written = filesystem.write(&path, entry.position as usize, &data).await;
    entry.position += written as u32;
    write_generic(core, address, entry)?;
    Ok(written as u32)
}

pub async fn lseek(core: &mut ArmCore, context: &AndroidContext, fd: u32, offset: i32, whence: u32) -> Result<u32> {
    if fd == VFS_FD {
        let current = context.globals(core)?.vfs_position as i64;
        let position = match whence {
            0 => offset as i64,
            1 => current + offset as i64,
            _ => context.vfs.len() as i64 + offset as i64,
        };
        context.update_globals(core, |x| x.vfs_position = position as u32)?;
        return Ok(position as u32);
    }

    let Some((address, mut entry, path)) = open_entry(core, context, fd)? else {
        return set_errno(core, context, EBADF);
    };
    let base = match whence {
        0 => 0,
        1 => entry.position as i64,
        _ => context.system.filesystem().size(&path).await.unwrap_or(0) as i64,
    };
    entry.position = (base + offset as i64) as u32;
    write_generic(core, address, entry)?;
    Ok(entry.position)
}

/// Directories are implicit in the backing store; only the storage roots report as directories.
pub async fn stat(core: &mut ArmCore, context: &AndroidContext, path: &str, buf: u32) -> Result<u32> {
    let path = host_path(core, context, path)?;
    let filesystem = context.system.filesystem();

    let (mode, size) = if let Some(size) = filesystem.size(&path).await {
        (S_IFREG, size as u64)
    } else if is_storage_root(&path) {
        (S_IFDIR, 0)
    } else {
        return set_errno(core, context, ENOENT);
    };

    core.write_bytes(buf, &[0; STAT_SIZE])?;
    write_generic(core, buf + STAT_MODE_OFFSET, mode)?;
    write_generic(core, buf + STAT_SIZE_OFFSET, size)?;
    Ok(0)
}

pub async fn access(core: &mut ArmCore, context: &AndroidContext, path: &str) -> Result<u32> {
    let path = host_path(core, context, path)?;
    if context.system.filesystem().exists(&path).await || is_storage_root(&path) {
        Ok(0)
    } else {
        set_errno(core, context, ENOENT)
    }
}

pub async fn unlink(core: &mut ArmCore, context: &AndroidContext, path: &str) -> Result<u32> {
    let path = host_path(core, context, path)?;
    if context.system.filesystem().remove(&path).await {
        Ok(0)
    } else {
        set_errno(core, context, ENOENT)
    }
}

pub async fn rename(core: &mut ArmCore, context: &AndroidContext, from: &str, to: &str) -> Result<u32> {
    let (from, to) = (host_path(core, context, from)?, host_path(core, context, to)?);
    let filesystem = context.system.filesystem();
    let Some(size) = filesystem.size(&from).await else {
        return set_errno(core, context, ENOENT);
    };
    let mut data = vec![0; size];
    filesystem.read(&from, 0, size, &mut data).await;
    filesystem.truncate(&to, 0).await;
    filesystem.write(&to, 0, &data).await;
    filesystem.remove(&from).await;
    Ok(0)
}

pub fn chdir(core: &mut ArmCore, context: &AndroidContext, path: &str) -> Result<u32> {
    let mut bytes = String::from(path).into_bytes();
    bytes.push(0);
    let ptr = Allocator::alloc(core, bytes.len() as u32)?;
    core.write_bytes(ptr, &bytes)?;
    context.update_globals(core, |x| x.cwd = ptr)?;
    Ok(0)
}

fn is_storage_root(path: &str) -> bool {
    matches!(path.trim_end_matches('/'), "files" | "sdcard")
}
