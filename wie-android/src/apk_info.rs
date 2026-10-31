//! Reads the launcher label and icon from an APK's binary manifest and resource table.

use alloc::{collections::BTreeMap, string::String, vec::Vec};

const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_TABLE_TYPE: u16 = 0x0002;
const RES_XML_TYPE: u16 = 0x0003;
const RES_XML_START_ELEMENT_TYPE: u16 = 0x0102;
const RES_XML_RESOURCE_MAP_TYPE: u16 = 0x0180;
const RES_TABLE_PACKAGE_TYPE: u16 = 0x0200;
const RES_TABLE_TYPE_TYPE: u16 = 0x0201;

const ATTR_LABEL: u32 = 0x0101_0001;
const ATTR_ICON: u32 = 0x0101_0002;

const TYPE_REFERENCE: u8 = 0x01;
const TYPE_STRING: u8 = 0x03;

const UTF8_FLAG: u32 = 0x100;
const TYPE_FLAG_SPARSE: u8 = 0x01;
const ENTRY_FLAG_COMPLEX: u16 = 0x0001;
const ENTRY_FLAG_COMPACT: u16 = 0x0008;
const NO_ENTRY: u32 = 0xffff_ffff;

fn u16_at(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(offset..offset + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(offset..offset + 4)?.try_into().ok()?))
}

/// Iterates `(type, start, header size, size)` of the chunks in `data[start..end]`.
fn chunks(data: &[u8], mut start: usize, end: usize) -> impl Iterator<Item = (u16, usize, usize, usize)> + '_ {
    core::iter::from_fn(move || {
        if start + 8 > end.min(data.len()) {
            return None;
        }
        let (kind, header, size) = (u16_at(data, start)?, u16_at(data, start + 2)? as usize, u32_at(data, start + 4)? as usize);
        if size < 8 {
            return None;
        }
        let chunk = (kind, start, header, size);
        start += size;
        Some(chunk)
    })
}

fn string_pool(data: &[u8], start: usize) -> Option<Vec<String>> {
    let count = u32_at(data, start + 8)? as usize;
    let utf8 = u32_at(data, start + 16)? & UTF8_FLAG != 0;
    let strings_start = start + u32_at(data, start + 20)? as usize;
    let header = u16_at(data, start + 2)? as usize;

    (0..count)
        .map(|index| {
            let offset = strings_start + u32_at(data, start + header + index * 4)? as usize;
            if utf8 {
                // UTF-16 length then UTF-8 length, each one or two bytes.
                let skip = |at: usize| if data.get(at)? & 0x80 != 0 { Some(at + 2) } else { Some(at + 1) };
                let length_at = skip(offset)?;
                let first = *data.get(length_at)? as usize;
                let (length, text) = if first & 0x80 != 0 {
                    (((first & 0x7f) << 8) | *data.get(length_at + 1)? as usize, length_at + 2)
                } else {
                    (first, length_at + 1)
                };
                Some(String::from_utf8_lossy(data.get(text..text + length)?).into_owned())
            } else {
                let first = u16_at(data, offset)? as usize;
                let (length, text) = if first & 0x8000 != 0 {
                    (((first & 0x7fff) << 16) | u16_at(data, offset + 2)? as usize, offset + 4)
                } else {
                    (first, offset + 2)
                };
                let units = (0..length).map(|i| u16_at(data, text + i * 2)).collect::<Option<Vec<_>>>()?;
                Some(String::from_utf16_lossy(&units))
            }
        })
        .collect()
}

/// A manifest attribute: a resource reference or a literal string.
enum Value {
    Reference(u32),
    Literal(String),
}

/// Reads `android:label` and `android:icon` of the `<application>` element.
fn application_attributes(manifest: &[u8]) -> Option<(Option<Value>, Option<Value>)> {
    if u16_at(manifest, 0)? != RES_XML_TYPE {
        return None;
    }
    let header = u16_at(manifest, 2)? as usize;
    let mut strings = Vec::new();
    let mut resource_map = Vec::new();

    for (kind, start, chunk_header, size) in chunks(manifest, header, manifest.len()) {
        match kind {
            RES_STRING_POOL_TYPE => strings = string_pool(manifest, start)?,
            RES_XML_RESOURCE_MAP_TYPE => {
                resource_map = (start + chunk_header..start + size)
                    .step_by(4)
                    .map(|offset| u32_at(manifest, offset))
                    .collect::<Option<Vec<_>>>()?;
            }
            RES_XML_START_ELEMENT_TYPE => {
                let element = start + chunk_header;
                let name = u32_at(manifest, element + 4)? as usize;
                if strings.get(name).map(String::as_str) != Some("application") {
                    continue;
                }
                let attributes_start = element + u16_at(manifest, element + 8)? as usize;
                let attribute_size = u16_at(manifest, element + 10)? as usize;
                let count = u16_at(manifest, element + 12)? as usize;

                let (mut label, mut icon) = (None, None);
                for index in 0..count {
                    let attribute = attributes_start + index * attribute_size;
                    let name = u32_at(manifest, attribute + 4)? as usize;
                    let raw = u32_at(manifest, attribute + 8)?;
                    let data_type = *manifest.get(attribute + 15)?;
                    let data = u32_at(manifest, attribute + 16)?;
                    let value = match data_type {
                        TYPE_REFERENCE => Value::Reference(data),
                        TYPE_STRING => Value::Literal(strings.get(data as usize).or_else(|| strings.get(raw as usize))?.clone()),
                        _ => continue,
                    };
                    match resource_map.get(name).copied() {
                        Some(ATTR_LABEL) => label = Some(value),
                        Some(ATTR_ICON) => icon = Some(value),
                        _ => {}
                    }
                }
                return Some((label, icon));
            }
            _ => {}
        }
    }
    None
}

/// Every string value of `id` in the resource table, with its configuration's language and density.
fn resource_strings(table: &[u8], id: u32) -> Option<Vec<([u8; 2], u16, String)>> {
    if u16_at(table, 0)? != RES_TABLE_TYPE {
        return None;
    }
    let (package_id, type_id, entry_id) = ((id >> 24) as u8, ((id >> 16) & 0xff) as u8, id & 0xffff);
    let mut values = Vec::new();
    let mut global = Vec::new();

    for (kind, start, _, size) in chunks(table, u16_at(table, 2)? as usize, table.len()) {
        match kind {
            RES_STRING_POOL_TYPE => global = string_pool(table, start)?,
            RES_TABLE_PACKAGE_TYPE if u32_at(table, start + 8)? as u8 == package_id => {
                let package_header = u16_at(table, start + 2)? as usize;
                for (kind, type_start, type_header, _) in chunks(table, start + package_header, start + size) {
                    if kind != RES_TABLE_TYPE_TYPE || *table.get(type_start + 8)? != type_id {
                        continue;
                    }
                    let flags = *table.get(type_start + 9)?;
                    let entry_count = u32_at(table, type_start + 12)?;
                    let entries_start = type_start + u32_at(table, type_start + 16)? as usize;
                    let config = type_start + 20;
                    let language = [*table.get(config + 8)?, *table.get(config + 9)?];
                    let density = u16_at(table, config + 14)?;

                    let offset = if flags & TYPE_FLAG_SPARSE != 0 {
                        (0..entry_count as usize).find_map(|index| {
                            let at = type_start + type_header + index * 4;
                            (u16_at(table, at)? as u32 == entry_id).then(|| u16_at(table, at + 2).map(|x| x as u32 * 4))?
                        })
                    } else if entry_id < entry_count {
                        u32_at(table, type_start + type_header + entry_id as usize * 4).filter(|x| *x != NO_ENTRY)
                    } else {
                        None
                    };
                    let Some(offset) = offset else { continue };

                    let entry = entries_start + offset as usize;
                    let entry_flags = u16_at(table, entry + 2)?;
                    let (data_type, data) = if entry_flags & ENTRY_FLAG_COMPACT != 0 {
                        ((entry_flags >> 8) as u8, u32_at(table, entry + 4)?)
                    } else if entry_flags & ENTRY_FLAG_COMPLEX != 0 {
                        continue;
                    } else {
                        let value = entry + u16_at(table, entry)? as usize;
                        (*table.get(value + 3)?, u32_at(table, value + 4)?)
                    };
                    if data_type == TYPE_STRING
                        && let Some(text) = global.get(data as usize)
                    {
                        values.push((language, density, text.clone()));
                    }
                }
            }
            _ => {}
        }
    }
    Some(values)
}

pub fn label(files: &BTreeMap<String, Vec<u8>>) -> Option<String> {
    let (label, _) = application_attributes(files.get("AndroidManifest.xml")?)?;
    let label = match label? {
        Value::Literal(text) => text,
        Value::Reference(id) => {
            let mut values = resource_strings(files.get("resources.arsc")?, id)?;
            // Prefer Korean, then the default configuration.
            values.sort_by_key(|(language, _, _)| match language {
                b"ko" => 0,
                [0, 0] => 1,
                _ => 2,
            });
            values.into_iter().next()?.2
        }
    };
    let label = label.trim();
    (!label.is_empty()).then(|| label.into())
}

pub fn icon(files: &BTreeMap<String, Vec<u8>>) -> Option<Vec<u8>> {
    let (_, icon) = application_attributes(files.get("AndroidManifest.xml")?)?;
    let paths = match icon? {
        Value::Literal(path) => alloc::vec![(0, path)],
        Value::Reference(id) => resource_strings(files.get("resources.arsc")?, id)?
            .into_iter()
            .map(|(_, density, path)| (density, path))
            .collect(),
    };
    // The densest bitmap; vector and adaptive icons are XML and cannot be shown.
    paths
        .into_iter()
        .filter(|(density, path)| *density < 0xfffe && (path.ends_with(".png") || path.ends_with(".jpg")))
        .max_by_key(|(density, _)| *density)
        .and_then(|(_, path)| files.get(&path).cloned())
}
