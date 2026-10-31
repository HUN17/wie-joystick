use alloc::{format, string::String, vec, vec::Vec};

use wie_core_arm::{Allocator, ArmCore, SvcId};
use wie_util::{ByteRead, ByteWrite, Result, WieError, read_generic, write_generic};

use super::{
    AndroidContext, SVC_CATEGORY_JAVA_VM, SVC_CATEGORY_JNI,
    format::ArgReader,
    java::{JavaValue, call_java_method, get_java_field},
    read_string,
};

const JNI_FUNCTION_COUNT: u32 = 233;
const JAVA_VM_FUNCTION_COUNT: u32 = 8;
const JNI_VERSION_1_6: u32 = 0x10006;

/// Guest object header: tag, class, length, element size; payload follows.
const OBJECT_HEADER_SIZE: u32 = 16;
const TAG_OBJECT: u32 = 1;
const TAG_STRING: u32 = 2;
const TAG_ARRAY: u32 = 3;
const TAG_CLASS: u32 = 4;

/// Element sizes of boolean, byte, char, short, int, long, float and double arrays.
const PRIMITIVE_SIZES: [u32; 8] = [1, 1, 2, 2, 4, 8, 4, 8];

/// Return kinds of the `Call<Type>Method` families, in JNI table order.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CallType {
    Object,
    Boolean,
    Byte,
    Char,
    Short,
    Int,
    Long,
    Float,
    Double,
    Void,
}

const CALL_TYPES: [CallType; 10] = [
    CallType::Object,
    CallType::Boolean,
    CallType::Byte,
    CallType::Char,
    CallType::Short,
    CallType::Int,
    CallType::Long,
    CallType::Float,
    CallType::Double,
    CallType::Void,
];

#[derive(Clone, Copy)]
pub struct JniEnv {
    ptr_env: u32,
}

impl JniEnv {
    pub async fn create(core: &mut ArmCore, context: &AndroidContext) -> Result<Self> {
        let ptr_table = Allocator::alloc(core, JNI_FUNCTION_COUNT * 4)?;
        for index in 0..JNI_FUNCTION_COUNT {
            let stub = core.make_svc_stub(SVC_CATEGORY_JNI, index)?;
            write_generic(core, ptr_table + index * 4, stub)?;
        }
        let ptr_env = Allocator::alloc(core, 4)?;
        write_generic(core, ptr_env, ptr_table)?;

        let ptr_vm_table = Allocator::alloc(core, JAVA_VM_FUNCTION_COUNT * 4)?;
        for index in 0..JAVA_VM_FUNCTION_COUNT {
            let stub = core.make_svc_stub(SVC_CATEGORY_JAVA_VM, index)?;
            write_generic(core, ptr_vm_table + index * 4, stub)?;
        }
        let ptr_vm = Allocator::alloc(core, 4)?;
        write_generic(core, ptr_vm, ptr_vm_table)?;

        context.update_globals(core, |x| {
            x.env = ptr_env;
            x.vm = ptr_vm;
        })?;

        Ok(Self { ptr_env })
    }

    pub fn ptr(&self) -> u32 {
        self.ptr_env
    }

    pub fn new_object(&self, core: &mut ArmCore, class_name: &str) -> Result<u32> {
        let class = new_class(core, class_name)?;
        alloc_object(core, TAG_OBJECT, class, 0, 0, &[])
    }

    pub fn new_string(&self, core: &mut ArmCore, value: &str) -> Result<u32> {
        new_string(core, value)
    }
}

fn alloc_object(core: &mut ArmCore, tag: u32, class: u32, length: u32, element_size: u32, payload: &[u8]) -> Result<u32> {
    let ptr = Allocator::alloc(core, OBJECT_HEADER_SIZE + payload.len() as u32)?;
    write_generic(core, ptr, [tag, class, length, element_size])?;
    core.write_bytes(ptr + OBJECT_HEADER_SIZE, payload)?;
    Ok(ptr)
}

fn null_terminated(value: &str) -> Vec<u8> {
    value.bytes().chain([0]).collect()
}

fn new_class(core: &mut ArmCore, name: &str) -> Result<u32> {
    alloc_object(core, TAG_CLASS, 0, name.len() as u32, 1, &null_terminated(name))
}

pub fn new_string(core: &mut ArmCore, value: &str) -> Result<u32> {
    alloc_object(core, TAG_STRING, 0, value.len() as u32, 1, &null_terminated(value))
}

pub fn new_array(core: &mut ArmCore, length: u32, element_size: u32) -> Result<u32> {
    alloc_object(core, TAG_ARRAY, 0, length, element_size, &vec![0; (length * element_size) as usize])
}

pub fn read_java_string(core: &ArmCore, object: u32) -> Result<String> {
    if object == 0 {
        return Ok(String::new());
    }
    read_string(core, object + OBJECT_HEADER_SIZE)
}

pub fn array_data(object: u32) -> u32 {
    object + OBJECT_HEADER_SIZE
}

pub fn array_length(core: &ArmCore, object: u32) -> Result<u32> {
    read_generic(core, object + 8)
}

fn class_name(core: &ArmCore, class: u32) -> Result<String> {
    if class == 0 {
        return Ok(String::new());
    }
    match read_generic::<u32, _>(core, class)? {
        TAG_CLASS => read_string(core, class + OBJECT_HEADER_SIZE),
        _ => {
            let class: u32 = read_generic(core, class + 4)?;
            if class == 0 { Ok(String::new()) } else { class_name(core, class) }
        }
    }
}

/// Method and field IDs point at `[name, signature, class]` cells in guest memory.
fn new_member_id(core: &mut ArmCore, class: u32, name: u32, signature: u32) -> Result<u32> {
    let ptr = Allocator::alloc(core, 12)?;
    write_generic(core, ptr, [name, signature, class])?;
    Ok(ptr)
}

struct MemberId {
    class: String,
    name: String,
    signature: String,
}

fn read_member_id(core: &ArmCore, id: u32) -> Result<MemberId> {
    let [name, signature, class]: [u32; 3] = read_generic(core, id)?;
    Ok(MemberId {
        class: class_name(core, class)?,
        name: read_string(core, name)?,
        signature: read_string(core, signature)?,
    })
}

/// Parameter kinds of a method descriptor, collapsing references to `L`.
fn parse_parameters(signature: &str) -> Vec<u8> {
    let mut result = Vec::new();
    let mut chars = signature.bytes().skip(1);
    while let Some(char) = chars.next() {
        match char {
            b')' => break,
            b'[' => {
                let mut next = chars.next();
                while next == Some(b'[') {
                    next = chars.next();
                }
                if next == Some(b'L') {
                    for c in chars.by_ref() {
                        if c == b';' {
                            break;
                        }
                    }
                }
                result.push(b'L');
            }
            b'L' => {
                for c in chars.by_ref() {
                    if c == b';' {
                        break;
                    }
                }
                result.push(b'L');
            }
            other => result.push(other),
        }
    }
    result
}

pub fn register_jni_svc_handler(core: &mut ArmCore, context: &AndroidContext) -> Result<()> {
    core.register_svc_handler(SVC_CATEGORY_JNI, handle_jni_svc, context)?;
    core.register_svc_handler(SVC_CATEGORY_JAVA_VM, handle_java_vm_svc, context)
}

fn ret(core: &mut ArmCore, lr: u32, values: &[u32]) -> Result<()> {
    core.write_return_value(values)?;
    core.set_next_pc(lr)
}

async fn handle_java_vm_svc(core: &mut ArmCore, context: &mut AndroidContext, id: SvcId) -> Result<()> {
    let (_, lr) = core.read_pc_lr()?;
    let env = context.globals(core)?.env;

    match id.0 {
        4 | 6 | 7 => {
            let ptr_env = core.read_param(1)?;
            tracing::debug!("JavaVM::{}({ptr_env:#x})", if id.0 == 6 { "GetEnv" } else { "AttachCurrentThread" });
            write_generic(core, ptr_env, env)?;
            ret(core, lr, &[0])
        }
        5 => {
            tracing::debug!("JavaVM::DetachCurrentThread()");
            ret(core, lr, &[0])
        }
        index => Err(WieError::Unimplemented(format!("JavaVM function {index}"))),
    }
}

async fn handle_jni_svc(core: &mut ArmCore, context: &mut AndroidContext, id: SvcId) -> Result<()> {
    let (_, lr) = core.read_pc_lr()?;
    let p = |core: &ArmCore, index: usize| core.read_param(index);

    match id.0 {
        4 => {
            tracing::debug!("GetVersion()");
            ret(core, lr, &[JNI_VERSION_1_6])
        }
        6 => {
            let name = read_string(core, p(core, 1)?)?;
            tracing::debug!("FindClass({name})");
            let class = new_class(core, &name)?;
            ret(core, lr, &[class])
        }
        15 | 228 => ret(core, lr, &[0]),
        16 | 17 => ret(core, lr, &[]),
        19 | 26 => ret(core, lr, &[0]),
        20 | 21 | 25 | 226 => {
            let object = p(core, 1)?;
            ret(core, lr, &[object])
        }
        22 | 23 | 227 => ret(core, lr, &[]),
        24 => {
            let same = p(core, 1)? == p(core, 2)?;
            ret(core, lr, &[same as u32])
        }
        27..=30 => {
            let class = p(core, 1)?;
            let name = class_name(core, class)?;
            tracing::debug!("NewObject({name})");
            let object = alloc_object(core, TAG_OBJECT, class, 0, 0, &[])?;
            ret(core, lr, &[object])
        }
        31 => {
            let object = p(core, 1)?;
            let class = match read_generic::<u32, _>(core, object)? {
                TAG_STRING => new_class(core, "java/lang/String")?,
                TAG_ARRAY => new_class(core, "[B")?,
                _ => read_generic(core, object + 4)?,
            };
            ret(core, lr, &[class])
        }
        32 => ret(core, lr, &[1]),
        33 | 94 | 113 | 144 => {
            let (class, name, signature) = (p(core, 1)?, p(core, 2)?, p(core, 3)?);
            tracing::debug!(
                "Get{}ID({}, {}, {})",
                if id.0 == 33 || id.0 == 113 { "Method" } else { "Field" },
                class_name(core, class)?,
                read_string(core, name)?,
                read_string(core, signature)?
            );
            let member = new_member_id(core, class, name, signature)?;
            ret(core, lr, &[member])
        }
        index @ (34..=93 | 114..=143) => {
            let (base, first_arg) = match index {
                34..=63 => (34, 3),
                64..=93 => (64, 4),
                _ => (114, 3),
            };
            let call_type = CALL_TYPES[((index - base) / 3) as usize];
            let variant = (index - base) % 3;
            let this = p(core, 1)?;
            let method = read_member_id(core, p(core, first_arg - 1)?)?;

            let kinds = parse_parameters(&method.signature);
            let mut args = Vec::with_capacity(kinds.len());
            let mut reader = match variant {
                0 => Some(ArgReader::Params(first_arg)),
                1 => Some(ArgReader::Memory(p(core, first_arg)?)),
                _ => None,
            };
            let jvalues = p(core, first_arg)?;
            for (position, kind) in kinds.iter().enumerate() {
                let value = match &mut reader {
                    Some(reader) => match kind {
                        b'J' | b'D' => reader.next_u64(core)?,
                        // Variadic floats are promoted to double.
                        b'F' => (f64::from_bits(reader.next_u64(core)?) as f32).to_bits() as u64,
                        _ => reader.next_u32(core)? as u64,
                    },
                    None => {
                        let address = jvalues + position as u32 * 8;
                        match kind {
                            b'J' | b'D' => read_generic(core, address)?,
                            _ => read_generic::<u32, _>(core, address)? as u64,
                        }
                    }
                };
                args.push(JavaValue(value));
            }

            let result = call_java_method(core, context, &method.class, &method.name, &method.signature, this, &args).await?;
            match call_type {
                CallType::Void => ret(core, lr, &[]),
                CallType::Long | CallType::Double => ret(core, lr, &[result.0 as u32, (result.0 >> 32) as u32]),
                _ => ret(core, lr, &[result.0 as u32]),
            }
        }
        index @ (95..=103 | 145..=153) => {
            let object = p(core, 1)?;
            let field = read_member_id(core, p(core, 2)?)?;
            let value = get_java_field(core, context, &field.class, &field.name, object)?;
            let call_type = CALL_TYPES[(index - if index >= 145 { 145 } else { 95 }) as usize];
            match call_type {
                CallType::Long | CallType::Double => ret(core, lr, &[value.0 as u32, (value.0 >> 32) as u32]),
                _ => ret(core, lr, &[value.0 as u32]),
            }
        }
        104..=112 | 154..=162 => {
            let field = read_member_id(core, p(core, 2)?)?;
            tracing::warn!("stub Set{}Field({}.{})", if id.0 >= 154 { "Static" } else { "" }, field.class, field.name);
            ret(core, lr, &[])
        }
        164 | 168 => {
            let string = p(core, 1)?;
            let value = read_java_string(core, string)?;
            let length = if id.0 == 164 { value.chars().count() } else { value.len() };
            ret(core, lr, &[length as u32])
        }
        167 => {
            let value = read_string(core, p(core, 1)?)?;
            tracing::debug!("NewStringUTF({value})");
            let string = new_string(core, &value)?;
            ret(core, lr, &[string])
        }
        169 => {
            let (string, is_copy) = (p(core, 1)?, p(core, 2)?);
            if is_copy != 0 {
                core.write_bytes(is_copy, &[0])?;
            }
            ret(core, lr, &[string + OBJECT_HEADER_SIZE])
        }
        166 | 170 | 191..=198 | 223 | 225 => ret(core, lr, &[]),
        171 => {
            let length = array_length(core, p(core, 1)?)?;
            ret(core, lr, &[length])
        }
        172 => {
            let length = p(core, 1)?;
            let array = new_array(core, length, 4)?;
            ret(core, lr, &[array])
        }
        173 => {
            let (array, index) = (p(core, 1)?, p(core, 2)?);
            let value: u32 = read_generic(core, array_data(array) + index * 4)?;
            ret(core, lr, &[value])
        }
        174 => {
            let (array, index, value) = (p(core, 1)?, p(core, 2)?, p(core, 3)?);
            write_generic(core, array_data(array) + index * 4, value)?;
            ret(core, lr, &[])
        }
        index @ 175..=182 => {
            let length = p(core, 1)?;
            tracing::debug!("NewArray({length}, kind {})", index - 175);
            let array = new_array(core, length, PRIMITIVE_SIZES[(index - 175) as usize])?;
            ret(core, lr, &[array])
        }
        183..=190 | 222 => {
            let (array, is_copy) = (p(core, 1)?, p(core, 2)?);
            if is_copy != 0 {
                core.write_bytes(is_copy, &[0])?;
            }
            ret(core, lr, &[array_data(array)])
        }
        index @ (199..=214) => {
            let kind = ((index - 199) % 8) as usize;
            let (array, start, length, buffer) = (p(core, 1)?, p(core, 2)?, p(core, 3)?, p(core, 4)?);
            let size = PRIMITIVE_SIZES[kind];
            let element = array_data(array) + start * size;
            let (from, to) = if index < 207 { (element, buffer) } else { (buffer, element) };
            let mut data = vec![0; (length * size) as usize];
            core.read_bytes(from, &mut data)?;
            core.write_bytes(to, &data)?;
            ret(core, lr, &[])
        }
        215 | 217 | 218 => ret(core, lr, &[0]),
        219 => {
            let ptr_vm = p(core, 1)?;
            let vm = context.globals(core)?.vm;
            write_generic(core, ptr_vm, vm)?;
            ret(core, lr, &[0])
        }
        index => Err(WieError::Unimplemented(format!("JNI function {index}"))),
    }
}
