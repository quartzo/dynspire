pub const IDL_UNIT: u8 = 0;
pub const IDL_U8: u8 = 1;
pub const IDL_U32: u8 = 2;
pub const IDL_U64: u8 = 3;
pub const IDL_ARRAY: u8 = 4;
pub const IDL_SLICE: u8 = 5;
pub const IDL_STR: u8 = 6;
pub const IDL_VEC: u8 = 7;
pub const IDL_OPTION: u8 = 8;
pub const IDL_TUPLE: u8 = 9;
pub const IDL_STRING: u8 = 10;
pub const IDL_BOOL: u8 = 11;
pub const IDL_OUT_VEC: u8 = 12;
pub const IDL_ENUM: u8 = 13;
pub const IDL_STRUCT: u8 = 14;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IdlTypeNode {
    pub kind: u8,
    pub _pad: [u8; 3],
    pub size: u32,
    pub child0: i32,
    pub child1: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IdlParam {
    pub name: *const u8,
    pub name_len: usize,
    pub type_idx: u32,
}

unsafe impl Sync for IdlParam {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IdlMethod {
    pub name: *const u8,
    pub name_len: usize,
    pub params: *const IdlParam,
    pub param_count: usize,
    pub return_type_idx: u32,
    pub _pad: [u8; 4],
}

unsafe impl Sync for IdlMethod {}

pub type FreeFn = unsafe extern "C" fn(type_index: u32, slots: *const u64, slot_count: usize);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct StructDescriptor {
    pub name: *const u8,
    pub name_len: usize,
}

unsafe impl Sync for StructDescriptor {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DynSpireIdl {
    pub name: *const u8,
    pub name_len: usize,
    pub hash: u64,
    pub type_table: *const IdlTypeNode,
    pub type_count: usize,
    pub methods: *const IdlMethod,
    pub method_count: usize,
    pub enum_table: *const *const EnumDescriptor,
    pub enum_count: usize,
    pub struct_table: *const *const StructDescriptor,
    pub struct_count: usize,
    pub free_fn: FreeFn,
}

unsafe impl Sync for DynSpireIdl {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EnumVariantDesc {
    pub disc: u32,
    pub name: *const u8,
    pub name_len: usize,
    pub field_count: u32,
    pub field_type_offset: u32,
}

unsafe impl Sync for EnumVariantDesc {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EnumDescriptor {
    pub name: *const u8,
    pub name_len: usize,
    pub variant_count: usize,
    pub variants: *const EnumVariantDesc,
    pub type_table: *const IdlTypeNode,
    pub type_count: usize,
    pub field_types: *const u32,
    pub field_type_count: usize,
}

unsafe impl Sync for EnumDescriptor {}

pub fn new_uuid() -> [u8; 16] {
    *uuid::Uuid::new_v4().as_bytes()
}

pub fn uuid_to_hex(id: &[u8; 16]) -> String {
    uuid::Uuid::from_bytes(*id).simple().to_string()
}

pub fn uuid_from_hex(s: &str) -> Option<[u8; 16]> {
    uuid::Uuid::parse_str(s)
        .ok()
        .map(|u| *u.as_bytes())
}

// === Vec<u8> helpers for foreign (Python/ctypes) callers ===

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VecView {
    pub ptr: *const u8,
    pub len: usize,
}

#[no_mangle]
pub extern "C" fn dynspire_vec_create() -> *mut Vec<u8> {
    Box::into_raw(Box::new(Vec::new()))
}

#[no_mangle]
pub extern "C" fn dynspire_vec_view(v: *const Vec<u8>) -> VecView {
    let v = unsafe { &*v };
    VecView {
        ptr: v.as_ptr(),
        len: v.len(),
    }
}

#[no_mangle]
pub extern "C" fn dynspire_vec_free(v: *mut Vec<u8>) {
    unsafe { drop(Box::from_raw(v)) }
}

#[no_mangle]
pub extern "C" fn dynspire_vec_u8_sizeof() -> usize {
    std::mem::size_of::<Vec<u8>>()
}
