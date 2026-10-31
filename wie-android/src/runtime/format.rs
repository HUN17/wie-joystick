use alloc::{format, string::String, vec::Vec};

use wie_core_arm::ArmCore;
use wie_util::{Result, read_generic, read_null_terminated_string_bytes};

/// Reads AAPCS variadic arguments either from call parameters or from a `va_list` pointer.
pub enum ArgReader {
    Params(usize),
    Memory(u32),
}

impl ArgReader {
    pub fn next_u32(&mut self, core: &ArmCore) -> Result<u32> {
        match self {
            Self::Params(index) => {
                let value = core.read_param(*index)?;
                *index += 1;
                Ok(value)
            }
            Self::Memory(address) => {
                let value = read_generic(core, *address)?;
                *address += 4;
                Ok(value)
            }
        }
    }

    pub fn next_u64(&mut self, core: &ArmCore) -> Result<u64> {
        match self {
            Self::Params(index) => *index = index.next_multiple_of(2),
            Self::Memory(address) => *address = address.next_multiple_of(8),
        }
        let low = self.next_u32(core)? as u64;
        let high = self.next_u32(core)? as u64;
        Ok(low | (high << 32))
    }
}

pub fn format(core: &ArmCore, format: &[u8], args: &mut ArgReader) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut chars = format.iter().copied().peekable();

    while let Some(char) = chars.next() {
        if char != b'%' {
            result.push(char);
            continue;
        }

        let mut left = false;
        let mut zero = false;
        let mut plus = false;
        while let Some(&flag) = chars.peek() {
            match flag {
                b'-' => left = true,
                b'0' => zero = true,
                b'+' => plus = true,
                b' ' | b'#' => {}
                _ => break,
            }
            chars.next();
        }

        let mut width = 0usize;
        if chars.peek() == Some(&b'*') {
            chars.next();
            width = args.next_u32(core)? as usize;
        }
        while let Some(&digit @ b'0'..=b'9') = chars.peek() {
            width = width * 10 + (digit - b'0') as usize;
            chars.next();
        }

        let mut precision = None;
        if chars.peek() == Some(&b'.') {
            chars.next();
            let mut value = 0usize;
            if chars.peek() == Some(&b'*') {
                chars.next();
                value = args.next_u32(core)? as usize;
            }
            while let Some(&digit @ b'0'..=b'9') = chars.peek() {
                value = value * 10 + (digit - b'0') as usize;
                chars.next();
            }
            precision = Some(value);
        }

        let mut long_long = false;
        while let Some(&length) = chars.peek() {
            match length {
                b'l' if long_long => {}
                b'l' => {
                    if chars.clone().nth(1) == Some(b'l') {
                        long_long = true;
                    }
                }
                b'h' | b'z' | b'j' | b't' => {}
                _ => break,
            }
            chars.next();
        }

        let Some(conversion) = chars.next() else { break };
        let text: Vec<u8> = match conversion {
            b'%' => {
                result.push(b'%');
                continue;
            }
            b'd' | b'i' => {
                let value = if long_long {
                    args.next_u64(core)? as i64
                } else {
                    args.next_u32(core)? as i32 as i64
                };
                let sign = if plus && value >= 0 { "+" } else { "" };
                format!("{sign}{value}").into_bytes()
            }
            b'u' | b'x' | b'X' | b'o' => {
                let value = if long_long { args.next_u64(core)? } else { args.next_u32(core)? as u64 };
                match conversion {
                    b'u' => format!("{value}"),
                    b'x' => format!("{value:x}"),
                    b'X' => format!("{value:X}"),
                    _ => format!("{value:o}"),
                }
                .into_bytes()
            }
            b'p' => format!("{:#x}", args.next_u32(core)?).into_bytes(),
            b'c' => [args.next_u32(core)? as u8].into(),
            b's' => {
                let address = args.next_u32(core)?;
                let mut bytes = if address == 0 {
                    b"(null)".into()
                } else {
                    read_null_terminated_string_bytes(core, address)?
                };
                if let Some(precision) = precision {
                    bytes.truncate(precision);
                }
                bytes
            }
            b'f' | b'F' | b'e' | b'g' | b'G' => {
                let value = f64::from_bits(args.next_u64(core)?);
                let text: String = match (conversion, precision) {
                    (b'f' | b'F', precision) => format!("{value:.*}", precision.unwrap_or(6)),
                    (b'e', precision) => format!("{value:.*e}", precision.unwrap_or(6)),
                    _ => format!("{value}"),
                };
                text.into_bytes()
            }
            other => {
                tracing::warn!("Unsupported format conversion %{}", other as char);
                continue;
            }
        };

        let padding = width.saturating_sub(text.len());
        if left {
            result.extend_from_slice(&text);
            result.extend(core::iter::repeat_n(b' ', padding));
        } else if zero && matches!(conversion, b'd' | b'i' | b'u' | b'x' | b'X' | b'o') {
            let (sign, digits) = match text.first() {
                Some(b'-' | b'+') => text.split_at(1),
                _ => (&[][..], &text[..]),
            };
            result.extend_from_slice(sign);
            result.extend(core::iter::repeat_n(b'0', padding));
            result.extend_from_slice(digits);
        } else {
            result.extend(core::iter::repeat_n(b' ', padding));
            result.extend_from_slice(&text);
        }
    }

    Ok(result)
}
