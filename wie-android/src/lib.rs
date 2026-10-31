#![no_std]
extern crate alloc;

mod apk_info;
mod emulator;
mod runtime;

pub use emulator::AndroidEmulator;
