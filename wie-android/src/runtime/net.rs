//! In-process stand-in for the game's network server.
//!
//! The Clet bridge talks to its server over plain BSD sockets through EA's "smart" relay framing.
//! The original servers are gone, so sockets connect to a local server instead: frames the game
//! writes are unwrapped and answered locally, and the answers come back through `select`/`read`.
//! Socket state lives in a guest-memory table referenced from the runtime globals.

mod server;

use alloc::{format, string::String, vec, vec::Vec};

use bytemuck::{Pod, Zeroable};

use wie_core_arm::{Allocator, ArmCore};
use wie_util::{ByteRead, ByteWrite, Result, read_generic, write_generic};

use super::AndroidContext;

/// Socket descriptors live above the file and VFS descriptors but below `FD_SETSIZE` (1024).
const FIRST_SOCKET_FD: u32 = 200;
pub const MAX_SOCKETS: u32 = 16;

const EBADF: u32 = 9;
const EAGAIN: u32 = 11;
const EMFILE: u32 = 24;
const ECONNRESET: u32 = 104;
const ECONNREFUSED: u32 = 111;
/// Port of the Clet game server (login, shop, storage). Other EA services, such as the DSP save
/// backup on 11536, are not emulated and refuse connections as they do now that they are gone.
const GAME_SERVER_PORT: u16 = 18186;
const MSG_PEEK: u32 = 2;
const F_GETFL: u32 = 3;
const O_RDWR: u32 = 2;
const FD_SET_SIZE: usize = 128;

/// Outgoing relay frames start with a header whose first word is the big-endian frame length.
const RELAY_HEADER_SIZE: usize = 0x6c;
/// Every game packet starts with its total length, an 8-byte tag, the category and the command.
pub const HEADER_SIZE: usize = 20;

/// Guest heap block holding queued bytes.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct GuestBuffer {
    ptr: u32,
    len: u32,
    capacity: u32,
}

/// One entry of the guest socket table; `open == 0` marks a free slot.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct SocketEntry {
    open: u32,
    /// Bytes the game wrote that do not yet form a whole packet.
    from_game: GuestBuffer,
    /// Bytes waiting for the game to `recv`.
    to_game: GuestBuffer,
}

impl GuestBuffer {
    fn read(&self, core: &ArmCore) -> Result<Vec<u8>> {
        let mut data = vec![0; self.len as usize];
        if self.len != 0 {
            core.read_bytes(self.ptr, &mut data)?;
        }
        Ok(data)
    }

    fn store(&mut self, core: &mut ArmCore, data: &[u8]) -> Result<()> {
        if data.len() as u32 > self.capacity {
            if self.ptr != 0 {
                Allocator::free(core, self.ptr, self.capacity)?;
            }
            self.capacity = (data.len() as u32).next_power_of_two().max(256);
            self.ptr = Allocator::alloc(core, self.capacity)?;
        }
        if !data.is_empty() {
            core.write_bytes(self.ptr, data)?;
        }
        self.len = data.len() as u32;
        Ok(())
    }

    fn release(&self, core: &mut ArmCore) -> Result<()> {
        if self.ptr != 0 {
            Allocator::free(core, self.ptr, self.capacity)?;
        }
        Ok(())
    }
}

pub fn is_socket(fd: u32) -> bool {
    (FIRST_SOCKET_FD..FIRST_SOCKET_FD + MAX_SOCKETS).contains(&fd)
}

fn set_errno(core: &mut ArmCore, context: &AndroidContext, errno: u32) -> Result<u32> {
    context.update_globals(core, |x| x.errno = errno)?;
    Ok(u32::MAX)
}

fn entry_address(core: &ArmCore, context: &AndroidContext, fd: u32) -> Result<u32> {
    Ok(context.globals(core)?.socket_table + (fd - FIRST_SOCKET_FD) * size_of::<SocketEntry>() as u32)
}

fn open_entry(core: &ArmCore, context: &AndroidContext, fd: u32) -> Result<Option<(u32, SocketEntry)>> {
    if !is_socket(fd) {
        return Ok(None);
    }
    let address = entry_address(core, context, fd)?;
    let entry: SocketEntry = read_generic(core, address)?;
    Ok((entry.open != 0).then_some((address, entry)))
}

pub fn socket(core: &mut ArmCore, context: &AndroidContext, domain: u32, kind: u32, protocol: u32) -> Result<u32> {
    tracing::debug!("socket({domain}, {kind}, {protocol})");
    for fd in FIRST_SOCKET_FD..FIRST_SOCKET_FD + MAX_SOCKETS {
        let address = entry_address(core, context, fd)?;
        let entry: SocketEntry = read_generic(core, address)?;
        if entry.open == 0 {
            write_generic(core, address, SocketEntry { open: 1, ..entry })?;
            return Ok(fd);
        }
    }
    set_errno(core, context, EMFILE)
}

/// Connections to the game server complete immediately; the server is in-process.
pub fn connect(core: &mut ArmCore, context: &AndroidContext, fd: u32, address: u32) -> Result<u32> {
    let [_, _, port_hi, port_lo, a, b, c, d]: [u8; 8] = read_generic(core, address)?;
    let port = u16::from_be_bytes([port_hi, port_lo]);
    tracing::debug!("connect({fd}, {a}.{b}.{c}.{d}:{port})");
    if open_entry(core, context, fd)?.is_none() {
        return set_errno(core, context, EBADF);
    }
    if port != GAME_SERVER_PORT {
        return set_errno(core, context, ECONNREFUSED);
    }
    Ok(0)
}

pub fn fcntl(fd: u32, command: u32, argument: u32) -> u32 {
    tracing::debug!("fcntl({fd}, {command}, {argument:#x})");
    if command == F_GETFL { O_RDWR } else { 0 }
}

/// `getsockopt(SO_ERROR)` and friends: report no pending error.
pub fn getsockopt(core: &mut ArmCore, fd: u32, level: u32, name: u32, value: u32, length: u32) -> Result<u32> {
    tracing::debug!("getsockopt({fd}, {level}, {name}, {value:#x}, {length:#x})");
    write_generic(core, value, 0u32)?;
    write_generic(core, length, 4u32)?;
    Ok(0)
}

pub async fn write(core: &mut ArmCore, context: &AndroidContext, fd: u32, buffer: u32, count: u32) -> Result<u32> {
    tracing::debug!("write({fd}, {buffer:#x}, {count})");
    let Some((address, mut entry)) = open_entry(core, context, fd)? else {
        return set_errno(core, context, EBADF);
    };
    let mut from_game = entry.from_game.read(core)?;
    let mut data = vec![0; count as usize];
    core.read_bytes(buffer, &mut data)?;
    from_game.extend_from_slice(&data);

    let mut to_game = entry.to_game.read(core)?;
    let mut refused = false;
    while let Some(frame) = take_frame(&mut from_game) {
        let packet = frame.get(RELAY_HEADER_SIZE..).unwrap_or_default();
        log_packet("game -> server", packet);
        let Some(replies) = server::answer(context, packet).await else {
            refused = true;
            break;
        };
        for reply in replies {
            log_packet("server -> game", &reply);
            // Incoming relay frames are the packet behind a big-endian length that counts itself.
            to_game.extend_from_slice(&(reply.len() as u32 + 4).to_be_bytes());
            to_game.extend_from_slice(&reply);
        }
    }

    entry.from_game.store(core, &from_game)?;
    entry.to_game.store(core, &to_game)?;
    write_generic(core, address, entry)?;
    // Requests the local server cannot answer fail the write, which every caller reports as a
    // network error instead of waiting for a reply forever.
    if refused {
        tracing::debug!("net: server refuses request on {fd}");
        return set_errno(core, context, ECONNRESET);
    }
    Ok(count)
}

pub fn recv(core: &mut ArmCore, context: &AndroidContext, fd: u32, buffer: u32, length: u32, flags: u32) -> Result<u32> {
    tracing::debug!("recv({fd}, {buffer:#x}, {length}, {flags:#x})");
    let Some((address, mut entry)) = open_entry(core, context, fd)? else {
        return set_errno(core, context, EBADF);
    };
    let mut to_game = entry.to_game.read(core)?;
    if to_game.is_empty() {
        return set_errno(core, context, EAGAIN);
    }
    let count = to_game.len().min(length as usize);
    core.write_bytes(buffer, &to_game[..count])?;
    if flags & MSG_PEEK == 0 {
        to_game.drain(..count);
        entry.to_game.store(core, &to_game)?;
        write_generic(core, address, entry)?;
    }
    Ok(count as u32)
}

/// Sockets are always writable and become readable once the server has queued a reply.
pub fn select(core: &mut ArmCore, context: &AndroidContext, nfds: u32, read_set: u32, write_set: u32, except_set: u32) -> Result<u32> {
    tracing::trace!("select({nfds}, {read_set:#x}, {write_set:#x}, {except_set:#x})");
    let mut ready = 0;
    for (set, reading) in [(read_set, true), (write_set, false)] {
        if set == 0 {
            continue;
        }
        let mut bits = [0u8; FD_SET_SIZE];
        core.read_bytes(set, &mut bits)?;
        for fd in 0..nfds.min(FD_SET_SIZE as u32 * 8) {
            let (byte, bit) = ((fd / 8) as usize, 1 << (fd % 8));
            if bits[byte] & bit == 0 {
                continue;
            }
            let is_ready = match open_entry(core, context, fd)? {
                Some((_, entry)) => !reading || entry.to_game.len != 0,
                None => false,
            };
            if is_ready {
                ready += 1;
            } else {
                bits[byte] &= !bit;
            }
        }
        core.write_bytes(set, &bits)?;
    }
    if except_set != 0 {
        core.write_bytes(except_set, &[0; FD_SET_SIZE])?;
    }
    Ok(ready)
}

pub fn close(core: &mut ArmCore, context: &AndroidContext, fd: u32) -> Result<u32> {
    tracing::debug!("close({fd})");
    let Some((address, entry)) = open_entry(core, context, fd)? else {
        return set_errno(core, context, EBADF);
    };
    entry.from_game.release(core)?;
    entry.to_game.release(core)?;
    write_generic(core, address, SocketEntry::zeroed())?;
    Ok(0)
}

fn take_frame(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buffer.len() < 4 {
        return None;
    }
    let length = (u32::from_be_bytes(buffer[..4].try_into().unwrap()) as usize).max(4);
    (buffer.len() >= length).then(|| buffer.drain(..length).collect())
}

fn log_packet(direction: &str, packet: &[u8]) {
    if packet.len() < HEADER_SIZE {
        tracing::debug!("net {direction}: short packet {}", hex(packet));
        return;
    }
    let category = u32::from_be_bytes(packet[12..16].try_into().unwrap());
    let command = u32::from_be_bytes(packet[16..20].try_into().unwrap());
    let tag = String::from_utf8_lossy(&packet[4..12]);
    tracing::debug!(
        "net {direction}: category {category} command {command} tag {tag:?} body {}",
        hex(&packet[HEADER_SIZE..])
    );
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02x}")).collect()
}
