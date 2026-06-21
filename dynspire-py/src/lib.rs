//! PyO3 engine that replaces the pure-Python ctypes adapter.
//!
//! The decode runs in Rust (this crate lives in-process with the spier), so
//! owned `Vec`/`String` reconstruction uses `Box::from_raw` natively — no
//! 24-byte stride arithmetic, no `dynspire_free` for data returns. The engine
//! owns the Rust value, converts to Python objects, and drops it normally.
//!
//! `dynspire_free` is kept ONLY for opaque `#[slot_struct]` returns whose
//! concrete Rust type the engine cannot name to drop. Those wrap in
//! [`OpaqueHandle`], which frees itself via the spier's `free_fn` on GC.

use std::ffi::c_void;
use std::sync::Arc;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyBool, PyBytes, PyList, PyString, PyTuple};

use dynspire_core::ffi::{
    DynSpireIdl, FreeFn, IdlMethod, IDL_ARRAY, IDL_BOOL, IDL_ENUM, IDL_OPTION, IDL_OUT_VEC,
    IDL_SLICE, IDL_STRING, IDL_STRUCT, IDL_STR, IDL_TUPLE, IDL_U32, IDL_U64, IDL_U8, IDL_UNIT,
    IDL_VEC,
};
use dynspire_core::slots::{SlotReader, SlotWriter, MAX_OUT_SLOTS};
use pyo3::{Py, PyRef};

// === C function pointer types (the spier's exported ABI) ===

type FnCreate = unsafe extern "C" fn(*const u8, usize) -> *mut c_void;
type FnDestroy = unsafe extern "C" fn(*mut c_void);
type FnDispatch = unsafe extern "C" fn(*mut c_void, *const u64, usize, *mut u64, usize) -> u8;
type FnSchema = unsafe extern "C" fn() -> *const DynSpireIdl;
type FnSpierName = unsafe extern "C" fn() -> *const u8;
type FnVecCreate = unsafe extern "C" fn() -> *mut c_void;
type FnVecFree = unsafe extern "C" fn(*mut c_void);

// === Schema (parsed once from the spier's DynSpireIdl into owned Rust) ===

#[derive(Clone, Copy)]
struct TypeNode {
    kind: u8,
    _size: u32,
    child0: i32,
    child1: i32,
}

struct ParamInfo {
    name: String,
    type_idx: u32,
}

struct MethodInfo {
    name: String,
    params: Vec<ParamInfo>,
    return_type_idx: u32,
}

struct SchemaData {
    name: String,
    hash: u64,
    types: Vec<TypeNode>,
    methods: Vec<MethodInfo>,
    struct_names: Vec<String>,
    free_fn: Option<FreeFn>,
    /// Keeps the spier `.so` mapped. Cloned into every handle/opaque value so
    /// that their function pointers stay valid for as long as they live — the
    /// `.so` is only `dlclose`d once the last reference (including pending
    /// `dynspire_free` drops) is gone.
    #[allow(dead_code)]
    lib: Arc<libloading::Library>,
    // enums omitted from this skeleton — port read_schema_enums when needed.
}

unsafe fn read_name(ptr: *const u8, len: usize) -> String {
    if ptr.is_null() || len == 0 {
        return String::new();
    }
    let bytes = std::slice::from_raw_parts(ptr, len);
    // The macro stores names null-terminated with name_len excluding the NUL.
    String::from_utf8_lossy(bytes).into_owned()
}

/// Reads a NUL-terminated C string (as returned by `dynspire_spier_name`).
unsafe fn read_c_string(ptr: *const u8) -> String {
    if ptr.is_null() {
        return String::new();
    }
    std::ffi::CStr::from_ptr(ptr as *const std::os::raw::c_char)
        .to_string_lossy()
        .into_owned()
}

unsafe fn parse_method(m: &IdlMethod) -> MethodInfo {
    let params: Vec<ParamInfo> = if m.param_count > 0 {
        std::slice::from_raw_parts(m.params, m.param_count)
            .iter()
            .map(|p| ParamInfo {
                name: read_name(p.name, p.name_len),
                type_idx: p.type_idx,
            })
            .collect()
    } else {
        Vec::new()
    };
    MethodInfo {
        name: read_name(m.name, m.name_len),
        params,
        return_type_idx: m.return_type_idx,
    }
}

unsafe fn parse_schema(raw: &DynSpireIdl, name: String, lib: Arc<libloading::Library>) -> Arc<SchemaData> {
    let types: Vec<TypeNode> = std::slice::from_raw_parts(raw.type_table, raw.type_count)
        .iter()
        .map(|t| TypeNode {
            kind: t.kind,
            _size: t.size,
            child0: t.child0,
            child1: t.child1,
        })
        .collect();

    let methods: Vec<MethodInfo> = std::slice::from_raw_parts(raw.methods, raw.method_count)
        .iter()
        .map(|m| unsafe { parse_method(m) })
        .collect();

    let struct_names: Vec<String> = (0..raw.struct_count)
        .map(|i| {
            let pp = *raw.struct_table.add(i);
            if pp.is_null() {
                String::new()
            } else {
                let d = &*pp;
                read_name(d.name, d.name_len)
            }
        })
        .collect();

    let free_fn = if raw.free_fn as usize == 0 {
        None
    } else {
        Some(raw.free_fn)
    };

    Arc::new(SchemaData {
        name,
        hash: raw.hash,
        types,
        methods,
        struct_names,
        free_fn,
        lib,
    })
}

// === Symbol loading ===

fn load_sym<T: Copy>(lib: &libloading::Library, name: &[u8]) -> PyResult<T> {
    unsafe {
        lib.get(name)
            .map(|s: libloading::Symbol<'_, T>| *s)
            .map_err(|e| {
                PyRuntimeError::new_err(format!("symbol {}: {e}", String::from_utf8_lossy(name)))
            })
    }
}

// === SpierLib ===

/// A loaded spier `.so`. Cheap to clone (inner state is `Arc`-shared).
///
/// Created via [`load_spier`]. Build handles with [`SpierLib::create_handle`].
#[pyclass(name = "SpierLib")]
struct SpierLib {
    data: Arc<SchemaData>,
    create: FnCreate,
    destroy: FnDestroy,
    dispatch: Arc<[FnDispatch]>,
    #[allow(dead_code)]
    vec_create: FnVecCreate,
    #[allow(dead_code)]
    vec_free: FnVecFree,
}

#[pymethods]
impl SpierLib {
    /// Returns the spier's reflected schema (methods, types, structs).
    fn schema(&self) -> SpierSchema {
        SpierSchema {
            data: self.data.clone(),
        }
    }

    /// Creates a client handle, optionally passing a `dict[str, str]` config.
    #[pyo3(signature = (config=None))]
    fn create_handle(&self, config: Option<&Bound<'_, PyAny>>) -> PyResult<SpierHandle> {
        let buf = serialize_config(config)?;
        let handle = unsafe { (self.create)(buf.as_ptr(), buf.len()) };
        if handle.is_null() {
            return Err(PyRuntimeError::new_err("spier create returned null"));
        }
        Ok(SpierHandle {
            data: self.data.clone(),
            dispatch: self.dispatch.clone(),
            destroy: self.destroy,
            handle,
        })
    }

    fn __repr__(&self) -> String {
        format!("<SpierLib {} hash=0x{:016x}>", self.data.name, self.data.hash)
    }
}

fn serialize_config(config: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<u8>> {
    let mut map = std::collections::HashMap::new();
    if let Some(c) = config {
        if !c.is_none() {
            map = c.extract()?;
        }
    }
    Ok(dynspire_core::serialize_kvmap(&map))
}

/// Loads a spier `.so` by name and reflects its schema.
///
/// Discovery follows the same priority as the ctypes adapter:
/// `lib_dir=` parameter → `DYNSPIRE_LIB_DIR` → bare `lib{name}.so`.
#[pyfunction]
#[pyo3(signature = (name, lib_dir=None))]
fn load_spier(name: &str, lib_dir: Option<&str>) -> PyResult<SpierLib> {
    let so_name = format!("lib{name}.so");
    let path = match lib_dir {
        Some(d) => std::path::PathBuf::from(d).join(&so_name),
        None => match std::env::var("DYNSPIRE_LIB_DIR") {
            Ok(d) => std::path::PathBuf::from(d).join(&so_name),
            Err(_) => std::path::PathBuf::from(&so_name),
        },
    };
    // `lib` is wrapped in an `Arc` once: the schema (and every handle/opaque
    // value cloned from it) shares this reference, so the `.so` is only
    // `dlclose`d once the last of them — and their pending drops — is gone.
    let lib = Arc::new(unsafe {
        libloading::Library::new(&path)
            .map_err(|e| PyRuntimeError::new_err(format!("dlopen {}: {e}", path.display())))?
    });

    let fn_schema: FnSchema = load_sym(&lib, b"dynspire_idl_schema\0")?;
    let raw = unsafe { fn_schema() };
    if raw.is_null() {
        return Err(PyRuntimeError::new_err("dynspire_idl_schema returned null"));
    }

    // The dedicated name symbol carries the real spier name; the IDL descriptor
    // leaves it empty by macro design. Fall back to the descriptor name.
    let name = match load_sym::<FnSpierName>(&lib, b"dynspire_spier_name\0") {
        Ok(f) => unsafe { read_c_string(f()) },
        Err(_) => unsafe { read_name((*raw).name, (*raw).name_len) },
    };

    let create: FnCreate = load_sym(&lib, b"dynspire_create\0")?;
    let destroy: FnDestroy = load_sym(&lib, b"dynspire_destroy\0")?;

    // Out-vec helpers (used by call_with_outs, resolved from the spier .so).
    let vec_create: FnVecCreate = load_sym(&lib, b"dynspire_vec_create\0")?;
    let vec_free: FnVecFree = load_sym(&lib, b"dynspire_vec_free\0")?;

    let data = unsafe { parse_schema(&*raw, name, lib.clone()) };

    let mut dispatch: Vec<FnDispatch> = Vec::with_capacity(data.methods.len());
    for m in data.methods.iter() {
        let sym = format!("dynspire_dispatch_{}\0", m.name);
        dispatch.push(load_sym(&lib, sym.as_bytes())?);
    }

    Ok(SpierLib {
        data,
        create,
        destroy,
        dispatch: Arc::from(dispatch),
        vec_create,
        vec_free,
    })
}

// === SpierSchema ===

#[pyclass(name = "SpierSchema")]
struct SpierSchema {
    data: Arc<SchemaData>,
}

impl SpierSchema {
    fn type_str(&self, idx: u32) -> String {
        let types = &self.data.types;
        if (idx as usize) >= types.len() {
            return "?".into();
        }
        let t = &types[idx as usize];
        let name = kind_name(t.kind);
        if t.kind == IDL_STRUCT && t.child0 >= 0 {
            let s = self
                .data
                .struct_names
                .get(t.child0 as usize)
                .cloned()
                .unwrap_or_else(|| "?".into());
            return format!("Struct<{s}>");
        }
        let mut parts = Vec::new();
        if t.child0 >= 0 {
            parts.push(self.type_str(t.child0 as u32));
        }
        if t.child1 >= 0 {
            parts.push(self.type_str(t.child1 as u32));
        }
        if parts.is_empty() {
            name.into()
        } else {
            format!("{name}<{}>", parts.join(", "))
        }
    }
}

#[pymethods]
impl SpierSchema {
    #[getter]
    fn name(&self) -> &str {
        &self.data.name
    }

    #[getter]
    fn hash(&self) -> u64 {
        self.data.hash
    }

    /// Method names in declaration order.
    #[getter]
    fn methods(&self) -> Vec<String> {
        self.data.methods.iter().map(|m| m.name.clone()).collect()
    }

    fn method_sig(&self, name: &str) -> PyResult<String> {
        let m = self
            .data
            .methods
            .iter()
            .find(|m| m.name == name)
            .ok_or_else(|| PyValueError::new_err(format!("unknown method: {name}")))?;
        let params = m
            .params
            .iter()
            .map(|p| format!("{}: {}", p.name, self.type_str(p.type_idx)))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(format!(
            "{}({params}) -> Result<{}, String>",
            m.name,
            self.type_str(m.return_type_idx)
        ))
    }

    fn __repr__(&self) -> String {
        format!("<SpierSchema {}>", self.data.name)
    }
}

fn kind_name(k: u8) -> &'static str {
    match k {
        IDL_UNIT => "Unit",
        IDL_U8 => "U8",
        IDL_U32 => "U32",
        IDL_U64 => "U64",
        IDL_ARRAY => "Array",
        IDL_SLICE => "Slice",
        IDL_STR => "Str",
        IDL_VEC => "Vec",
        IDL_OPTION => "Option",
        IDL_TUPLE => "Tuple",
        IDL_STRING => "String",
        IDL_BOOL => "Bool",
        IDL_OUT_VEC => "OutVec",
        IDL_ENUM => "Enum",
        IDL_STRUCT => "Struct",
        _ => "?",
    }
}

// === SpierHandle ===

/// Runtime handle for calling a spier. `Drop` calls `dynspire_destroy`.
#[pyclass(name = "SpierHandle")]
struct SpierHandle {
    data: Arc<SchemaData>,
    dispatch: Arc<[FnDispatch]>,
    destroy: FnDestroy,
    handle: *mut c_void,
}

unsafe impl Send for SpierHandle {}
unsafe impl Sync for SpierHandle {}

impl SpierHandle {
    fn find_method(&self, name: &str) -> PyResult<(usize, &MethodInfo)> {
        self.data
            .methods
            .iter()
            .enumerate()
            .find(|(_, m)| m.name == name)
            .ok_or_else(|| PyValueError::new_err(format!("unknown method: {name}")))
    }

    fn free_handle(&mut self) {
        if !self.handle.is_null() {
            unsafe { (self.destroy)(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

#[pymethods]
impl SpierHandle {
    /// Calls a spier method by name with positional args, returning the
    /// decoded result. The return type is inferred from the schema.
    #[pyo3(signature = (method_name, *args))]
    fn call<'py>(
        &self,
        py: Python<'py>,
        method_name: &str,
        args: &Bound<'py, PyTuple>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let (idx, method) = self.find_method(method_name)?;
        if method.params.len() != args.len() {
            return Err(PyValueError::new_err(format!(
                "{} expects {} args, got {}",
                method_name,
                method.params.len(),
                args.len()
            )));
        }
        for p in &method.params {
            if self.data.types[p.type_idx as usize].kind == IDL_OUT_VEC {
                return Err(PyValueError::new_err(format!(
                    "{method_name} has out-vec params; use call_with_outs()"
                )));
            }
        }

        // Encode. Input borrows alias the Python objects held by `args`, which
        // stay alive for the duration of this synchronous call.
        let mut w = SlotWriter::new();
        let mut keepalive: Vec<Box<dyn std::any::Any + Send>> = Vec::new();
        for (p, arg) in method.params.iter().zip(args.iter()) {
            encode_value(&mut w, p.type_idx, &self.data, &arg, &mut keepalive)?;
        }

        // Dispatch. GIL is held: input borrows into Python memory are safe.
        let in_slots = w.as_slice();
        let mut out = [0u64; MAX_OUT_SLOTS];
        let ret = unsafe {
            (self.dispatch[idx])(
                self.handle,
                in_slots.as_ptr(),
                in_slots.len(),
                out.as_mut_ptr(),
                MAX_OUT_SLOTS,
            )
        };
        drop(keepalive);
        if ret != 0 {
            return Err(PyRuntimeError::new_err(format!(
                "dispatch transport error (code {ret})"
            )));
        }

        // Decode Result<T, String>: tag 0 = Ok(payload), tag 1 = Err(String).
        let mut r = SlotReader::new(&out);
        let tag = r.read_u64();
        if tag == 1 {
            let err = reconstruct_owned_string(&mut r);
            return Err(PyRuntimeError::new_err(format!("spier error: {err}")));
        }
        decode_value(&mut r, method.return_type_idx, &self.data, py)
    }

    /// Out-vec variant. TODO: mirrors `call` but resolves `&mut Vec<u8>` params
    /// via the spier's `dynspire_vec_*` helpers. Not yet implemented.
    #[pyo3(signature = (method_name, *args))]
    #[allow(unused_variables)]
    fn call_with_outs<'py>(
        &self,
        py: Python<'py>,
        method_name: &str,
        args: &Bound<'py, PyTuple>,
    ) -> PyResult<Py<PyAny>> {
        Err(PyRuntimeError::new_err(
            "call_with_outs() not yet implemented in the PyO3 engine",
        ))
    }

    fn destroy(&mut self) {
        self.free_handle();
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __exit__(
        &mut self,
        _exc_type: &Bound<'_, PyAny>,
        _exc_val: &Bound<'_, PyAny>,
        _exc_tb: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        self.free_handle();
        Ok(())
    }

    fn __repr__(&self) -> String {
        format!("<SpierHandle>")
    }
}

impl Drop for SpierHandle {
    fn drop(&mut self) {
        self.free_handle();
    }
}

// === OpaqueHandle: a #[slot_struct] return the engine can't drop by type ===

/// Wraps an opaque spier-owned value (a `#[slot_struct]` return). The engine
/// cannot reconstruct its concrete Rust type, so it holds the boxed pointer and
/// releases it through the spier's `dynspire_free` on garbage collection.
///
/// The boxed pointer is also the value passed back as an input parameter (a
/// struct param is encoded as `&self as *const Self`, which is this pointer).
///
/// Holds a clone of the schema (and thus the `Arc<Library>`) so the spier
/// `.so` stays mapped until *after* this drop has run `dynspire_free`.
#[pyclass(name = "OpaqueHandle")]
struct OpaqueHandle {
    free_fn: Option<FreeFn>,
    type_idx: u32,
    ptr: u64,
    #[allow(dead_code)]
    data: Arc<SchemaData>,
}

#[pymethods]
impl OpaqueHandle {
    #[getter]
    fn handle(&self) -> u64 {
        self.ptr
    }

    fn __int__(&self) -> u64 {
        self.ptr
    }

    fn __bool__(&self) -> bool {
        self.ptr != 0
    }

    fn __repr__(&self) -> String {
        format!("<OpaqueHandle type_idx={} ptr=0x{:x}>", self.type_idx, self.ptr)
    }
}

impl Drop for OpaqueHandle {
    fn drop(&mut self) {
        if self.ptr == 0 {
            return;
        }
        if let Some(ff) = self.free_fn {
            // dynspire_free expects Result framing — [tag, <payload slots>] —
            // reading the tag first, then reconstructing the payload by type.
            // This handle only exists for Ok returns, so the tag is always 0;
            // the payload for a slot_struct is the boxed pointer.
            let slots = [0u64, self.ptr];
            unsafe { ff(self.type_idx, slots.as_ptr(), slots.len()) };
        }
    }
}

// === Dynamic encoder (Python arg -> slots) ===

fn encode_value<'py>(
    w: &mut SlotWriter,
    type_idx: u32,
    data: &SchemaData,
    arg: &Bound<'py, PyAny>,
    keepalive: &mut Vec<Box<dyn std::any::Any + Send>>,
) -> PyResult<()> {
    let t = &data.types[type_idx as usize];
    match t.kind {
        IDL_UNIT => Ok(()),
        IDL_BOOL => {
            w.write_u64(if arg.is_truthy()? { 1 } else { 0 });
            Ok(())
        }
        // IDL_U32 collapses i16/u16/i32/u32/f32; follow Python's value type.
        IDL_U32 => {
            if let Ok(f) = arg.extract::<f32>() {
                w.write_u64(f.to_bits() as u64);
            } else {
                w.write_u64(arg.extract::<u32>()? as u64);
            }
            Ok(())
        }
        // IDL_U64 collapses u64/i64/isize/usize/f64; follow Python's value type.
        IDL_U64 => {
            if let Ok(f) = arg.extract::<f64>() {
                w.write_u64(f.to_bits());
            } else {
                w.write_u64(arg.extract::<u64>()?);
            }
            Ok(())
        }
        IDL_U8 => {
            w.write_u64(arg.extract::<u32>()? as u64 & 0xFF);
            Ok(())
        }
        IDL_STR | IDL_STRING => {
            let s: &str = arg.extract()?;
            w.write_u64(s.as_ptr() as u64);
            w.write_u64(s.len() as u64);
            Ok(())
        }
        IDL_SLICE => {
            let child = &data.types[t.child0 as usize];
            if child.kind == IDL_U8 {
                let b: &[u8] = arg.extract()?;
                w.write_u64(b.as_ptr() as u64);
                w.write_u64(b.len() as u64);
                Ok(())
            } else {
                Err(PyValueError::new_err("only &[u8] slices supported"))
            }
        }
        IDL_VEC => {
            let child = &data.types[t.child0 as usize];
            match child.kind {
                IDL_U8 => {
                    let b: &[u8] = arg.extract()?;
                    w.write_u64(b.as_ptr() as u64);
                    w.write_u64(b.len() as u64);
                    Ok(())
                }
                IDL_VEC if data.types[child.child0 as usize].kind == IDL_U8 => {
                    encode_vec_vec_u8_input(w, arg, keepalive)
                }
                _ => Err(PyValueError::new_err(format!(
                    "unsupported Vec element kind {}",
                    child.kind
                ))),
            }
        }
        IDL_STRUCT => {
            // Pass-through of an opaque handle (or a raw int) returned earlier.
            let ptr = if let Ok(h) = arg.extract::<PyRef<'_, OpaqueHandle>>() {
                h.ptr
            } else {
                arg.extract::<u64>()?
            };
            w.write_u64(ptr);
            Ok(())
        }
        // TODO: IDL_ARRAY, IDL_OPTION, IDL_TUPLE, IDL_ENUM inputs.
        _ => Err(PyValueError::new_err(format!(
            "unsupported input type kind {}",
            t.kind
        ))),
    }
}

/// Encodes a `Vec<Vec<u8>>` input. The engine builds an owned array of
/// `Vec<u8>` structs in Rust and hands `(ptr, len)`; the spier's
/// `SlotDecode for Vec<T>` clones it. The array is kept alive across dispatch
/// via `keepalive`, then reclaimed — no leak.
fn encode_vec_vec_u8_input(
    w: &mut SlotWriter,
    arg: &Bound<'_, PyAny>,
    keepalive: &mut Vec<Box<dyn std::any::Any + Send>>,
) -> PyResult<()> {
    let mut owned: Vec<Vec<u8>> = Vec::new();
    for item in arg.try_iter()? {
        owned.push(item?.extract::<Vec<u8>>()?);
    }
    let len = owned.len();
    let boxed: Box<[Vec<u8>]> = owned.into_boxed_slice();
    let ptr = boxed.as_ptr() as u64;
    keepalive.push(Box::new(boxed));
    w.write_u64(ptr);
    w.write_u64(len as u64);
    Ok(())
}

// === Dynamic decoder (slots -> Python value) ===

fn decode_value<'py>(
    r: &mut SlotReader,
    type_idx: u32,
    data: &Arc<SchemaData>,
    py: Python<'py>,
) -> PyResult<Bound<'py, PyAny>> {
    let t = &data.types[type_idx as usize];
    match t.kind {
        IDL_UNIT => Ok(py.None().into_bound(py)),
        IDL_BOOL => Ok(PyBool::new(py, r.read_u64() != 0).to_owned().into_any()),
        IDL_U8 => Ok((r.read_u64() & 0xFF)
            .into_pyobject(py)?
            .into_any()),
        IDL_U32 | IDL_U64 => Ok(r.read_u64().into_pyobject(py)?.into_any()),
        IDL_STRING => {
            let s = reconstruct_owned_string(r);
            Ok(PyString::new(py, &s).into_any())
        }
        IDL_VEC => {
            let child = &data.types[t.child0 as usize];
            if child.kind == IDL_U8 {
                let bytes = reconstruct_owned_bytes(r);
                Ok(PyBytes::new(py, &bytes).into_any())
            } else if child.kind == IDL_VEC && data.types[child.child0 as usize].kind == IDL_U8 {
                let outer = reconstruct_owned_vec_vec_u8(r);
                let list = PyList::empty(py);
                for inner in &outer {
                    list.append(PyBytes::new(py, inner))?;
                }
                Ok(list.into_any())
            } else {
                // TODO: Vec<T> of scalars — reconstruct by element stride.
                Err(PyValueError::new_err(format!(
                    "unsupported Vec element kind {}",
                    child.kind
                )))
            }
        }
        IDL_OPTION => {
            if r.read_u64() == 0 {
                Ok(py.None().into_bound(py))
            } else {
                decode_value(r, t.child0 as u32, data, py)
            }
        }
        IDL_TUPLE => {
            let a = if t.child0 >= 0 {
                Some(decode_value(r, t.child0 as u32, data, py)?)
            } else {
                None
            };
            let b = if t.child1 >= 0 {
                Some(decode_value(r, t.child1 as u32, data, py)?)
            } else {
                None
            };
            match (a, b) {
                (Some(a), Some(b)) => Ok(PyTuple::new(py, [a, b])?.into_any()),
                (Some(a), None) => Ok(a.into_any()),
                _ => Ok(py.None().into_bound(py)),
            }
        }
        IDL_STRUCT => {
            // Opaque: engine can't name the type, so wrap the boxed pointer and
            // free it via dynspire_free on GC.
            let obj = OpaqueHandle {
                free_fn: data.free_fn,
                type_idx,
                ptr: r.read_u64(),
                data: data.clone(),
            };
            Ok(Bound::new(py, obj)?.into_any())
        }
        // TODO: IDL_ARRAY, IDL_ENUM returns.
        _ => Err(PyValueError::new_err(format!(
            "unsupported return type kind {}",
            t.kind
        ))),
    }
}

// === Owned reconstruction (mirrors dynspire::slots::SlotReceive) ===

fn reconstruct_owned_bytes(r: &mut SlotReader) -> Vec<u8> {
    let ptr = r.read_u64() as *mut u8;
    let len = r.read_u64() as usize;
    if ptr.is_null() || len == 0 {
        Vec::new()
    } else {
        unsafe {
            let fat = std::ptr::slice_from_raw_parts_mut(ptr, len);
            Box::from_raw(fat).into_vec()
        }
    }
}

fn reconstruct_owned_string(r: &mut SlotReader) -> String {
    // The spier's String output is its bytes handed as Vec<u8>; it was valid UTF-8.
    unsafe { String::from_utf8_unchecked(reconstruct_owned_bytes(r)) }
}

fn reconstruct_owned_vec_vec_u8(r: &mut SlotReader) -> Vec<Vec<u8>> {
    let ptr = r.read_u64() as *mut Vec<u8>;
    let len = r.read_u64() as usize;
    if ptr.is_null() || len == 0 {
        Vec::new()
    } else {
        unsafe {
            let fat = std::ptr::slice_from_raw_parts_mut(ptr, len);
            Box::from_raw(fat).into_vec()
        }
    }
}

// === Module ===

#[pymodule]
fn dynspire(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(load_spier, m)?)?;
    m.add_class::<SpierLib>()?;
    m.add_class::<SpierSchema>()?;
    m.add_class::<SpierHandle>()?;
    m.add_class::<OpaqueHandle>()?;
    Ok(())
}
