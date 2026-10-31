use alloc::{collections::BTreeMap, format, string::String, vec, vec::Vec};

use elf::{ElfBytes, abi, endian::AnyEndian};

use wie_core_arm::ArmCore;
use wie_util::{Result, WieError, read_generic, write_generic};

const R_ARM_ABS32: u32 = 2;
const R_ARM_GLOB_DAT: u32 = 21;
const R_ARM_JUMP_SLOT: u32 = 22;
const R_ARM_RELATIVE: u32 = 23;

pub struct SharedLibrary {
    pub exports: BTreeMap<String, u32>,
    pub init_array: Vec<u32>,
}

/// Maps an ARM `ET_DYN` image at `base` and applies its dynamic relocations.
/// Undefined symbols are resolved through `resolve_import`.
pub fn load_shared_library(
    core: &mut ArmCore,
    data: &[u8],
    base: u32,
    mut resolve_import: impl FnMut(&mut ArmCore, &str) -> Result<u32>,
) -> Result<SharedLibrary> {
    let elf = ElfBytes::<AnyEndian>::minimal_parse(data).map_err(|x| WieError::FatalError(format!("Failed to parse ELF: {x}")))?;

    if elf.ehdr.e_machine != abi::EM_ARM || elf.ehdr.e_type != abi::ET_DYN {
        return Err(WieError::FatalError("Not an ARM shared library".into()));
    }

    let segments = elf
        .segments()
        .ok_or_else(|| WieError::FatalError("ELF is missing program headers".into()))?;
    let loads = segments.iter().filter(|x| x.p_type == abi::PT_LOAD).collect::<Vec<_>>();
    let image_size = loads.iter().map(|x| x.p_vaddr + x.p_memsz).max().unwrap_or(0) as usize;

    let mut image = vec![0u8; image_size];
    for segment in &loads {
        let file = &data[segment.p_offset as usize..(segment.p_offset + segment.p_filesz) as usize];
        image[segment.p_vaddr as usize..segment.p_vaddr as usize + file.len()].copy_from_slice(file);
    }

    // Only the PLT and text are code; compiling data as instructions in both instruction sets
    // made the ahead-of-time module several times larger than needed.
    let (headers, names) = elf
        .section_headers_with_strtab()
        .map_err(|x| WieError::FatalError(format!("Failed to read section headers: {x}")))?;
    let code = headers.zip(names).and_then(|(headers, names)| {
        let ranges = headers
            .iter()
            .filter(|x| matches!(names.get(x.sh_name as usize), Ok(".plt" | ".text")))
            .map(|x| x.sh_addr as u32..(x.sh_addr + x.sh_size) as u32)
            .collect::<Vec<_>>();
        Some(ranges.iter().map(|x| x.start).min()?..ranges.iter().map(|x| x.end).max()?)
    });
    match code {
        // Android armeabi libraries built by the NDK of that era are ARM-only.
        Some(code) if !has_thumb_functions(&elf)? => {
            core.load_with_code(&image, base, image_size, base + code.start..base + code.end, Some(false))?
        }
        _ => core.load(&image, base, image_size)?,
    }

    tracing::debug!("Loaded shared library at {base:#x}, size {image_size:#x}");

    let (symbols, strings) = elf
        .dynamic_symbol_table()
        .map_err(|x| WieError::FatalError(format!("Failed to read dynamic symbols: {x}")))?
        .ok_or_else(|| WieError::FatalError("ELF is missing dynamic symbols".into()))?;

    let mut exports = BTreeMap::new();
    let mut symbol_addresses = Vec::with_capacity(symbols.len());
    for symbol in symbols.iter() {
        let name = strings
            .get(symbol.st_name as usize)
            .map_err(|x| WieError::FatalError(format!("Invalid symbol name: {x}")))?;

        let address = if symbol.st_shndx == abi::SHN_UNDEF {
            if name.is_empty() { 0 } else { resolve_import(core, name)? }
        } else {
            let address = base + symbol.st_value as u32;
            exports.insert(name.into(), address);
            address
        };
        symbol_addresses.push(address);
    }

    let (section_headers, _) = elf
        .section_headers_with_strtab()
        .map_err(|x| WieError::FatalError(format!("Failed to read section headers: {x}")))?;
    let section_headers = section_headers.ok_or_else(|| WieError::FatalError("ELF is missing section headers".into()))?;

    for section in section_headers.iter().filter(|x| x.sh_type == abi::SHT_REL) {
        let relocations = elf
            .section_data_as_rels(&section)
            .map_err(|x| WieError::FatalError(format!("Failed to read relocations: {x}")))?;

        for relocation in relocations {
            let target = base + relocation.r_offset as u32;
            let symbol = symbol_addresses[relocation.r_sym as usize];

            let value = match relocation.r_type {
                R_ARM_RELATIVE => read_generic::<u32, _>(core, target)?.wrapping_add(base),
                R_ARM_ABS32 => read_generic::<u32, _>(core, target)?.wrapping_add(symbol),
                R_ARM_GLOB_DAT | R_ARM_JUMP_SLOT => symbol,
                r_type => return Err(WieError::Unimplemented(format!("Unsupported relocation type {r_type}"))),
            };
            write_generic(core, target, value)?;
        }
    }

    let mut init_array = Vec::new();
    let (section_headers, string_table) = elf
        .section_headers_with_strtab()
        .map_err(|x| WieError::FatalError(format!("Failed to read section headers: {x}")))?;
    if let (Some(section_headers), Some(string_table)) = (section_headers, string_table) {
        for section in section_headers.iter() {
            if string_table.get(section.sh_name as usize).ok() == Some(".init_array") {
                for offset in (0..section.sh_size as u32).step_by(4) {
                    let function: u32 = read_generic(core, base + section.sh_addr as u32 + offset)?;
                    if function != 0 && function != u32::MAX {
                        init_array.push(function);
                    }
                }
            }
        }
    }

    Ok(SharedLibrary { exports, init_array })
}

/// Thumb function symbols have their low address bit set.
fn has_thumb_functions(elf: &ElfBytes<AnyEndian>) -> Result<bool> {
    let Some((symbols, _)) = elf
        .dynamic_symbol_table()
        .map_err(|x| WieError::FatalError(format!("Failed to read dynamic symbols: {x}")))?
    else {
        return Ok(false);
    };
    Ok(symbols
        .iter()
        .any(|x| x.st_symtype() == abi::STT_FUNC && x.st_shndx != abi::SHN_UNDEF && x.st_value & 1 != 0))
}
