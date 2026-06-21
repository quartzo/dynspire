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

use pyo3::exceptions::{PyAttributeError, PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyBool, PyBytes, PyDict, PyDictMethods, PyList, PyString, PyTuple};

use dynspire_core::ffi::{
    DynSpireIdl, FreeFn, IdlMethod, VecView, IDL_ARRAY, IDL_BOOL, IDL_ENUM, IDL_OPTION,
    IDL_OUT_VEC, IDL_SLICE, IDL_STRING, IDL_STRUCT, IDL_STR, IDL_TUPLE, IDL_U32, IDL_U64, IDL_U8,
    IDL_F32, IDL_F64, IDL_UNIT, IDL_VEC,
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
type FnVecView = unsafe extern "C" fn(*mut c_void) -> VecView;

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
    enums: Vec<EnumInfo>,
    free_fn: Option<FreeFn>,
    /// Keeps the spier `.so` mapped. Cloned into every handle/opaque value so
    /// that their function pointers stay valid for as long as they live — the
    /// `.so` is only `dlclose`d once the last reference (including pending
    /// `dynspire_free` drops) is gone.
    #[allow(dead_code)]
    lib: Arc<libloading::Library>,
    // enums omitted from this skeleton — port read_schema_enums when needed.
}

/// A parsed `#[slot_enum]`: its name and variants (each variant's field types
/// resolved to GLOBAL type-table indices, so field decode recurses normally).
struct EnumInfo {
    name: String,
    variants: Vec<VariantInfo>,
}

struct VariantInfo {
    name: String,
    disc: u32,
    field_type_idxs: Vec<u32>,
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
    let mut types: Vec<TypeNode> = std::slice::from_raw_parts(raw.type_table, raw.type_count)
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

    // Parse each enum descriptor. Every enum carries its own mini type-table
    // for its variant field types; we merge those nodes into the global `types`
    // (remapping child indices) so field decode recurses through the normal
    // type table, and variant field indices become global.
    let mut enums: Vec<EnumInfo> = Vec::new();
    for i in 0..raw.enum_count {
        let ed_ptr = *raw.enum_table.add(i);
        if ed_ptr.is_null() {
            continue;
        }
        let ed = &*ed_ptr;
        let base = types.len() as i32;
        let enum_tt = std::slice::from_raw_parts(ed.type_table, ed.type_count);
        for node in enum_tt {
            types.push(TypeNode {
                kind: node.kind,
                _size: node.size,
                child0: if node.child0 >= 0 { node.child0 + base } else { node.child0 },
                child1: if node.child1 >= 0 { node.child1 + base } else { node.child1 },
            });
        }

        let ft_flat = if ed.field_type_count > 0 {
            std::slice::from_raw_parts(ed.field_types, ed.field_type_count)
        } else {
            &[][..]
        };
        let varr = if ed.variant_count > 0 {
            std::slice::from_raw_parts(ed.variants, ed.variant_count)
        } else {
            &[][..]
        };

        let mut variants = Vec::with_capacity(varr.len());
        for v in varr {
            let start = v.field_type_offset as usize;
            let count = v.field_count as usize;
            let field_type_idxs: Vec<u32> = ft_flat
                .get(start..start + count)
                .unwrap_or(&[])
                .iter()
                .map(|&local| (base + local as i32) as u32)
                .collect();
            variants.push(VariantInfo {
                name: read_name(v.name, v.name_len),
                disc: v.disc,
                field_type_idxs,
            });
        }
        enums.push(EnumInfo {
            name: read_name(ed.name, ed.name_len),
            variants,
        });
    }

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
        enums,
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
    vec_free: FnVecFree,
    vec_view: FnVecView,
}

#[pymethods]
impl SpierLib {
    /// Returns the spier's reflected schema (methods, types, structs).
    fn schema(&self) -> SpierSchema {
        SpierSchema {
            data: self.data.clone(),
        }
    }

    /// Returns the IDL hash (same as schema.hash).
    fn idl_hash(&self) -> u64 {
        self.data.hash
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
            vec_create: self.vec_create,
            vec_free: self.vec_free,
            vec_view: self.vec_view,
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
/// Discovery follows the same priority for all hosts:
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

    // Out-vec helpers (used by the unified invoke path, resolved from the .so).
    let vec_create: FnVecCreate = load_sym(&lib, b"dynspire_vec_create\0")?;
    let vec_free: FnVecFree = load_sym(&lib, b"dynspire_vec_free\0")?;
    let vec_view: FnVecView = load_sym(&lib, b"dynspire_vec_view\0")?;

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
        vec_view,
    })
}

// === SpierSchema ===

#[pyclass(name = "SpierSchema")]
struct SpierSchema {
    data: Arc<SchemaData>,
}

fn schema_type_str(data: &SchemaData, idx: u32) -> String {
    let types = &data.types;
    if (idx as usize) >= types.len() {
        return "?".into();
    }
    let t = &types[idx as usize];
    let name = kind_name(t.kind);
    if t.kind == IDL_STRUCT && t.child0 >= 0 {
        let s = data
            .struct_names
            .get(t.child0 as usize)
            .cloned()
            .unwrap_or_else(|| "?".into());
        return format!("Struct<{s}>");
    }
    let mut parts = Vec::new();
    if t.child0 >= 0 {
        parts.push(schema_type_str(data, t.child0 as u32));
    }
    if t.child1 >= 0 {
        parts.push(schema_type_str(data, t.child1 as u32));
    }
    if parts.is_empty() {
        name.into()
    } else {
        format!("{name}<{}>", parts.join(", "))
    }
}

fn struct_name_at(data: &SchemaData, idx: u32) -> String {
    struct_id_at(data, idx)
        .and_then(|sid| data.struct_names.get(sid as usize).cloned())
        .unwrap_or_else(|| "OpaqueValue".into())
}

/// Returns the struct's stable index (`child0` into `struct_names`) if this
/// type node is a struct, or `None` otherwise. Used for O(1) type-identity
/// comparison without string allocation.
fn struct_id_at(data: &SchemaData, idx: u32) -> Option<i32> {
    data.types.get(idx as usize)
        .filter(|t| t.kind == IDL_STRUCT && t.child0 >= 0)
        .map(|t| t.child0)
}

// === Rich schema objects for introspection ===

#[derive(Clone)]
#[pyclass(name = "SpierParam")]
struct SpierParam {
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    type_idx: u32,
}

#[pyclass(name = "SpierMethod")]
struct SpierMethod {
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    params: Vec<SpierParam>,
    #[pyo3(get)]
    return_type: u32,
    #[pyo3(get)]
    index: usize,
}

impl SpierMethod {
    fn from_info(idx: usize, m: &MethodInfo) -> Self {
        SpierMethod {
            name: m.name.clone(),
            params: m.params.iter().map(|p| SpierParam {
                name: p.name.clone(),
                type_idx: p.type_idx,
            }).collect(),
            return_type: m.return_type_idx,
            index: idx,
        }
    }
}

#[pyclass(name = "SpierTypeInfo")]
struct SpierTypeInfo {
    kind: u8,
    #[pyo3(get)]
    type_idx: u32,
}

#[pymethods]
impl SpierTypeInfo {
    #[getter]
    fn kind_name(&self) -> &'static str {
        kind_name(self.kind)
    }
}

/// A factory callable: `EnumClass.variant_name(payload)` → SpierEnumValue.
#[pyclass(name = "EnumVariantFactory")]
struct EnumVariantFactory {
    variant: String,
}

#[pymethods]
impl EnumVariantFactory {
    #[pyo3(signature = (*args))]
    fn __call__(&self, args: &Bound<'_, PyTuple>) -> SpierEnumValue {
        SpierEnumValue {
            variant: self.variant.clone(),
            fields: args.as_unbound().clone_ref(args.py()),
        }
    }
}

/// An enum namespace returned by `create_enum_class()`.
/// `cls.variant_name(payload)` creates a SpierEnumValue.
#[pyclass(name = "SpierEnumClass")]
struct SpierEnumClass {
    variants: Vec<String>,
}

#[pymethods]
impl SpierEnumClass {
    fn __getattr__(&self, _py: Python<'_>, name: &str) -> PyResult<EnumVariantFactory> {
        if self.variants.iter().any(|v| v == name) {
            Ok(EnumVariantFactory { variant: name.to_string() })
        } else {
            Err(PyAttributeError::new_err(format!("enum has no variant {:?}", name)))
        }
    }

    fn __repr__(&self) -> String {
        format!("<SpierEnumClass [{}]>", self.variants.join(", "))
    }
}

/// Enum schema descriptor returned by `schema.enum_by_name(name)`.
#[pyclass(name = "SpierEnumSchema")]
struct SpierEnumSchema {
    #[pyo3(get)]
    name: String,
    variants: Vec<String>,
}

#[pymethods]
impl SpierEnumSchema {
    #[getter]
    fn variant_names(&self) -> Vec<String> {
        self.variants.clone()
    }

    fn create_enum_class(&self) -> SpierEnumClass {
        SpierEnumClass { variants: self.variants.clone() }
    }

    fn __repr__(&self) -> String {
        format!("<SpierEnumSchema {:?} [{}]>", self.name, self.variants.join(", "))
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

    /// Methods in declaration order (as SpierMethod objects with .name, .params, etc.).
    #[getter]
    fn methods(&self) -> Vec<SpierMethod> {
        self.data.methods.iter().enumerate()
            .map(|(i, m)| SpierMethod::from_info(i, m))
            .collect()
    }

    /// Returns the SpierMethod for a given name.
    fn method(&self, name: &str) -> PyResult<SpierMethod> {
        self.data.methods.iter().enumerate()
            .find(|(_, m)| m.name == name)
            .map(|(i, m)| SpierMethod::from_info(i, m))
            .ok_or_else(|| PyValueError::new_err(format!("unknown method: {name}")))
    }

    /// Returns type info at a given type-table index.
    fn type_at(&self, type_idx: u32) -> PyResult<SpierTypeInfo> {
        self.data.types.get(type_idx as usize)
            .map(|t| SpierTypeInfo { kind: t.kind, type_idx })
            .ok_or_else(|| PyValueError::new_err(format!("type index {type_idx} out of range")))
    }

    /// Returns the enum schema by name.
    fn enum_by_name(&self, name: &str) -> PyResult<SpierEnumSchema> {
        self.data.enums.iter()
            .find(|e| e.name == name)
            .map(|e| SpierEnumSchema {
                name: e.name.clone(),
                variants: e.variants.iter().map(|v| v.name.clone()).collect(),
            })
            .ok_or_else(|| PyValueError::new_err(format!("unknown enum: {name}")))
    }

    fn method_sig(&self, name_or_method: &Bound<'_, PyAny>) -> PyResult<String> {
        let name = if let Ok(s) = name_or_method.extract::<String>() {
            s
        } else if let Ok(m) = name_or_method.extract::<PyRef<'_, SpierMethod>>() {
            m.name.clone()
        } else {
            return Err(PyTypeError::new_err("expected method name (str) or SpierMethod"));
        };
        let m = self
            .data
            .methods
            .iter()
            .find(|m| m.name == name)
            .ok_or_else(|| PyValueError::new_err(format!("unknown method: {name}")))?;
        let params = m
            .params
            .iter()
            .map(|p| format!("{}: {}", p.name, schema_type_str(&self.data, p.type_idx)))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(format!(
            "{}({params}) -> Result<{}, String>",
            m.name,
            schema_type_str(&self.data, m.return_type_idx)
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
        IDL_F32 => "F32",
        IDL_F64 => "F64",
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

/// RAII guard that frees every out-vec handle created via `dynspire_vec_create`,
/// ensuring release on every path (success, error, panic).
struct OutVecGuard {
    ptrs: Vec<*mut c_void>,
    free: FnVecFree,
}

impl Drop for OutVecGuard {
    fn drop(&mut self) {
        for p in &self.ptrs {
            unsafe { (self.free)(*p) };
        }
    }
}

// === SpierHandle ===

/// Runtime handle for calling a spier. `Drop` calls `dynspire_destroy`.
#[pyclass(name = "SpierHandle")]
struct SpierHandle {
    data: Arc<SchemaData>,
    dispatch: Arc<[FnDispatch]>,
    destroy: FnDestroy,
    vec_create: FnVecCreate,
    vec_free: FnVecFree,
    vec_view: FnVecView,
    handle: *mut c_void,
}

unsafe impl Send for SpierHandle {}
unsafe impl Sync for SpierHandle {}

/// Merges positional args and kwargs into a single tuple suitable for `invoke`.
/// If kwargs is empty, returns the original args. Otherwise builds a dict from
/// positional args (mapped by param name) merged with kwargs.
fn merge_kwargs<'py>(
    py: Python<'py>,
    data: &SchemaData,
    method: &MethodInfo,
    args: &Bound<'py, PyTuple>,
    kwargs: Option<&Bound<'py, PyDict>>,
) -> PyResult<Bound<'py, PyTuple>> {
    let Some(kw) = kwargs else {
        return Ok(args.clone());
    };
    if kw.is_empty() {
        return Ok(args.clone());
    }
    let d = PyDict::new(py);
    let mut pos = 0;
    for p in &method.params {
        if data.types[p.type_idx as usize].kind == IDL_OUT_VEC {
            continue;
        }
        if pos < args.len() {
            d.set_item(p.name.as_str(), args.get_item(pos)?)?;
            pos += 1;
        }
    }
    for (k, v) in kw.iter() {
        d.set_item(k, v)?;
    }
    Ok(PyTuple::new(py, [d.into_any()])?)
}

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

    /// True if the method has any `&mut Vec<u8>` (out-vec) parameters.
    fn has_outvec(&self, method: &MethodInfo) -> bool {
        method
            .params
            .iter()
            .any(|p| self.data.types[p.type_idx as usize].kind == IDL_OUT_VEC)
    }

    /// Unified entry point: routes to the out-vec or plain path automatically.
    /// If a single dict arg is passed, expands to positional args by param name.
    fn invoke<'py>(
        &self,
        py: Python<'py>,
        method_name: &str,
        args: &Bound<'py, PyTuple>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let (idx, method) = self.find_method(method_name)?;

        // Dict-arg expansion: h.call("put", {"cf": 0, "key": b"..."}) → positional.
        let dict = if args.len() == 1 {
            match args.get_item(0)?.extract::<std::collections::HashMap<String, Py<PyAny>>>() {
                Ok(d) => Some(d),
                Err(_) => None,
            }
        } else {
            None
        };

        if let Some(d) = dict {
            let mut values: Vec<Bound<'py, PyAny>> = Vec::new();
            for p in &method.params {
                if self.data.types[p.type_idx as usize].kind == IDL_OUT_VEC {
                    continue;
                }
                let val = d.get(&p.name).ok_or_else(|| {
                    PyValueError::new_err(format!("missing keyword argument {:?}", p.name))
                })?;
                values.push(val.clone_ref(py).into_bound(py));
            }
            let tup = PyTuple::new(py, values)?;
            if self.has_outvec(method) {
                self.invoke_outvec(py, idx, method, &tup)
            } else {
                self.invoke_plain(py, idx, method, &tup)
            }
        } else if self.has_outvec(method) {
            self.invoke_outvec(py, idx, method, args)
        } else {
            self.invoke_plain(py, idx, method, args)
        }
    }

    fn invoke_plain<'py>(
        &self,
        py: Python<'py>,
        idx: usize,
        method: &MethodInfo,
        args: &Bound<'py, PyTuple>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if method.params.len() != args.len() {
            return Err(PyValueError::new_err(format!(
                "{} expects {} args, got {}",
                method.name,
                method.params.len(),
                args.len()
            )));
        }

        // Encode. Input borrows alias the Python objects held by `args`, which
        // stay alive for the duration of this synchronous call.
        let mut w = SlotWriter::new();
        let mut keepalive: Vec<Box<dyn std::any::Any + Send>> = Vec::new();
        for (p, arg) in method.params.iter().zip(args.iter()) {
            encode_value(py, &mut w, p.type_idx, &self.data, &arg, &mut keepalive)?;
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

    fn invoke_outvec<'py>(
        &self,
        py: Python<'py>,
        idx: usize,
        method: &MethodInfo,
        args: &Bound<'py, PyTuple>,
    ) -> PyResult<Bound<'py, PyAny>> {
        // Out-vec params are skipped from the caller's args; they're allocated here.
        let input_count = method
            .params
            .iter()
            .filter(|p| self.data.types[p.type_idx as usize].kind != IDL_OUT_VEC)
            .count();
        if args.len() != input_count {
            return Err(PyValueError::new_err(format!(
                "{} expects {} input arg(s) (excluding out-vecs), got {}",
                method.name,
                input_count,
                args.len()
            )));
        }

        // Build the slot stream in declaration order: out-vec params get a fresh
        // `Vec<u8>` handle (freed by the guard), inputs are encoded from `args`.
        let mut w = SlotWriter::new();
        let mut keepalive: Vec<Box<dyn std::any::Any + Send>> = Vec::new();
        let mut guard = OutVecGuard {
            ptrs: Vec::new(),
            free: self.vec_free,
        };
        let mut arg_iter = args.iter();
        for p in &method.params {
            if self.data.types[p.type_idx as usize].kind == IDL_OUT_VEC {
                let vp = unsafe { (self.vec_create)() };
                if vp.is_null() {
                    return Err(PyRuntimeError::new_err("dynspire_vec_create returned null"));
                }
                guard.ptrs.push(vp);
                w.write_u64(vp as u64);
            } else {
                let arg = arg_iter
                    .next()
                    .expect("input arg count was validated above");
                encode_value(py, &mut w, p.type_idx, &self.data, &arg, &mut keepalive)?;
            }
        }
        drop(keepalive);

        // Dispatch (GIL held: input borrows into Python memory are safe).
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
        if ret != 0 {
            return Err(PyRuntimeError::new_err(format!(
                "dispatch transport error (code {ret})"
            )));
        }

        // Snapshot the out-vec contents before the guard frees the handles.
        let out_bytes: Vec<Vec<u8>> = guard
            .ptrs
            .iter()
            .map(|&vp| {
                let view = unsafe { (self.vec_view)(vp) };
                if view.ptr.is_null() || view.len == 0 {
                    Vec::new()
                } else {
                    unsafe { std::slice::from_raw_parts(view.ptr, view.len).to_vec() }
                }
            })
            .collect();

        // Decode the return value (Result<T, String> framing, same as plain).
        let mut r = SlotReader::new(&out);
        let tag = r.read_u64();
        let ret_val = if tag == 1 {
            let err = reconstruct_owned_string(&mut r);
            return Err(PyRuntimeError::new_err(format!("spier error: {err}")));
        } else {
            decode_value(&mut r, method.return_type_idx, &self.data, py)?
        };

        let list = PyList::new(py, out_bytes.iter().map(|b| PyBytes::new(py, b)))?;
        PyTuple::new(py, [ret_val, list.into_any()]).map(|t| t.into_any())
    }
}

#[pymethods]
impl SpierHandle {
    /// Escape-hatch dispatch by method name. Prefer attribute access
    /// (`h.compress(data)`) — this exists for dynamic method names. Methods
    /// with out-vec (`&mut Vec<u8>`) params automatically return
    /// `(ret_val, list[bytes])`.
    #[pyo3(signature = (method_name, *args, **kwargs))]
    fn call<'py>(
        &self,
        py: Python<'py>,
        method_name: &str,
        args: &Bound<'py, PyTuple>,
        kwargs: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let (_, method) = self.find_method(method_name)?;
        let args = merge_kwargs(py, &self.data, method, args, kwargs)?;
        self.invoke(py, method_name, &args)
    }

    /// Attribute dispatch: `h.<method_name>` returns a bound callable.
    fn __getattr__<'py>(
        slf: &Bound<'py, Self>,
        py: Python<'py>,
        name: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        let this = slf.borrow();
        let exists = this.data.methods.iter().any(|m| m.name == name);
        drop(this);
        if exists {
            let handle = slf.clone().unbind();
            let bm = BoundMethod {
                handle,
                method: name.to_string(),
            };
            Py::new(py, bm).map(|o| o.into_bound(py).into_any())
        } else {
            Err(PyAttributeError::new_err(format!(
                "'SpierHandle' object has no attribute {name:?}"
            )))
        }
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

// === BoundMethod: callable wrapper returned by SpierHandle.__getattr__ ===

/// A spier method bound to a specific handle. Created by attribute access on a
/// [`SpierHandle`] (e.g. `h.compress`). Calling it dispatches through the same
/// unified invoke path as attribute access (e.g. `h.compress(data)`), so out-vec methods automatically
/// return `(ret_val, list[bytes])`.
#[pyclass(name = "BoundMethod", module = "dynspire")]
struct BoundMethod {
    handle: Py<SpierHandle>,
    method: String,
}

#[pymethods]
impl BoundMethod {
    #[pyo3(signature = (*args, **kwargs))]
    fn __call__<'py>(
        &self,
        py: Python<'py>,
        args: &Bound<'py, PyTuple>,
        kwargs: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let h = self.handle.borrow(py);
        let (_, method) = h.find_method(&self.method)?;
        let args = merge_kwargs(py, &h.data, method, args, kwargs)?;
        h.invoke(py, &self.method, &args)
    }

    fn __repr__(&self) -> String {
        format!("<bound spier method {:?}>", self.method)
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

    /// The struct type name from the spier's IDL schema (e.g. "CompressionReport").
    #[getter]
    fn type_name(&self) -> String {
        struct_name_at(&self.data, self.type_idx)
    }

    fn __int__(&self) -> u64 {
        self.ptr
    }

    fn __bool__(&self) -> bool {
        self.ptr != 0
    }

    fn __repr__(&self) -> String {
        format!("<{} 0x{:x}>", struct_name_at(&self.data, self.type_idx), self.ptr)
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

// === SpierEnumValue: a #[slot_enum] value (variant name + payload fields) ===

/// A `#[slot_enum]` value returned by a spier, or built to pass one back as an
/// input. `variant` is the variant name; `fields` is the tuple of payload slots
/// (empty for unit variants).
#[pyclass(name = "SpierEnumValue")]
struct SpierEnumValue {
    variant: String,
    fields: Py<PyTuple>,
}

#[pymethods]
impl SpierEnumValue {
    #[new]
    #[pyo3(signature = (variant, *fields))]
    fn new(variant: String, fields: &Bound<'_, PyTuple>) -> Self {
        Self {
            variant,
            fields: fields.as_unbound().clone_ref(fields.py()),
        }
    }

    #[getter]
    fn variant(&self) -> &str {
        &self.variant
    }

    #[getter]
    fn fields(&self, py: Python<'_>) -> Py<PyTuple> {
        self.fields.clone_ref(py)
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        Ok(format!(
            "SpierEnumValue({:?}, {})",
            self.variant,
            self.fields.bind(py).repr()?
        ))
    }

    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        other.extract::<PyRef<'_, SpierEnumValue>>()
            .map(|o| self.variant == o.variant)
            .unwrap_or(false)
    }

    fn __hash__(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.variant.hash(&mut h);
        h.finish()
    }
}

// === Dynamic encoder (Python arg -> slots) ===

fn encode_value<'py>(
    py: Python<'py>,
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
        IDL_U32 => {
            w.write_u64(arg.extract::<u32>()? as u64);
            Ok(())
        }
        IDL_U64 => {
            w.write_u64(arg.extract::<u64>()?);
            Ok(())
        }
        IDL_F32 => {
            w.write_u64(arg.extract::<f32>()?.to_bits() as u64);
            Ok(())
        }
        IDL_F64 => {
            w.write_u64(arg.extract::<f64>()?.to_bits());
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
            // Pass-through of an opaque handle returned earlier.
            let expected_id = struct_id_at(data, type_idx);
            if let Ok(h) = arg.extract::<PyRef<'_, OpaqueHandle>>() {
                let actual_id = struct_id_at(data, h.type_idx);
                if actual_id != expected_id {
                    return Err(PyTypeError::new_err(format!(
                        "expected {}, got {}",
                        struct_name_at(data, type_idx),
                        struct_name_at(data, h.type_idx),
                    )));
                }
                w.write_u64(h.ptr);
                Ok(())
            } else {
                Err(PyTypeError::new_err(format!(
                    "expected {} (OpaqueHandle), got {}",
                    struct_name_at(data, type_idx),
                    arg.get_type().name()?,
                )))
            }
        }
        IDL_ENUM => {
            let ev = arg.extract::<PyRef<'_, SpierEnumValue>>()?;
            let ei = data.enums
                .get(t.child0 as usize)
                .ok_or_else(|| PyValueError::new_err(format!("enum index {} out of range", t.child0)))?;
            let variant = ei
                .variants
                .iter()
                .find(|v| v.name == ev.variant)
                .ok_or_else(|| {
                    PyValueError::new_err(format!("enum {} has no variant {:?}", ei.name, ev.variant))
                })?;
            w.write_u64(variant.disc as u64);
            let fields = ev.fields.bind(py);
            if fields.len() != variant.field_type_idxs.len() {
                return Err(PyValueError::new_err(format!(
                    "variant {:?} expects {} field(s), got {}",
                    ev.variant,
                    variant.field_type_idxs.len(),
                    fields.len()
                )));
            }
            for (i, &ft_idx) in variant.field_type_idxs.iter().enumerate() {
                let f = fields.get_item(i)?;
                encode_value(py, w, ft_idx, data, &f, keepalive)?;
            }
            Ok(())
        }
        // TODO: IDL_ARRAY, IDL_OPTION, IDL_TUPLE inputs.
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
        IDL_F32 => Ok(f32::from_bits(r.read_u64() as u32).into_pyobject(py)?.into_any()),
        IDL_F64 => Ok(f64::from_bits(r.read_u64()).into_pyobject(py)?.into_any()),
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
            } else if child.kind == IDL_STRING {
                let v = reconstruct_owned_vec_string(r);
                let list = PyList::new(py, v.iter().map(|s| PyString::new(py, s)))?;
                Ok(list.into_any())
            } else {
                // TODO: Vec<[u8; N]>, Vec<(Vec<u8>, Vec<u8>)>.
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
        IDL_ENUM => {
            let ei = data.enums
                .get(t.child0 as usize)
                .ok_or_else(|| PyValueError::new_err(format!("enum index {} out of range", t.child0)))?;
            let disc = r.read_u64() as u32;
            let variant = ei
                .variants
                .iter()
                .find(|v| v.disc == disc)
                .ok_or_else(|| {
                    PyValueError::new_err(format!("enum {} unknown discriminant {}", ei.name, disc))
                })?;
            let mut fields: Vec<Bound<'py, PyAny>> = Vec::new();
            for &ft_idx in &variant.field_type_idxs {
                fields.push(decode_value(r, ft_idx, data, py)?);
            }
            let tup = PyTuple::new(py, fields)?;
            let obj = SpierEnumValue {
                variant: variant.name.clone(),
                fields: tup.unbind(),
            };
            Ok(Bound::new(py, obj)?.into_any())
        }
        // TODO: IDL_ARRAY returns.
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

/// Reconstructs an owned `Vec<String>` from its `(ptr, len)` slot pair. The
/// spier leaks a `Box<[String]>`; `Box::from_raw` reclaims the whole array and
/// every `String` in it (no 24-byte stride arithmetic — we're in Rust).
fn reconstruct_owned_vec_string(r: &mut SlotReader) -> Vec<String> {
    let ptr = r.read_u64() as *mut String;
    let len = r.read_u64() as usize;
    if ptr.is_null() || len == 0 {
        return Vec::new();
    }
    unsafe {
        let fat = std::ptr::slice_from_raw_parts_mut(ptr, len);
        Box::from_raw(fat).into_vec()
    }
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
    m.add_class::<SpierMethod>()?;
    m.add_class::<SpierParam>()?;
    m.add_class::<SpierTypeInfo>()?;
    m.add_class::<SpierEnumSchema>()?;
    m.add_class::<SpierEnumClass>()?;
    m.add_class::<EnumVariantFactory>()?;
    m.add_class::<SpierHandle>()?;
    m.add_class::<BoundMethod>()?;
    m.add_class::<OpaqueHandle>()?;
    m.add_class::<SpierEnumValue>()?;
    Ok(())
}
