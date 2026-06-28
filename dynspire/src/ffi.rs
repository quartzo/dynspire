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

/// Read element `idx` from a boxed slice of `Vec<u8>`-compatible objects
/// (covers `Vec<u8>` and `String` since they share layout).
/// Returns a `#[repr(C)]` view so foreign callers never touch Rust layout.
#[no_mangle]
pub extern "C" fn dynspire_vec_view_at(base: *const u8, idx: usize) -> VecView {
    if base.is_null() {
        return VecView { ptr: core::ptr::null(), len: 0 };
    }
    unsafe {
        let stride = core::mem::size_of::<Vec<u8>>();
        let elem = base.add(idx * stride) as *const Vec<u8>;
        let v = &*elem;
        VecView { ptr: v.as_ptr(), len: v.len() }
    }
}
