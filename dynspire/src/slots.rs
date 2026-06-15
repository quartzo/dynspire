// === SlotWriter / SlotReader ===

use crate::ffi::EnumDescriptor;

pub trait SlotEnumDescriptor: Sized {
    fn descriptor() -> &'static EnumDescriptor;
}

pub struct SlotWriter {
    inline: [u64; MAX_IN_SLOTS],
    inline_len: usize,
    heap: Option<Vec<u64>>,
}

pub const MAX_IN_SLOTS: usize = 16;

impl SlotWriter {
    pub fn new() -> Self {
        Self { inline: [0; MAX_IN_SLOTS], inline_len: 0, heap: None }
    }

    pub fn write_u64(&mut self, val: u64) {
        if let Some(h) = &mut self.heap {
            h.push(val);
        } else if self.inline_len < MAX_IN_SLOTS {
            self.inline[self.inline_len] = val;
            self.inline_len += 1;
        } else {
            let mut h = self.inline.to_vec();
            h.push(val);
            self.heap = Some(h);
        }
    }

    pub fn as_slice(&self) -> &[u64] {
        match &self.heap {
            Some(h) => h,
            None => &self.inline[..self.inline_len],
        }
    }

    pub fn len(&self) -> usize {
        match &self.heap {
            Some(h) => h.len(),
            None => self.inline_len,
        }
    }
}

impl Default for SlotWriter {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SlotReader<'a> {
    slots: &'a [u64],
    pos: usize,
}

impl<'a> SlotReader<'a> {
    pub fn new(slots: &'a [u64]) -> Self {
        Self { slots, pos: 0 }
    }

    pub fn read_u64(&mut self) -> u64 {
        let val = self.slots[self.pos];
        self.pos += 1;
        val
    }
}

// === Input traits (caller -> spier) ===

/// Encodes a value into slots for passing as a method parameter (caller → spier).
///
/// You should not implement this manually. Built-in impls cover all conventional
/// types (scalars, `&[u8]`, `&str`, `String`, `Vec<T: Clone>`, `&mut Vec<u8>`,
/// tuples, `Option<T>`, `Result<T,E>`). For custom types, use `#[slot_struct]`
/// or `#[slot_enum]` — they generate all four slot traits automatically.
pub trait SlotEncode {
    fn encode(&self, w: &mut SlotWriter);
}

impl SlotEncode for () {
    fn encode(&self, _w: &mut SlotWriter) {}
}

impl<A: SlotEncode> SlotEncode for (A,) {
    fn encode(&self, w: &mut SlotWriter) {
        self.0.encode(w);
    }
}

impl<A: SlotEncode, B: SlotEncode> SlotEncode for (A, B) {
    fn encode(&self, w: &mut SlotWriter) {
        self.0.encode(w);
        self.1.encode(w);
    }
}

impl<A: SlotEncode, B: SlotEncode, C: SlotEncode> SlotEncode for (A, B, C) {
    fn encode(&self, w: &mut SlotWriter) {
        self.0.encode(w);
        self.1.encode(w);
        self.2.encode(w);
    }
}

impl<A: SlotEncode, B: SlotEncode, C: SlotEncode, D: SlotEncode> SlotEncode for (A, B, C, D) {
    fn encode(&self, w: &mut SlotWriter) {
        self.0.encode(w);
        self.1.encode(w);
        self.2.encode(w);
        self.3.encode(w);
    }
}

impl<A: SlotEncode, B: SlotEncode, C: SlotEncode, D: SlotEncode, E: SlotEncode> SlotEncode for (A, B, C, D, E) {
    fn encode(&self, w: &mut SlotWriter) {
        self.0.encode(w);
        self.1.encode(w);
        self.2.encode(w);
        self.3.encode(w);
        self.4.encode(w);
    }
}

/// Decodes a value from slots on the spier side (input parameter).
///
/// You should not implement this manually. For custom types, use `#[slot_struct]`
/// or `#[slot_enum]` to generate all four slot traits automatically.
///
/// # Safety
///
/// The caller must ensure the slots were produced by a matching [`SlotEncode`]
/// impl in the same process.
pub trait SlotDecode<'a>: Sized {
    unsafe fn decode(r: &mut SlotReader<'a>) -> Self;
}

pub unsafe fn decode_param<'a, T: SlotDecode<'a>>(r: &mut SlotReader<'a>) -> T {
    T::decode(r)
}

// === Output traits (spier -> caller) ===

/// Encodes a return value into slots (spier → caller).
///
/// You should not implement this manually. Built-in impls cover all conventional
/// types (scalars, `String`, `Vec<u8>`, `Vec<T>`, tuples, `Option<T>`,
/// `Result<T,E>`). For custom types, use `#[slot_struct]` or `#[slot_enum]` —
/// they generate all four slot traits automatically.
pub trait SlotReturn: Sized {
    fn into_slots(self, w: &mut SlotWriter);
}

/// Decodes a return value from slots on the caller side.
///
/// You should not implement this manually. For custom types, use `#[slot_struct]`
/// or `#[slot_enum]` to generate all four slot traits automatically.
///
/// # Safety
///
/// The caller must ensure the slots were produced by a matching [`SlotReturn`]
/// impl in the same process.
pub trait SlotReceive: Sized {
    unsafe fn from_slots(r: &mut SlotReader) -> Self;
}

// === Scalar impls ===

macro_rules! impl_scalar {
    ($ty:ty, $conv:expr, $back:expr) => {
        impl SlotEncode for $ty {
            fn encode(&self, w: &mut SlotWriter) {
                w.write_u64(($conv)(self));
            }
        }
        impl<'a> SlotDecode<'a> for $ty {
            unsafe fn decode(r: &mut SlotReader<'a>) -> Self {
                ($back)(r.read_u64())
            }
        }
        impl SlotReturn for $ty {
            fn into_slots(self, w: &mut SlotWriter) {
                w.write_u64(($conv)(&self));
            }
        }
        impl SlotReceive for $ty {
            unsafe fn from_slots(r: &mut SlotReader) -> Self {
                ($back)(r.read_u64())
            }
        }
    };
}

impl_scalar!(u8, |v: &u8| *v as u64, |v: u64| v as u8);
impl_scalar!(u16, |v: &u16| *v as u64, |v: u64| v as u16);
impl_scalar!(u32, |v: &u32| *v as u64, |v: u64| v as u32);
impl_scalar!(u64, |v: &u64| *v, |v: u64| v);
impl_scalar!(i8, |v: &i8| *v as u64, |v: u64| v as i8);
impl_scalar!(i16, |v: &i16| *v as u64, |v: u64| v as i16);
impl_scalar!(i32, |v: &i32| *v as u64, |v: u64| v as i32);
impl_scalar!(i64, |v: &i64| *v as u64, |v: u64| v as i64);
impl_scalar!(bool, |v: &bool| *v as u64, |v: u64| v != 0);
impl_scalar!(f64, |v: &f64| v.to_bits(), |v: u64| f64::from_bits(v));

// [u8; 16] — 2 slots
impl SlotEncode for [u8; 16] {
    fn encode(&self, w: &mut SlotWriter) {
        w.write_u64(u64::from_le_bytes(self[0..8].try_into().unwrap()));
        w.write_u64(u64::from_le_bytes(self[8..16].try_into().unwrap()));
    }
}
impl<'a> SlotDecode<'a> for [u8; 16] {
    unsafe fn decode(r: &mut SlotReader<'a>) -> Self {
        let lo = r.read_u64().to_le_bytes();
        let hi = r.read_u64().to_le_bytes();
        let mut arr = [0u8; 16];
        arr[0..8].copy_from_slice(&lo);
        arr[8..16].copy_from_slice(&hi);
        arr
    }
}
impl SlotReturn for [u8; 16] {
    fn into_slots(self, w: &mut SlotWriter) {
        w.write_u64(u64::from_le_bytes(self[0..8].try_into().unwrap()));
        w.write_u64(u64::from_le_bytes(self[8..16].try_into().unwrap()));
    }
}
impl SlotReceive for [u8; 16] {
    unsafe fn from_slots(r: &mut SlotReader) -> Self {
        let lo = r.read_u64().to_le_bytes();
        let hi = r.read_u64().to_le_bytes();
        let mut arr = [0u8; 16];
        arr[0..8].copy_from_slice(&lo);
        arr[8..16].copy_from_slice(&hi);
        arr
    }
}

// () — 0 slots
impl SlotReturn for () {
    fn into_slots(self, _w: &mut SlotWriter) {}
}
impl SlotReceive for () {
    unsafe fn from_slots(_r: &mut SlotReader) -> Self {}
}

// === Borrow impls (input, zero-copy) ===

impl SlotEncode for &[u8] {
    fn encode(&self, w: &mut SlotWriter) {
        w.write_u64(self.as_ptr() as u64);
        w.write_u64(self.len() as u64);
    }
}
impl<'a> SlotDecode<'a> for &'a [u8] {
    unsafe fn decode(r: &mut SlotReader<'a>) -> Self {
        let ptr = r.read_u64() as *const u8;
        let len = r.read_u64() as usize;
        if ptr.is_null() || len == 0 {
            &[]
        } else {
            core::slice::from_raw_parts(ptr, len)
        }
    }
}

impl SlotEncode for &str {
    fn encode(&self, w: &mut SlotWriter) {
        w.write_u64(self.as_ptr() as u64);
        w.write_u64(self.len() as u64);
    }
}
impl<'a> SlotDecode<'a> for &'a str {
    unsafe fn decode(r: &mut SlotReader<'a>) -> Self {
        let ptr = r.read_u64() as *const u8;
        let len = r.read_u64() as usize;
        if ptr.is_null() || len == 0 {
            ""
        } else {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr, len))
        }
    }
}

// === &mut Vec<u8> out-param (caller-owned fill, 1 slot) ===

impl SlotEncode for &mut Vec<u8> {
    fn encode(&self, w: &mut SlotWriter) {
        let ptr = core::ptr::addr_of!(**self) as u64;
        w.write_u64(ptr);
    }
}
impl<'a> SlotDecode<'a> for &'a mut Vec<u8> {
    unsafe fn decode(r: &mut SlotReader<'a>) -> Self {
        let ptr = r.read_u64() as *mut Vec<u8>;
        &mut *ptr
    }
}

// === Owned input impls (caller-owned, borrowed as ptr+len) ===

impl SlotEncode for String {
    fn encode(&self, w: &mut SlotWriter) {
        w.write_u64(self.as_ptr() as u64);
        w.write_u64(self.len() as u64);
    }
}
impl<'a> SlotDecode<'a> for String {
    unsafe fn decode(r: &mut SlotReader<'a>) -> Self {
        let ptr = r.read_u64() as *const u8;
        let len = r.read_u64() as usize;
        if ptr.is_null() || len == 0 {
            String::new()
        } else {
            String::from_utf8_unchecked(core::slice::from_raw_parts(ptr, len).to_vec())
        }
    }
}

impl<T: Clone> SlotEncode for Vec<T> {
    fn encode(&self, w: &mut SlotWriter) {
        w.write_u64(self.as_ptr() as u64);
        w.write_u64(self.len() as u64);
    }
}
impl<'a, T: Clone> SlotDecode<'a> for Vec<T> {
    unsafe fn decode(r: &mut SlotReader<'a>) -> Self {
        let ptr = r.read_u64() as *const T;
        let len = r.read_u64() as usize;
        if ptr.is_null() || len == 0 {
            Vec::new()
        } else {
            core::slice::from_raw_parts(ptr, len).to_vec()
        }
    }
}

// === Owned impls (output, ownership transfer) ===

impl<T> SlotReturn for Vec<T> {
    fn into_slots(self, w: &mut SlotWriter) {
        if self.is_empty() {
            w.write_u64(0);
            w.write_u64(0);
            return;
        }
        let len = self.len();
        let boxed = self.into_boxed_slice();
        let ptr = boxed.as_ptr() as usize;
        core::mem::forget(boxed);
        w.write_u64(ptr as u64);
        w.write_u64(len as u64);
    }
}
impl<T> SlotReceive for Vec<T> {
    unsafe fn from_slots(r: &mut SlotReader) -> Self {
        let ptr = r.read_u64() as *mut T;
        let len = r.read_u64() as usize;
        if ptr.is_null() || len == 0 {
            return Vec::new();
        }
        let fat = core::ptr::slice_from_raw_parts_mut(ptr, len);
        Box::from_raw(fat).into_vec()
    }
}

impl SlotReturn for String {
    fn into_slots(self, w: &mut SlotWriter) {
        self.into_bytes().into_slots(w);
    }
}
impl SlotReceive for String {
    unsafe fn from_slots(r: &mut SlotReader) -> Self {
        let v: Vec<u8> = Vec::<u8>::from_slots(r);
        String::from_utf8_unchecked(v)
    }
}

impl<T: SlotReturn> SlotReturn for Option<T> {
    fn into_slots(self, w: &mut SlotWriter) {
        match self {
            Some(v) => {
                w.write_u64(1);
                v.into_slots(w);
            }
            None => {
                w.write_u64(0);
            }
        }
    }
}
impl<T: SlotReceive> SlotReceive for Option<T> {
    unsafe fn from_slots(r: &mut SlotReader) -> Self {
        if r.read_u64() == 0 {
            None
        } else {
            Some(T::from_slots(r))
        }
    }
}

impl<A: SlotReturn, B: SlotReturn> SlotReturn for (A, B) {
    fn into_slots(self, w: &mut SlotWriter) {
        self.0.into_slots(w);
        self.1.into_slots(w);
    }
}
impl<A: SlotReceive, B: SlotReceive> SlotReceive for (A, B) {
    unsafe fn from_slots(r: &mut SlotReader) -> Self {
        let a = A::from_slots(r);
        let b = B::from_slots(r);
        (a, b)
    }
}

// === Result<T, E> — unified tagged union ===

pub const fn max_slots(a: usize, b: usize) -> usize {
    if a > b { a } else { b }
}

impl<T: SlotReturn, E: SlotReturn> SlotReturn for Result<T, E> {
    fn into_slots(self, w: &mut SlotWriter) {
        match self {
            Ok(v) => {
                w.write_u64(0);
                v.into_slots(w);
            }
            Err(e) => {
                w.write_u64(1);
                e.into_slots(w);
            }
        }
    }
}

impl<T: SlotReceive, E: SlotReceive> SlotReceive for Result<T, E> {
    unsafe fn from_slots(r: &mut SlotReader) -> Self {
        if r.read_u64() == 0 {
            Ok(T::from_slots(r))
        } else {
            Err(E::from_slots(r))
        }
    }
}

/// Maximum out-slots the caller buffer can hold.
/// Current IDL methods need at most 5 slots (Result<Option<Vec<u8>>, String>).
pub const MAX_OUT_SLOTS: usize = 8;

/// Write a SlotReturn into a caller-provided raw out-slots buffer.
/// Returns 0 on success, 2 if the buffer is too small.
/// Used by macro-generated spier dispatch functions.
pub fn write_to_ffi<R: SlotReturn>(val: R, out_slots: *mut u64, out_capacity: usize) -> u8 {
    let mut w = SlotWriter::new();
    val.into_slots(&mut w);
    let n = w.len();
    if n > out_capacity {
        return 2;
    }
    if n > 0 {
        unsafe {
            std::ptr::copy_nonoverlapping(w.as_slice().as_ptr(), out_slots, n);
        }
    }
    0
}

/// Read a SlotReceive from response slots.
pub fn read_response<R: SlotReceive>(slots: &[u64]) -> R {
    let mut r = SlotReader::new(slots);
    unsafe { R::from_slots(&mut r) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_input<T>(val: &T) -> T
    where
        T: SlotEncode + for<'a> SlotDecode<'a>,
    {
        let mut w = SlotWriter::new();
        val.encode(&mut w);
        let mut r = SlotReader::new(w.as_slice());
        unsafe { T::decode(&mut r) }
    }

    fn roundtrip_output<T>(val: T) -> T
    where
        T: SlotReturn + SlotReceive,
    {
        let mut w = SlotWriter::new();
        val.into_slots(&mut w);
        let mut r = SlotReader::new(w.as_slice());
        unsafe { T::from_slots(&mut r) }
    }

    #[test]
    fn test_scalar_roundtrips() {
        assert_eq!(roundtrip_input(&42i8), 42i8);
        assert_eq!(roundtrip_input(&-1i8), -1i8);
        assert_eq!(roundtrip_input(&1000i16), 1000i16);
        assert_eq!(roundtrip_input(&-1i32), -1i32);
        assert_eq!(roundtrip_input(&-1i64), -1i64);
        assert_eq!(roundtrip_input(&255u8), 255u8);
        assert_eq!(roundtrip_input(&65535u16), 65535u16);

        assert_eq!(roundtrip_output(42i8), 42i8);
        assert_eq!(roundtrip_output(-1i64), -1i64);
        assert_eq!(roundtrip_output(255u8), 255u8);
    }

    #[test]
    fn test_string_input_roundtrip() {
        let s = String::from("hello world");
        assert_eq!(roundtrip_input(&s), s);

        let empty = String::new();
        assert_eq!(roundtrip_input(&empty), empty);
    }

    #[test]
    fn test_string_output_roundtrip() {
        let s = String::from("hello world");
        assert_eq!(roundtrip_output(s.clone()), s);
    }

    #[test]
    fn test_vec_u8_input_roundtrip() {
        let v = vec![1u8, 2, 3, 4, 5];
        assert_eq!(roundtrip_input(&v), v);

        let empty: Vec<u8> = vec![];
        assert_eq!(roundtrip_input(&empty), empty);
    }

    #[test]
    fn test_vec_u8_output_roundtrip() {
        let v = vec![1u8, 2, 3, 4, 5];
        assert_eq!(roundtrip_output(v.clone()), v);
    }

    #[test]
    fn test_vec_string_input_roundtrip() {
        let v = vec!["hello".to_string(), "world".to_string()];
        assert_eq!(roundtrip_input(&v), v);

        let empty: Vec<String> = vec![];
        assert_eq!(roundtrip_input(&empty), empty);
    }

    #[test]
    fn test_vec_string_output_roundtrip() {
        let v = vec!["hello".to_string(), "world".to_string()];
        assert_eq!(roundtrip_output(v.clone()), v);
    }

    #[test]
    fn test_vec_vec_u8_input_roundtrip() {
        let v = vec![vec![1u8, 2], vec![3, 4, 5], vec![]];
        assert_eq!(roundtrip_input(&v), v);
    }

    #[test]
    fn test_vec_vec_u8_output_roundtrip() {
        let v = vec![vec![1u8, 2], vec![3, 4, 5], vec![]];
        assert_eq!(roundtrip_output(v.clone()), v);
    }

    #[derive(Clone, PartialEq, Debug)]
    struct Point {
        x: i64,
        y: i64,
    }

    #[test]
    fn test_vec_struct_input_roundtrip() {
        let v = vec![Point { x: 1, y: 2 }, Point { x: -3, y: 0 }];
        assert_eq!(roundtrip_input(&v), v);
    }

    #[test]
    fn test_vec_struct_output_roundtrip() {
        let v = vec![Point { x: 1, y: 2 }, Point { x: -3, y: 0 }];
        assert_eq!(roundtrip_output(v.clone()), v);
    }

    #[test]
    fn test_vec_input_consumes_two_slots() {
        let v = vec![1u32, 2, 3, 4, 5];
        let mut w = SlotWriter::new();
        v.encode(&mut w);
        assert_eq!(w.len(), 2, "Vec<T> input must consume exactly 2 slots (ptr, len)");
    }
}
