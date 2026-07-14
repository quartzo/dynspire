//! Code generator: AST → Rust source string.
//!
//! [`generate`] produces a single `.rs` file containing everything the IDL
//! crate needs: trait, types, Op enum, hash, schema, tower client, and the
//! spier dispatch macro.

use crate::ast::*;
use crate::parser;

use std::collections::HashMap;

// ===========================================================================
// BuildContext — deduplication across multiple build() calls
// ===========================================================================

/// Shared context for deduplicating types across multiple [`build`](BuildContext::build) calls.
///
/// When two `.dspi` files include the same type fragment, the generated code
/// would contain duplicate type definitions (e.g. `pub struct SharedHandle`).
/// `BuildContext` tracks which types have already been emitted and skips
/// definitions that are identical, or panics if they conflict.
///
/// ```ignore
/// let mut ctx = dynspire_codegen::BuildContext::new();
/// ctx.build("src/a.dspi");   // generates SharedHandle
/// ctx.build("src/b.dspi");   // skips SharedHandle (already exists, same content)
/// ```
pub struct BuildContext {
    seen: HashMap<String, String>,
}

impl BuildContext {
    pub fn new() -> Self {
        Self {
            seen: HashMap::new(),
        }
    }

    /// Check if a type has already been emitted. Returns `true` if it should be
    /// skipped (already seen with matching canonical), panics on conflict.
    fn check_or_register(&mut self, name: &str, canonical: &str) -> bool {
        match self.seen.entry(name.to_string()) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(canonical.to_string());
                false
            }
            std::collections::hash_map::Entry::Occupied(e) => {
                if e.get() != canonical {
                    panic!(
                        "dynspire-codegen: conflicting type `{name}`: \
                         previously defined as `{}`, now `{}`",
                        e.get(),
                        canonical,
                    );
                }
                true
            }
        }
    }

    /// Read, parse, resolve includes, validate, generate, and write a `.dspi` file.
    /// Types already emitted in previous calls are deduplicated.
    pub fn build(&mut self, dspi_path: &str) {
        self.build_with(dspi_path, "spier", generate_spier_with_ctx);
    }

    /// Like [`build`](Self::build) but generates only the spier side.
    pub fn build_spier(&mut self, dspi_path: &str) {
        self.build_with(dspi_path, "spier", generate_spier_with_ctx);
    }

    /// Like [`build`](Self::build) but generates only the host side.
    pub fn build_host(&mut self, dspi_path: &str) {
        self.build_with(dspi_path, "host", generate_host_with_ctx);
    }

    /// Generate a pure-Python ctypes client module and write it to
    /// `py_out_path` (relative to `CARGO_MANIFEST_DIR`).
    ///
    /// The Python consumer never needs a Rust toolchain — the `.py` is emitted
    /// at the spier's build time, alongside the compiled `.so`. There is no
    /// runtime schema reflection: the slot layout and `dynspire_free` type
    /// indices are baked in as constants.
    pub fn build_python(&mut self, dspi_path: &str, py_out_path: &str) {
        let (iface, rerun_paths) = load_interface(dspi_path);
        let code = generate_python_with_ctx(&iface, &mut BuildContext::new());

        let dir = match std::env::var("CARGO_MANIFEST_DIR") {
            Ok(manifest) if !manifest.is_empty() => format!("{manifest}/{py_out_path}"),
            _ => py_out_path.to_string(),
        };
        if let Some(parent) = std::path::Path::new(&dir).parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&dir, &code)
            .unwrap_or_else(|e| panic!("dynspire-codegen: failed to write {dir}: {e}"));

        for path in &rerun_paths {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    fn build_with(
        &mut self,
        dspi_path: &str,
        suffix: &str,
        gen_fn: fn(&Interface, &mut BuildContext) -> String,
    ) {
        let (iface, rerun_paths) = load_interface(dspi_path);
        let code = gen_fn(&iface, self);
        let out_dir = std::env::var("OUT_DIR")
            .expect("dynspire-codegen: OUT_DIR not set (must be called from build.rs)");
        let file_name = format!("{out_dir}/{}_{}.rs", iface.name.to_lowercase(), suffix);
        std::fs::write(&file_name, &code)
            .unwrap_or_else(|e| panic!("dynspire-codegen: failed to write {file_name}: {e}"));

        for path in &rerun_paths {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

impl Default for BuildContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a `.dspi`, resolve includes, merge, and validate. Shared by
/// [`BuildContext::build_with`] and [`BuildContext::build_python`].
fn load_interface(dspi_path: &str) -> (Interface, Vec<String>) {
    let src = std::fs::read_to_string(dspi_path)
        .unwrap_or_else(|e| panic!("dynspire-codegen: failed to read {dspi_path}: {e}"));
    let mut iface = parser::parse(&src)
        .unwrap_or_else(|e| panic!("dynspire-codegen: parse error in {dspi_path}: {e}"));

    let base_dir = std::path::Path::new(dspi_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));

    let mut rerun_paths = vec![dspi_path.to_string()];
    let mut included_types = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = Vec::new();
    let mut processed: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();

    resolve_includes(
        &iface.includes,
        base_dir,
        &mut stack,
        &mut processed,
        &mut included_types,
        &mut rerun_paths,
    );
    merge_types(&mut iface, included_types);
    parser::validate(&iface)
        .unwrap_or_else(|e| panic!("dynspire-codegen: validation error in {dspi_path}: {e}"));

    (iface, rerun_paths)
}

// ===========================================================================
// Name helpers
// ===========================================================================

fn trait_name(iface: &Interface) -> String {
    format!("{}Engine", iface.name)
}

fn op_name(iface: &Interface) -> String {
    format!("{}Op", iface.name)
}

fn hash_const_name(iface: &Interface) -> String {
    format!("{}_IDL_HASH", iface.name.to_uppercase())
}

fn client_name(iface: &Interface) -> String {
    format!("DynSpire{}", iface.name)
}

fn spier_macro_name(iface: &Interface) -> String {
    format!("impl_{}_spier", iface.name.to_lowercase())
}

fn pascal(s: &str) -> String {
    s.split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect()
}

// ===========================================================================
// Inline slot encoding/decoding — replaces trait-based dispatch
// ===========================================================================

fn find_type<'a>(types: &'a [TypeDecl], name: &str) -> &'a TypeDecl {
    types
        .iter()
        .find(|t| t.name() == name)
        .unwrap_or_else(|| panic!("dynspire-codegen: unknown type reference: {name}"))
}

/// Generate inline write-encode statements for a borrowed value (input param).
fn gen_write_encode(ft: &FieldType, expr: &str, w: &str, types: &[TypeDecl]) -> String {
    match ft {
        FieldType::Unit => String::new(),
        FieldType::Bool => format!("{w}.write_u64(if {expr} {{ 1 }} else {{ 0 }});"),
        FieldType::U8 | FieldType::I8 => format!("{w}.write_u64({expr} as u64);"),
        FieldType::U16 | FieldType::I16 | FieldType::U32 | FieldType::I32 => {
            format!("{w}.write_u64({expr} as u64);")
        }
        FieldType::U64 => format!("{w}.write_u64({expr});"),
        FieldType::I64 => format!("{w}.write_u64({expr} as u64);"),
        FieldType::F32 => format!("{w}.write_u64({expr}.to_bits() as u64);"),
        FieldType::F64 => format!("{w}.write_u64({expr}.to_bits());"),
        FieldType::Str | FieldType::U8Slice => {
            format!("{w}.write_u64({expr}.as_ptr() as u64); {w}.write_u64({expr}.len() as u64);")
        }
        FieldType::OutU8Vec => {
            format!("{w}.write_u64(core::ptr::addr_of!(*{expr}) as u64);")
        }
        FieldType::String => {
            format!("{w}.write_u64({expr}.as_ptr() as u64); {w}.write_u64({expr}.len() as u64);")
        }
        FieldType::Vec(_) => {
            format!("{w}.write_u64({expr}.as_ptr() as u64); {w}.write_u64({expr}.len() as u64);")
        }
        FieldType::Option(inner) => {
            let some = gen_write_encode(inner, "__v", w, types);
            if some.is_empty() {
                format!("if {expr}.is_some() {{ {w}.write_u64(1); }} else {{ {w}.write_u64(0); }}")
            } else {
                format!("if let Some(__v) = {expr} {{ {w}.write_u64(1); {some} }} else {{ {w}.write_u64(0); }}")
            }
        }
        FieldType::Tuple(elems) => {
            let mut s = String::new();
            for (i, e) in elems.iter().enumerate() {
                s.push_str(&gen_write_encode(e, &format!("{expr}.{i}"), w, types));
            }
            s
        }
        FieldType::Array(inner, len) => {
            if matches!(inner.as_ref(), FieldType::U8) && *len % 8 == 0 {
                let mut s = String::new();
                for i in 0..*len / 8 {
                    let start = i * 8;
                    s.push_str(&format!(
                        "{w}.write_u64(u64::from_le_bytes({expr}[{start}..{}].try_into().unwrap()));",
                        start + 8,
                    ));
                }
                s
            } else {
                let mut s = String::new();
                for i in 0..*len {
                    s.push_str(&gen_write_encode(inner, &format!("{expr}[{i}]"), w, types));
                }
                s
            }
        }
        FieldType::DStr | FieldType::DSlice(_) => {
            format!("{w}.write_u64({expr}.ptr as u64); {w}.write_u64({expr}.len as u64);")
        }
        FieldType::DString => {
            format!(
                "{w}.write_u64({expr}.allocator as u64); {w}.write_u64({expr}.ptr as u64); \
                 {w}.write_u64({expr}.len as u64); {w}.write_u64({expr}.cap as u64);"
            )
        }
        FieldType::DVec(_) => {
            format!(
                "{w}.write_u64({expr}.allocator as u64); {w}.write_u64({expr}.ptr as u64); \
                 {w}.write_u64({expr}.len as u64); {w}.write_u64({expr}.cap as u64);"
            )
        }
        FieldType::DOption(inner) => {
            let some = gen_write_encode(inner, "__v", w, types);
            format!(
                "if {expr}.tag == 0 {{ {w}.write_u64(0); }} else {{ let __v = {expr}.value; {w}.write_u64(1); {some} }}"
            )
        }
        FieldType::Named(name) => {
            let ty = find_type(types, name);
            match ty {
                TypeDecl::Struct(s) if s.fields.is_empty() => {
                    format!("{w}.write_u64(core::ptr::addr_of!({expr}) as u64);")
                }
                TypeDecl::Struct(_) | TypeDecl::Opaque(_) => {
                    format!("{w}.write_u64(&{expr} as *const {name} as u64);")
                }
                TypeDecl::Enum(e) => gen_write_enum_encode(e, expr, w, types),
            }
        }
    }
}

/// Generate inline write-return statements for an owned value (output return).
fn gen_write_return(ft: &FieldType, expr: &str, w: &str, types: &[TypeDecl]) -> String {
    match ft {
        FieldType::Unit => String::new(),
        FieldType::Bool => format!("{w}.write_u64(if {expr} {{ 1 }} else {{ 0 }});"),
        FieldType::U8 | FieldType::I8 => format!("{w}.write_u64({expr} as u64);"),
        FieldType::U16 | FieldType::I16 | FieldType::U32 | FieldType::I32 => {
            format!("{w}.write_u64({expr} as u64);")
        }
        FieldType::U64 => format!("{w}.write_u64({expr});"),
        FieldType::I64 => format!("{w}.write_u64({expr} as u64);"),
        FieldType::F32 => format!("{w}.write_u64({expr}.to_bits() as u64);"),
        FieldType::F64 => format!("{w}.write_u64({expr}.to_bits());"),
        FieldType::Str | FieldType::U8Slice | FieldType::OutU8Vec => {
            gen_write_encode(ft, expr, w, types)
        }
        FieldType::String => format!(
            "if {expr}.is_empty() {{ {w}.write_u64(0); {w}.write_u64(0); }} else {{ \
             let __len = {expr}.len(); \
             let __data = dynspire::managed::dynspire_alloc(__alloc, __len, 1); \
             if __data.is_null() {{ {w}.write_u64(0); {w}.write_u64(0); }} else {{ \
             core::ptr::copy_nonoverlapping({expr}.as_ptr(), __data, __len); \
             {w}.write_u64(__data as u64); \
             {w}.write_u64(__len as u64); }} }}"
        ),
        FieldType::Vec(inner) => {
            let rt = inner.rust_type();
            format!(
                "if {expr}.is_empty() {{ {w}.write_u64(0); {w}.write_u64(0); }} else {{ \
                 let __len = {expr}.len(); \
                 let __nbytes = __len * core::mem::size_of::<{rt}>(); \
                 let __data = dynspire::managed::dynspire_alloc(__alloc, __nbytes, core::mem::align_of::<{rt}>()); \
                 if __data.is_null() {{ {w}.write_u64(0); {w}.write_u64(0); }} else {{ \
                 core::ptr::copy_nonoverlapping({expr}.as_ptr() as *const u8, __data, __nbytes); \
                 {w}.write_u64(__data as u64); \
                 {w}.write_u64(__len as u64); }} }}"
            )
        }
        FieldType::DStr => format!(
            "{w}.write_u64({expr}.ptr as u64); {w}.write_u64({expr}.len as u64);"
        ),
        FieldType::DSlice(inner) => {
            let rt = inner.rust_type();
            format!(
                "{w}.write_u64({expr}.ptr as *const {rt} as u64); {w}.write_u64({expr}.len as u64);"
            )
        }
        FieldType::DString => format!(
            "{w}.write_u64({expr}.allocator as u64); {w}.write_u64({expr}.ptr as u64); \
             {w}.write_u64({expr}.len as u64); {w}.write_u64({expr}.cap as u64);"
        ),
        FieldType::DVec(_inner) => {
            format!(
                "{w}.write_u64({expr}.allocator as u64); {w}.write_u64({expr}.ptr as u64); \
                 {w}.write_u64({expr}.len as u64); {w}.write_u64({expr}.cap as u64);"
            )
        }
        FieldType::DOption(inner) => {
            let some = gen_write_return(inner, "__v", w, types);
            format!(
                "if {expr}.tag == 0 {{ {w}.write_u64(0); }} else {{ let __v = {expr}.value; {w}.write_u64(1); {some} }}"
            )
        }
        FieldType::Option(inner) => {
            let some = gen_write_return(inner, "__v", w, types);
            if some.is_empty() {
                format!("if {expr}.is_some() {{ {w}.write_u64(1); }} else {{ {w}.write_u64(0); }}")
            } else {
                format!("if let Some(__v) = {expr} {{ {w}.write_u64(1); {some} }} else {{ {w}.write_u64(0); }}")
            }
        }
        FieldType::Tuple(elems) => {
            let mut s = String::new();
            for (i, e) in elems.iter().enumerate() {
                s.push_str(&gen_write_return(e, &format!("{expr}.{i}"), w, types));
            }
            s
        }
        FieldType::Array(inner, len) => gen_write_encode(
            &FieldType::Array(inner.clone(), *len),
            expr,
            w,
            types,
        ),
        FieldType::Named(name) => {
            let ty = find_type(types, name);
            match ty {
                TypeDecl::Struct(s) if s.fields.is_empty() => {
                    format!("{w}.write_u64(core::ptr::addr_of!({expr}) as u64);")
                }
                TypeDecl::Struct(s) if struct_has_dynamic_fields(s, types) => {
                    format!(
                        "let __ptr = dynspire::managed::dyn_alloc(__alloc, core::mem::size_of::<{name}>(), core::mem::align_of::<{name}>(), 0, Some(drop_{name} as unsafe extern \"C\" fn(*mut core::ffi::c_void))) as *mut {name}; \
                         *__ptr = {expr}; \
                         {w}.write_u64(__ptr as u64);"
                    )
                }
                TypeDecl::Struct(_) | TypeDecl::Opaque(_) => {
                    format!(
                        "let __ptr = dynspire::managed::dynspire_alloc(__alloc, core::mem::size_of::<{name}>(), core::mem::align_of::<{name}>()) as *mut {name}; \
                         *__ptr = {expr}; \
                         {w}.write_u64(__ptr as u64);"
                    )
                }
                TypeDecl::Enum(e) => gen_write_enum_return(e, expr, w, types),
            }
        }
    }
}

/// Generate inline read-decode expression for a borrowed value (input param, spier side).
fn gen_read_decode(ft: &FieldType, r: &str, types: &[TypeDecl]) -> String {
    match ft {
        FieldType::Unit => "()".into(),
        FieldType::Bool => format!("{r}.read_u64() != 0"),
        FieldType::U8 => format!("{r}.read_u64() as u8"),
        FieldType::U16 => format!("{r}.read_u64() as u16"),
        FieldType::U32 => format!("{r}.read_u64() as u32"),
        FieldType::U64 => format!("{r}.read_u64()"),
        FieldType::I8 => format!("{r}.read_u64() as i8"),
        FieldType::I16 => format!("{r}.read_u64() as i16"),
        FieldType::I32 => format!("{r}.read_u64() as i32"),
        FieldType::I64 => format!("{r}.read_u64() as i64"),
        FieldType::F32 => format!("f32::from_bits({r}.read_u64() as u32)"),
        FieldType::F64 => format!("f64::from_bits({r}.read_u64())"),
        FieldType::Str => format!(
            "unsafe {{ let __p = {r}.read_u64() as *const u8; let __l = {r}.read_u64() as usize; \
             if __p.is_null() || __l == 0 {{ \"\" }} \
             else {{ core::str::from_utf8_unchecked(core::slice::from_raw_parts(__p, __l)) }} }}"
        ),
        FieldType::U8Slice => format!(
            "unsafe {{ let __p = {r}.read_u64() as *const u8; let __l = {r}.read_u64() as usize; \
             if __p.is_null() || __l == 0 {{ &[] as &[u8] }} \
             else {{ core::slice::from_raw_parts(__p, __l) }} }}"
        ),
        FieldType::OutU8Vec => format!("unsafe {{ &mut *({r}.read_u64() as *mut Vec<u8>) }}"),
        FieldType::String => format!(
            "unsafe {{ let __p = {r}.read_u64() as *const u8; let __l = {r}.read_u64() as usize; \
             if __p.is_null() || __l == 0 {{ String::new() }} \
             else {{ String::from_utf8_unchecked(core::slice::from_raw_parts(__p, __l).to_vec()) }} }}"
        ),
        FieldType::Vec(inner) => {
            let rt = inner.rust_type();
            format!(
                "unsafe {{ let __p = {r}.read_u64() as *const {rt}; let __l = {r}.read_u64() as usize; \
                 if __p.is_null() || __l == 0 {{ Vec::new() }} \
                 else {{ core::slice::from_raw_parts(__p, __l).to_vec() }} }}"
            )
        }
        FieldType::DStr => format!(
            "dynspire::managed::DStr {{ ptr: {r}.read_u64() as *const u8, len: {r}.read_u64() as usize }}"
        ),
        FieldType::DSlice(inner) => {
            let rt = inner.rust_type();
            format!(
                "dynspire::managed::DSlice::<{rt}> {{ ptr: {r}.read_u64() as *const {rt}, len: {r}.read_u64() as usize }}"
            )
        }
        FieldType::DString => format!(
            "dynspire::managed::DString {{ \
             allocator: {r}.read_u64() as *mut dynspire::managed::DynSpireAllocator, \
             ptr: {r}.read_u64() as *mut u8, len: {r}.read_u64() as usize, cap: {r}.read_u64() as usize }}"
        ),
        FieldType::DVec(inner) => {
            let rt = inner.rust_type();
            format!(
                "dynspire::managed::DVec::<{rt}> {{ \
                 allocator: {r}.read_u64() as *mut dynspire::managed::DynSpireAllocator, \
                 ptr: {r}.read_u64() as *mut {rt}, len: {r}.read_u64() as usize, cap: {r}.read_u64() as usize }}"
            )
        }
        FieldType::DOption(inner) => {
            let rt = inner.rust_type();
            let inner_decode = gen_read_decode(inner, r, types);
            format!(
                "{{ let __tag = {r}.read_u64(); if __tag == 0 {{ dynspire::managed::DOption::<{rt}>::none() }} else {{ dynspire::managed::DOption::<{rt}>::some({inner_decode}) }} }}"
            )
        }
        FieldType::Option(inner) => {
            let inner_decode = gen_read_decode(inner, r, types);
            format!(
                "{{ let __tag = {r}.read_u64(); if __tag == 0 {{ None }} else {{ Some({inner_decode}) }} }}"
            )
        }
        FieldType::Tuple(elems) => {
            let parts: Vec<String> = elems.iter().map(|e| gen_read_decode(e, r, types)).collect();
            format!("({})", parts.join(", "))
        }
        FieldType::Array(inner, len) => {
            if matches!(inner.as_ref(), FieldType::U8) && *len % 8 == 0 {
                let mut chunks = String::new();
                for i in 0..*len / 8 {
                    chunks.push_str(&format!(
                        "__arr[{}..{}].copy_from_slice(&{r}.read_u64().to_le_bytes());",
                        i * 8,
                        i * 8 + 8,
                    ));
                }
                format!("{{ let mut __arr = [0u8; {len}]; {chunks} __arr }}")
            } else {
                let mut inits = String::new();
                for _ in 0..*len {
                    inits.push_str(&format!("{},", gen_read_decode(inner, r, types)));
                }
                format!("[{inits}]")
            }
        }
        FieldType::Named(name) => {
            let ty = find_type(types, name);
            match ty {
                TypeDecl::Struct(s) if s.fields.is_empty() => {
                    format!("{{ let _ = {r}.read_u64(); {name} }}")
                }
                TypeDecl::Struct(_) | TypeDecl::Opaque(_) => {
                    format!("unsafe {{ (*({r}.read_u64() as *const {name})).clone() }}")
                }
                TypeDecl::Enum(e) => gen_read_enum_decode(e, r, types),
            }
        }
    }
}

/// Generate inline read-receive expression for an owned value (output return, host side).
fn gen_read_receive(ft: &FieldType, r: &str, types: &[TypeDecl]) -> String {
    match ft {
        FieldType::Unit => "()".into(),
        FieldType::Bool => format!("{r}.read_u64() != 0"),
        FieldType::U8 => format!("{r}.read_u64() as u8"),
        FieldType::U16 => format!("{r}.read_u64() as u16"),
        FieldType::U32 => format!("{r}.read_u64() as u32"),
        FieldType::U64 => format!("{r}.read_u64()"),
        FieldType::I8 => format!("{r}.read_u64() as i8"),
        FieldType::I16 => format!("{r}.read_u64() as i16"),
        FieldType::I32 => format!("{r}.read_u64() as i32"),
        FieldType::I64 => format!("{r}.read_u64() as i64"),
        FieldType::F32 => format!("f32::from_bits({r}.read_u64() as u32)"),
        FieldType::F64 => format!("f64::from_bits({r}.read_u64())"),
        FieldType::Str | FieldType::U8Slice | FieldType::OutU8Vec => {
            gen_read_decode(ft, r, types)
        }
        FieldType::String => format!(
            "unsafe {{ \
             let __ptr = {r}.read_u64() as *mut u8; \
             let __len = {r}.read_u64() as usize; \
             if __ptr.is_null() || __len == 0 {{ String::new() }} \
             else {{ let __v = String::from_utf8_unchecked(core::slice::from_raw_parts(__ptr, __len).to_vec()); dynspire::managed::dynspire_release(__ptr); __v }} }}"
        ),
        FieldType::Vec(inner) => {
            let rt = inner.rust_type();
            format!(
                "unsafe {{ \
                 let __ptr = {r}.read_u64() as *mut {rt}; \
                 let __len = {r}.read_u64() as usize; \
                 if __ptr.is_null() || __len == 0 {{ Vec::new() }} \
                 else {{ let __v = core::slice::from_raw_parts(__ptr, __len).to_vec(); dynspire::managed::dynspire_release(__ptr as *mut u8); __v }} }}"
            )
        }
        FieldType::DStr => format!(
            "dynspire::managed::DStr {{ ptr: {r}.read_u64() as *const u8, len: {r}.read_u64() as usize }}"
        ),
        FieldType::DSlice(inner) => {
            let rt = inner.rust_type();
            format!(
                "dynspire::managed::DSlice::<{rt}> {{ ptr: {r}.read_u64() as *const {rt}, len: {r}.read_u64() as usize }}"
            )
        }
        FieldType::DString => format!(
            "{{ let __alloc = {r}.read_u64() as *mut dynspire::managed::DynSpireAllocator; \
              let __ptr = {r}.read_u64() as *mut u8; let __len = {r}.read_u64() as usize; \
              let __cap = {r}.read_u64() as usize; \
              dynspire::managed::OwnedDString::from_raw(dynspire::managed::DString {{ allocator: __alloc, ptr: __ptr, len: __len, cap: __cap }}) }}"
        ),
        FieldType::DVec(inner) => {
            let rt = inner.rust_type();
            format!(
                "{{ let __alloc = {r}.read_u64() as *mut dynspire::managed::DynSpireAllocator; \
                 let __ptr = {r}.read_u64() as *mut {rt}; let __len = {r}.read_u64() as usize; \
                 let __cap = {r}.read_u64() as usize; \
                  dynspire::managed::OwnedDVec::<{rt}>::from_raw(dynspire::managed::DVec::<{rt}> {{ allocator: __alloc, ptr: __ptr, len: __len, cap: __cap }}) }}"
            )
        }
        FieldType::DOption(inner) => {
            let rt = inner.rust_type();
            let inner_recv = gen_read_receive(inner, r, types);
            format!(
                "{{ let __tag = {r}.read_u64(); if __tag == 0 {{ dynspire::managed::DOption::<{rt}>::none() }} else {{ dynspire::managed::DOption::<{rt}>::some({inner_recv}) }} }}"
            )
        }
        FieldType::Option(inner) => {
            let inner_recv = gen_read_receive(inner, r, types);
            format!(
                "{{ let __tag = {r}.read_u64(); if __tag == 0 {{ None }} else {{ Some({inner_recv}) }} }}"
            )
        }
        FieldType::Tuple(elems) => {
            let parts: Vec<String> = elems.iter().map(|e| gen_read_receive(e, r, types)).collect();
            format!("({})", parts.join(", "))
        }
        FieldType::Array(inner, len) => {
            if matches!(inner.as_ref(), FieldType::U8) && *len % 8 == 0 {
                let mut chunks = String::new();
                for i in 0..*len / 8 {
                    chunks.push_str(&format!(
                        "__arr[{}..{}].copy_from_slice(&{r}.read_u64().to_le_bytes());",
                        i * 8,
                        i * 8 + 8,
                    ));
                }
                format!("{{ let mut __arr = [0u8; {len}]; {chunks} __arr }}")
            } else {
                let mut inits = String::new();
                for _ in 0..*len {
                    inits.push_str(&format!("{},", gen_read_receive(inner, r, types)));
                }
                format!("[{inits}]")
            }
        }
        FieldType::Named(name) => {
            let ty = find_type(types, name);
            match ty {
                TypeDecl::Struct(s) if s.fields.is_empty() => {
                    format!("{{ let _ = {r}.read_u64(); {name} }}")
                }
                TypeDecl::Struct(s) if struct_has_dynamic_fields(s, types) => {
                    format!(
                        "unsafe {{ let __ptr = {r}.read_u64() as *mut {name}; \
                         let __v = core::ptr::read(__ptr); \
                         dynspire::managed::dynspire_dealloc_only(__ptr as *mut u8); __v }}"
                    )
                }
                TypeDecl::Struct(_) | TypeDecl::Opaque(_) => {
                    format!("unsafe {{ let __ptr = {r}.read_u64() as *mut {name}; let __v = core::ptr::read(__ptr); dynspire::managed::dynspire_release(__ptr as *mut u8); __v }}")
                }
                TypeDecl::Enum(e) => gen_read_enum_receive(e, r, types),
            }
        }
    }
}

// --- Enum inline helpers ---

fn gen_write_enum_encode(e: &EnumDecl, expr: &str, w: &str, types: &[TypeDecl]) -> String {
    let n = &e.name;
    let mut arms = String::new();
    for (i, v) in e.variants.iter().enumerate() {
        let disc = i as u64;
        if v.fields.is_empty() {
            arms.push_str(&format!("{n}::{vn} => {{ {w}.write_u64({disc}); }} ", vn=v.name));
        } else {
            let fnames: Vec<String> = (0..v.fields.len()).map(|i| format!("__f{i}")).collect();
            let ref_pats: Vec<String> = fnames.iter().map(|f| format!("ref {f}")).collect();
            let mut field_stmts = String::new();
            for (fty, fname) in v.fields.iter().zip(&fnames) {
                let actual_expr = match fty {
                    FieldType::F32 | FieldType::F64 => format!("(*{fname})"),
                    FieldType::U8 | FieldType::I8 | FieldType::U16 | FieldType::I16
                    | FieldType::U32 | FieldType::I32 | FieldType::U64 | FieldType::I64
                    | FieldType::Bool => format!("*{fname}"),
                    _ => fname.clone(),
                };
                let dt = fty.to_field_dtype();
                field_stmts.push_str(&gen_write_encode(&dt, &actual_expr, w, types));
            }
            arms.push_str(&format!(
                "{n}::{vn}({pats}) => {{ {w}.write_u64({disc}); {field_stmts} }} ",
                vn=v.name, pats=ref_pats.join(", "),
            ));
        }
    }
    format!("match {expr} {{ {arms} }}")
}

fn gen_write_enum_return(e: &EnumDecl, expr: &str, w: &str, types: &[TypeDecl]) -> String {
    let n = &e.name;
    let mut arms = String::new();
    for (i, v) in e.variants.iter().enumerate() {
        let disc = i as u64;
        if v.fields.is_empty() {
            arms.push_str(&format!("{n}::{vn} => {{ {w}.write_u64({disc}); }} ", vn=v.name));
        } else {
            let fnames: Vec<String> = (0..v.fields.len()).map(|i| format!("__f{i}")).collect();
            let mut field_stmts = String::new();
            for (fty, fname) in v.fields.iter().zip(&fnames) {
                field_stmts.push_str(&gen_write_return(&fty.to_field_dtype(), fname, w, types));
            }
            arms.push_str(&format!(
                "{n}::{vn}({pats}) => {{ {w}.write_u64({disc}); {field_stmts} }} ",
                vn=v.name, pats=fnames.join(", "),
            ));
        }
    }
    format!("match {expr} {{ {arms} }}")
}

fn gen_read_enum_decode(e: &EnumDecl, r: &str, types: &[TypeDecl]) -> String {
    let n = &e.name;
    let mut arms = String::new();
    for (i, v) in e.variants.iter().enumerate() {
        let disc = i as u64;
        if v.fields.is_empty() {
            arms.push_str(&format!("{disc} => {n}::{vn}, ", vn=v.name));
        } else {
            let fields: Vec<String> = v.fields.iter().map(|fty| gen_read_decode(&fty.to_field_dtype(), r, types)).collect();
            arms.push_str(&format!("{disc} => {n}::{vn}({fields}), ", vn=v.name, fields=fields.join(", ")));
        }
    }
    format!(
        "match {r}.read_u64() {{ {arms} _ => panic!(\"invalid discriminant for {n}\") }}"
    )
}

fn gen_read_enum_receive(e: &EnumDecl, r: &str, types: &[TypeDecl]) -> String {
    let n = &e.name;
    let mut arms = String::new();
    for (i, v) in e.variants.iter().enumerate() {
        let disc = i as u64;
        if v.fields.is_empty() {
            arms.push_str(&format!("{disc} => {n}::{vn}, ", vn=v.name));
        } else {
            let fields: Vec<String> = v.fields.iter().map(|fty| gen_read_receive(&fty.to_field_dtype(), r, types)).collect();
            arms.push_str(&format!("{disc} => {n}::{vn}({fields}), ", vn=v.name, fields=fields.join(", ")));
        }
    }
    format!(
        "match {r}.read_u64() {{ {arms} _ => panic!(\"invalid discriminant for {n}\") }}"
    )
}

/// Generate the Result<T, String> receive expression (host side).
fn gen_read_result_receive(ret: &FieldType, r: &str, types: &[TypeDecl]) -> String {
    let ok_recv = gen_read_receive(ret, r, types);
    let err_recv = gen_read_receive(&FieldType::String, r, types);
    format!(
        "{{ let __tag = {r}.read_u64(); if __tag == 0 {{ Ok({ok_recv}) }} else {{ Err({err_recv}) }} }}"
    )
}

/// Generate the inline write-to-out-slots epilogue.
fn gen_write_out_epilogue(w: &str, out_slots: &str, out_capacity: &str) -> String {
    format!(
        "let __n = {w}.len(); if __n > {out_capacity} {{ return 2; }} if __n > 0 {{ unsafe {{ core::ptr::copy_nonoverlapping({w}.as_slice().as_ptr(), {out_slots}, __n); }} }} 0"
    )
}

// ===========================================================================
// gen_trait
// ===========================================================================

fn gen_trait(iface: &Interface) -> String {
    let tn = trait_name(iface);
    let mut out = format!("pub trait {}: Send + Sync {{\n", tn);

    for m in &iface.methods {
        let params: Vec<String> = std::iter::once("&self".to_string())
            .chain(m.params.iter().map(|p| format!("{}: {}", p.name, p.ty.rust_input_type())))
            .collect();
        out.push_str(&format!(
            "    fn {}({}) -> Result<{}, String>;\n",
            m.name,
            params.join(", "),
            m.return_type.rust_output_type(),
        ));
    }
    out.push_str("}\n\n");
    out
}

// ===========================================================================
// gen_types — struct/enum/opaque definitions + slot impls + descriptors
// ===========================================================================

fn gen_types(iface: &Interface, ctx: &mut BuildContext) -> String {
    let mut out = String::new();

    for ty in &iface.types {
        let canonical = ty.canonical();
        if ctx.check_or_register(ty.name(), &canonical) {
            continue;
        }
        match ty {
            TypeDecl::Struct(s) => {
                if !s.fields.is_empty() {
                    out.push_str(&gen_struct_def(s));
                    if struct_has_dynamic_fields(s, &iface.types) {
                        out.push_str(&gen_drop_fn(s, &iface.types));
                    }
                }
            }
            TypeDecl::Enum(e) => {
                out.push_str(&gen_enum_def(e));
                if enum_has_dynamic_fields(e, &iface.types) {
                    out.push_str(&gen_enum_drop_fn(e, &iface.types));
                }
            }
            TypeDecl::Opaque(_) => {}
        }
    }
    out
}

fn gen_struct_def(s: &StructDecl) -> String {
    let mut out = format!(
        "#[repr(C)]\n#[derive(Clone, Debug, PartialEq)]\npub struct {} {{\n",
        s.name
    );
    for (fname, fty) in &s.fields {
        out.push_str(&format!("    pub {}: {},\n", fname, fty.rust_field_type()));
    }
    out.push_str("}\n\n");
    out
}

fn gen_enum_def(e: &EnumDecl) -> String {
    let has_fields = e.variants.iter().any(|v| !v.fields.is_empty());
    let repr = if has_fields { "#[repr(C, u32)]" } else { "#[repr(u32)]" };
    let mut out = format!(
        "{}\n#[derive(Clone, Debug, PartialEq)]\npub enum {} {{\n",
        repr, e.name
    );
    for v in &e.variants {
        if v.fields.is_empty() {
            out.push_str(&format!("    {},\n", v.name));
        } else {
            let fields: Vec<String> = v.fields.iter().map(|t| t.rust_field_type()).collect();
            out.push_str(&format!("    {}({}),\n", v.name, fields.join(", ")));
        }
    }
    out.push_str("}\n\n");
    out
}

// ===========================================================================
// Dynamic-field helpers — drop_fn generation for structs/enums
// ===========================================================================

fn field_has_dynamic(ft: &FieldType, types: &[TypeDecl]) -> bool {
    match ft {
        FieldType::DString | FieldType::DVec(_) => true,
        FieldType::DOption(inner) => field_has_dynamic(inner, types),
        FieldType::Named(name) => {
            if let Some(ty) = types.iter().find(|t| t.name() == name.as_str()) {
                match ty {
                    TypeDecl::Struct(s) => s.fields.iter().any(|(_, f)| field_has_dynamic(f, types)),
                    TypeDecl::Enum(e) => e.variants.iter().any(|v| v.fields.iter().any(|f| field_has_dynamic(f, types))),
                    TypeDecl::Opaque(_) => false,
                }
            } else {
                false
            }
        }
        _ => false,
    }
}

fn struct_has_dynamic_fields(s: &StructDecl, types: &[TypeDecl]) -> bool {
    s.fields.iter().any(|(_, f)| field_has_dynamic(f, types))
}

fn enum_has_dynamic_fields(e: &EnumDecl, types: &[TypeDecl]) -> bool {
    e.variants.iter().any(|v| v.fields.iter().any(|f| field_has_dynamic(f, types)))
}

/// Emit `dynspire_release(s.field.ptr)` lines for each dynamic field in a
/// struct, used inside the generated `drop_fn`.
fn gen_field_releases(prefix: &str, fields: &[(String, FieldType)], types: &[TypeDecl]) -> String {
    let mut out = String::new();
    for (fname, fty) in fields {
        match fty {
            FieldType::DString => {
                out.push_str(&format!(
                    "        dynspire::managed::dynspire_release({prefix}{fname}.ptr as *mut u8);\n"
                ));
            }
            FieldType::DVec(_inner) => {
                out.push_str(&format!(
                    "        dynspire::managed::dynspire_release({prefix}{fname}.ptr as *mut u8);\n"
                ));
            }
            FieldType::DOption(inner) => match inner.as_ref() {
                FieldType::DString => {
                    out.push_str(&format!(
                        "        if {prefix}{fname}.tag != 0 {{\n            dynspire::managed::dynspire_release({prefix}{fname}.value.ptr as *mut u8);\n        }}\n"
                    ));
                }
                FieldType::DVec(_inner2) => {
                    out.push_str(&format!(
                        "        if {prefix}{fname}.tag != 0 {{\n            dynspire::managed::dynspire_release({prefix}{fname}.value.ptr as *mut u8);\n        }}\n"
                    ));
                }
                _ => {}
            },
            FieldType::Named(name) => {
                if let Some(ty) = types.iter().find(|t| t.name() == name.as_str()) {
                    match ty {
                        TypeDecl::Struct(inner_s) if struct_has_dynamic_fields(inner_s, types) => {
                            out.push_str(&format!(
                                "        drop_{name}(&{prefix}{fname} as *const {name} as *mut core::ffi::c_void);\n"
                            ));
                        }
                        TypeDecl::Enum(inner_e) if enum_has_dynamic_fields(inner_e, types) => {
                            out.push_str(&format!(
                                "        drop_{name}(&{prefix}{fname} as *const {name} as *mut core::ffi::c_void);\n"
                            ));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn gen_drop_fn(s: &StructDecl, types: &[TypeDecl]) -> String {
    let name = &s.name;
    let releases = gen_field_releases("(*__s).", &s.fields, types);
    format!(
        "unsafe extern \"C\" fn drop_{name}(ptr: *mut core::ffi::c_void) {{\n\
         \x20   let __s = unsafe {{ &mut *(ptr as *mut {name}) }};\n\
         {releases}}}\n\n"
    )
}

fn gen_enum_drop_fn(e: &EnumDecl, types: &[TypeDecl]) -> String {
    let name = &e.name;
    let mut arms = String::new();
    for v in e.variants.iter() {
        if v.fields.is_empty() {
            continue;
        }
        let binds: Vec<String> = (0..v.fields.len()).map(|i| format!("__f{i}")).collect();
        let binds_str = binds.join(", ");
        let releases = gen_field_releases("", &v.fields.iter().enumerate().map(|(i, f)| (binds[i].clone(), f.clone())).collect::<Vec<_>>(), types);
        if releases.is_empty() {
            continue;
        }
        arms.push_str(&format!(
            "        {name}::{vn}({binds_str}) => {{\n{releases}        }}\n",
            vn = v.name,
        ));
    }
    if arms.is_empty() {
        return String::new();
    }
    format!(
        "unsafe extern \"C\" fn drop_{name}(ptr: *mut core::ffi::c_void) {{\n\
         \x20   let __s = unsafe {{ &mut *(ptr as *mut {name}) }};\n\
         \x20   match unsafe {{ core::ptr::read(__s) }} {{\n\
         {arms}\
         \x20   }}\n}}\n\n"
    )
}

// ===========================================================================
// gen_op_enum
// ===========================================================================
// gen_op_enum
// ===========================================================================

fn gen_op_enum(iface: &Interface) -> String {
    let opn = op_name(iface);
    let variants: Vec<String> = iface.methods.iter()
        .enumerate()
        .map(|(i, m)| format!("    {} = {},", pascal(&m.name), i))
        .collect();

    let from_u8_arms: Vec<String> = iface.methods.iter()
        .enumerate()
        .map(|(i, m)| format!("            {} => Some(Self::{}),", i, pascal(&m.name)))
        .collect();

    format!(
        r#"#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum {opn} {{
{variants}
}}

impl {opn} {{
    pub fn from_u8(v: u8) -> Option<Self> {{
        match v {{
{arms}
            _ => None,
        }}
    }}
}}

impl dynspire::SpierOp for {opn} {{
    fn as_index(self) -> usize {{ self as usize }}
}}

"#,
        opn=opn,
        variants=variants.join("\n"),
        arms=from_u8_arms.join("\n"),
    )
}

// ===========================================================================
// gen_tower — DynSpireRle client wrapper
// ===========================================================================

fn gen_tower(iface: &Interface) -> String {
    let cn = client_name(iface);
    let tn = trait_name(iface);
    let opn = op_name(iface);
    let types = &iface.types;

    let mut methods = String::new();
    for m in &iface.methods {
        let params: Vec<String> = std::iter::once("&self".to_string())
            .chain(m.params.iter().map(|p| format!("{}: {}", p.name, p.ty.rust_type())))
            .collect();

        let mut encode_stmts = String::new();
        let mut post_stmts = String::new();
        for p in &m.params {
            if matches!(p.ty, FieldType::OutU8Vec) {
                let n = &p.name;
                encode_stmts.push_str(&format!(
                    "        let mut __ov_{n}: dynspire::managed::DVec<u8> = dynspire::managed::DVec::<u8> {{ allocator: self.client.alloc_ptr(), ptr: std::ptr::null_mut(), len: 0, cap: 0 }};\n\
                     __w.write_u64(&mut __ov_{n} as *mut dynspire::managed::DVec<u8> as u64);\n",
                ));
                post_stmts.push_str(&format!(
                    "        if !__ov_{n}.ptr.is_null() {{\n\
                     let __bytes = unsafe {{ core::slice::from_raw_parts(__ov_{n}.ptr, __ov_{n}.len) }};\n\
                     {out}.extend_from_slice(__bytes);\n\
                     unsafe {{ dynspire::managed::dynspire_release(__ov_{n}.ptr); }}\n\
                     }}\n",
                    n = n, out = p.name,
                ));
            } else {
                encode_stmts.push_str(&gen_write_encode(&p.ty, &p.name, "__w", types));
            }
        }

        let result_recv = gen_read_result_receive(&m.return_type, "__r", types);

        methods.push_str(&format!(
            "    fn {name}({params}) -> Result<{ret}, String> {{\n\
             let mut __w = dynspire::slots::SlotWriter::new();\n\
             {encode_stmts}\
             let mut __out = [0u64; dynspire::slots::MAX_OUT_SLOTS];\n\
             self.client.dispatch({opn}::{pascal} as usize, __w.as_slice(), &mut __out)?;\n\
             let mut __r = dynspire::slots::SlotReader::new(&__out);\n\
             let __result = {result_recv};\n\
             {post_stmts}\
             __result\n\
             }}\n",
            name = m.name,
            params = params.join(", "),
            ret = m.return_type.rust_output_type(),
            opn = opn,
            pascal = pascal(&m.name),
            result_recv = result_recv,
            post_stmts = post_stmts,
        ));
    }

    format!(
        r#"pub struct {cn} {{
    client: dynspire::DynSpireClient,
}}

impl {cn} {{
    pub fn connect(spier_name: &str, config: &std::collections::HashMap<String, String>) -> Result<Self, String> {{
        Self::connect_with_debug(spier_name, config, false)
    }}

    pub fn connect_with_debug(spier_name: &str, config: &std::collections::HashMap<String, String>, debug: bool) -> Result<Self, String> {{
        let client = dynspire::DynSpireClient::connect(spier_name, &IDL, config, debug)?;
        Ok(Self {{ client }})
    }}

    /// Snapshot of the allocator's memory occupation (see
    /// `DynSpireClient::allocator_report`). Only meaningful when the client was
    /// created with `debug = true`.
    pub fn allocator_report(&self) -> dynspire::managed::DynSpireAllocatorReport {{
        self.client.allocator_report()
    }}

    /// Allocate an owning `DVec` in the host allocator (for passing owned
    /// buffers into spier methods that take a `DVec` parameter). Released when
    /// the returned guard is dropped.
    pub fn new_dvec<T: dynspire::managed::ReprC>(&self, cap: usize) -> dynspire::managed::OwnedDVec<T> {{
        dynspire::managed::OwnedDVec::new_in(unsafe {{ &*self.client.alloc_ptr() }}, cap)
    }}

    /// Allocate an owning `DString` in the host allocator. Released when the
    /// returned guard is dropped.
    pub fn new_dstring(&self, s: &str) -> dynspire::managed::OwnedDString {{
        dynspire::managed::OwnedDString::new_in(unsafe {{ &*self.client.alloc_ptr() }}, s)
    }}
}}

impl {tn} for {cn} {{
{methods}}}

"#,
        cn=cn, tn=tn, methods=methods,
    )
}

// ===========================================================================
// gen_spier_exports — module-scope C export: dynspire_idl_hash
//
// Owned returns are released by the host directly via `dynspire_release`
// (the allocation carries its own RC header + drop_fn), so no
// `dynspire_free`-style type_index dispatch is needed.
// ===========================================================================

fn gen_spier_exports(_iface: &Interface, hash: u64) -> String {
    format!(
        r#"#[no_mangle]
pub extern "C" fn dynspire_idl_hash() -> u64 {{
    {hash}
}}

"#,
        hash = hash,
    )
}

// ===========================================================================
// gen_spier_macro — slim macro: only dispatch + create/destroy/spier_name
// ===========================================================================

fn gen_spier_macro(iface: &Interface) -> String {
    let mn = spier_macro_name(iface);
    let tn = trait_name(iface);
    let types = &iface.types;

    let mut dispatch_fns = String::new();

    for m in &iface.methods {
        let fn_name = format!("dynspire_dispatch_{}", m.name);

        let mut post_call = String::new();
        let decode_block = if m.params.is_empty() {
            String::new()
        } else {
            let mut block = String::from(
                "            let __in_data = if !in_slots.is_null() && in_count > 0 {\n                unsafe { core::slice::from_raw_parts(in_slots, in_count) }\n            } else { &[] };\n            let mut __r = dynspire::slots::SlotReader::new(__in_data);\n",
            );
            for p in &m.params {
                if matches!(p.ty, FieldType::OutU8Vec) {
                    let n = &p.name;
                    block.push_str(&format!(
                        "            let __ov_{n}_dvec = __r.read_u64() as *mut dynspire::managed::DVec<u8>;\n\
                         let mut __ov_{n}_vec: Vec<u8> = Vec::new();\n\
                         let {n}: &mut Vec<u8> = &mut __ov_{n}_vec;\n",
                    ));
                    post_call.push_str(&format!(
                        "            if !__ov_{n}_dvec.is_null() {{\n\
                         let __dvec = &mut *(__ov_{n}_dvec as *mut dynspire::managed::DVec<u8>);\n\
                         let __n = __ov_{n}_vec.len();\n\
                         if __n > 0 {{\n\
                         let __new = dynspire::managed::dynspire_realloc(__dvec.allocator, __dvec.ptr, __dvec.cap, __n, 1);\n\
                         if !__new.is_null() {{\n\
                         core::ptr::copy_nonoverlapping(__ov_{n}_vec.as_ptr(), __new, __n);\n\
                         __dvec.ptr = __new; __dvec.len = __n; __dvec.cap = __n; }}\n\
                         }}\n\
                         }}\n",
                        n = n,
                    ));
                } else {
                    let decode_expr = gen_read_decode(&p.ty, "__r", types);
                    block.push_str(&format!(
                        "            let {}: {} = {decode_expr};\n",
                        p.name, p.ty.rust_type(),
                    ));
                }
            }
            block
        };

        let null_check = "            if state_handle.is_null() {\n\
                           let mut __w = dynspire::slots::SlotWriter::new();\n\
                           __w.write_u64(1);\n\
                           let __err = \"null handle\".to_string();\n\
                           if __err.is_empty() { __w.write_u64(0); __w.write_u64(0); } else {\n\
                           let __len = __err.len(); let __data = dynspire::managed::dynspire_alloc(&mut dynspire::managed::default_allocator() as *mut _, __len, 1);\n\
                           if __data.is_null() { __w.write_u64(0); __w.write_u64(0); } else {\n\
                           core::ptr::copy_nonoverlapping(__err.as_ptr(), __data, __len);\n\
                           __w.write_u64(__data as u64); __w.write_u64(__len as u64); }\n\
                           }\n\
                           let __n = __w.len(); if __n > out_capacity { return 2; }\n\
                           if __n > 0 { unsafe { core::ptr::copy_nonoverlapping(__w.as_slice().as_ptr(), out_slots, __n); } }\n\
                           return 0; }\n";

        let state_cast = "            let __spier_state = unsafe { &*(state_handle as *const __SpierState) };\n            let state = &__spier_state.inner;\n            #[allow(unused_variables)]\n            let __alloc = __spier_state.allocator;\n";

        let param_names: Vec<String> = std::iter::once("state".to_string())
            .chain(m.params.iter().map(|p| p.name.clone()))
            .collect();
        let call_args = format!("({})", param_names.join(", "));

        let guarded = m.return_type.is_guarded_return();
        let ok_var = if guarded { "__raw" } else { "__v" };
        let ok_return = gen_write_return(&m.return_type, ok_var, "__w", types);
        let err_return = gen_write_return(&FieldType::String, "__e", "__w", types);
        let write_out = gen_write_out_epilogue("__w", "out_slots", "out_capacity");

        let result_encode = if ok_return.is_empty() && m.return_type == FieldType::Unit {
            format!(
                "match _result {{\n\
                 Ok(__v) => {{ __w.write_u64(0); }}\n\
                 Err(__e) => {{ __w.write_u64(1); {err_return} }}\n\
                 }}\n"
            )
        } else if guarded {
            format!(
                "match _result {{\n\
                 Ok(__v) => {{ let __raw = __v.into_raw(); __w.write_u64(0); {ok_return} }}\n\
                 Err(__e) => {{ __w.write_u64(1); {err_return} }}\n\
                 }}\n"
            )
        } else {
            format!(
                "match _result {{\n\
                 Ok(__v) => {{ __w.write_u64(0); {ok_return} }}\n\
                 Err(__e) => {{ __w.write_u64(1); {err_return} }}\n\
                 }}\n"
            )
        };

        dispatch_fns.push_str(&format!(
            r#"        #[no_mangle]
        pub unsafe extern "C" fn {fn_name}(
            state_handle: *mut std::ffi::c_void,
            {in_slots_name}: *const u64,
            {in_count_name}: usize,
            out_slots: *mut u64,
            out_capacity: usize,
        ) -> u8 {{
{null_check}{state_cast}{decode}            let _result = <$state as $crate::{tn}>::{method}{call_args};
{post_call}            let mut __w = dynspire::slots::SlotWriter::new();
            {result_encode}            {write_out}
        }}
"#,
            fn_name=fn_name,
            in_slots_name=if m.params.is_empty() { "_in_slots" } else { "in_slots" },
            in_count_name=if m.params.is_empty() { "_in_count" } else { "in_count" },
            null_check=null_check,
            state_cast=state_cast,
            decode=decode_block,
            post_call=post_call,
            tn=tn,
            method=m.name,
            call_args=call_args,
        ));
    }

    format!(
        r#"#[macro_export]
macro_rules! {mn} {{
    ($state:ty, $init:path, $name:literal) => {{
{dispatch}
        struct __SpierState {{
            #[allow(dead_code)]
            allocator: *mut dynspire::managed::DynSpireAllocator,
            inner: $state,
        }}

        // Recover the host allocator from `&self` in a spier trait method.
        // `__SpierState` wraps `$state`; `state` (the `&self` the trait method
        // receives) points at `inner`, so we step back by `inner`'s offset.
        impl dynspire::managed::DynSpireStateExt for $state {{
            fn __dynspire_alloc(&self) -> *mut dynspire::managed::DynSpireAllocator {{
                let __off = ::core::mem::offset_of!(__SpierState, inner);
                let __p = unsafe {{ (self as *const $state as *const u8).sub(__off) as *const __SpierState }};
                unsafe {{ (*__p).allocator }}
            }}
        }}

        #[no_mangle]
        pub unsafe extern "C" fn dynspire_create(
            allocator: *mut dynspire::managed::DynSpireAllocator,
            data_ptr: *const u8,
            data_len: usize,
        ) -> *mut std::ffi::c_void {{
            let config = if data_ptr.is_null() || data_len == 0 {{
                std::collections::HashMap::new()
            }} else {{
                let data = unsafe {{ std::slice::from_raw_parts(data_ptr, data_len) }};
                dynspire::deserialize_kvmap(data)
            }};
            match $init(&config) {{
                Ok(inner) => {{
                    Box::into_raw(Box::new(__SpierState {{ allocator, inner }})) as *mut std::ffi::c_void
                }}
                Err(e) => {{
                    eprintln!("spier init failed: {{e}}");
                    std::ptr::null_mut()
                }}
            }}
        }}

        #[no_mangle]
        pub unsafe extern "C" fn dynspire_destroy(handle: *mut std::ffi::c_void) {{
            if !handle.is_null() {{
                unsafe {{ drop(Box::from_raw(handle as *mut __SpierState)); }}
            }}
        }}

        #[no_mangle]
        pub extern "C" fn dynspire_spier_name() -> *const u8 {{
            concat!($name, "\0").as_ptr()
        }}
    }};
}}
"#,
        mn=mn,
        dispatch=dispatch_fns,
    )
}

// ===========================================================================
// generate — orchestrate everything into one source string
// ===========================================================================

/// Generate a complete Rust source file from an [`Interface`] AST.
pub fn generate(iface: &Interface) -> String {
    generate_with_ctx(iface, &mut BuildContext::new())
}

/// Generate only the spier side: trait, types, op enum, hash, and spier macro.
pub fn generate_spier(iface: &Interface) -> String {
    generate_spier_with_ctx(iface, &mut BuildContext::new())
}

/// Generate only the host side: trait, types, op enum, hash, IDL, and tower.
pub fn generate_host(iface: &Interface) -> String {
    generate_host_with_ctx(iface, &mut BuildContext::new())
}

fn generate_with_ctx(iface: &Interface, ctx: &mut BuildContext) -> String {
    let mut out = generate_spier_with_ctx(iface, ctx);
    out.push_str(&generate_host_suffix(iface));
    out
}

fn generate_spier_with_ctx(iface: &Interface, ctx: &mut BuildContext) -> String {
    let hash = fnv1a_64(iface.canonical_sig().as_bytes());
    let hcn = hash_const_name(iface);

    let mut out = String::new();
    out.push_str("// Auto-generated by dynspire-codegen — do not edit.\n\n");

    out.push_str(&gen_trait(iface));
    out.push_str(&gen_types(iface, ctx));
    out.push_str(&gen_op_enum(iface));
    out.push_str(&format!("pub const {}: u64 = {};\n\n", hcn, hash));
    out.push_str(&gen_spier_exports(iface, hash));
    out.push_str(&gen_spier_macro(iface));

    out
}

fn generate_host_with_ctx(iface: &Interface, ctx: &mut BuildContext) -> String {
    let hash = fnv1a_64(iface.canonical_sig().as_bytes());
    let hcn = hash_const_name(iface);

    let mut out = String::new();
    out.push_str("// Auto-generated by dynspire-codegen — do not edit.\n\n");

    out.push_str(&gen_trait(iface));
    out.push_str(&gen_types(iface, ctx));
    out.push_str(&gen_op_enum(iface));
    out.push_str(&format!("pub const {}: u64 = {};\n\n", hcn, hash));
    out.push_str(&generate_host_suffix(iface));

    out
}

fn generate_host_suffix(iface: &Interface) -> String {
    let hcn = hash_const_name(iface);
    let mut out = String::new();
    out.push_str(&format!(
        "pub static IDL: dynspire::IdlDescriptor = dynspire::IdlDescriptor {{\n    hash: {},\n    methods: &[{}],\n}};\n\n",
        hcn,
        iface.methods.iter()
            .map(|m| format!("\"{}\"", m.name))
            .collect::<Vec<_>>()
            .join(", "),
    ));
    out.push_str(&gen_tower(iface));
    out
}

// ===========================================================================
// generate_python — emit a pure-Python ctypes client module
// ===========================================================================
//
// Mirrors the Rust host codec (`gen_write_encode` / `gen_read_receive`) but
// emits Python targeting the `dynspire._runtime` ctypes helpers. One compiler,
// two back-ends: the parser/AST/hash stay singular; only the emission has a
// Python twin, so there is no risk of hash drift.

/// Whether an owned return must be freed immediately after being copied into
/// Python (as opposed to boxed structs/opaques, which are freed lazily by the
/// `OpaqueHandle` wrapper on GC).
fn is_immediate_owned(ft: &FieldType, types: &[TypeDecl]) -> bool {
    use FieldType::*;
    match ft {
        String | Vec(_) => true,
        Option(inner) => is_immediate_owned(inner, types),
        Tuple(elems) => elems.iter().any(|e| is_immediate_owned(e, types)),
        Named(name) => match find_type(types, name) {
            TypeDecl::Enum(e) => e
                .variants
                .iter()
                .any(|v| v.fields.iter().any(|f| is_immediate_owned(f, types))),
            _ => false,
        },
        _ => false,
    }
}

// --- encode: Python value -> slot stream (mirrors gen_write_encode) ---

fn gen_py_write(ft: &FieldType, expr: &str, w: &str, types: &[TypeDecl], indent: usize) -> std::string::String {
    use FieldType::*;
    let pad = " ".repeat(indent);
    let inpad = " ".repeat(indent + 4);
    match ft {
        Unit => std::string::String::new(),
        Bool => format!("{pad}{w}.write_bool({expr})\n"),
        U8 => format!("{pad}{w}.write_u8({expr})\n"),
        U16 => format!("{pad}{w}.write_u16({expr})\n"),
        U32 => format!("{pad}{w}.write_u32({expr})\n"),
        U64 => format!("{pad}{w}.write_u64({expr})\n"),
        I8 => format!("{pad}{w}.write_i8({expr})\n"),
        I16 => format!("{pad}{w}.write_i16({expr})\n"),
        I32 => format!("{pad}{w}.write_i32({expr})\n"),
        I64 => format!("{pad}{w}.write_i64({expr})\n"),
        F32 => format!("{pad}{w}.write_f32({expr})\n"),
        F64 => format!("{pad}{w}.write_f64({expr})\n"),
        Str | String => format!("{pad}{w}.write_str({expr})\n"),
        U8Slice => format!("{pad}{w}.write_bytes({expr})\n"),
        OutU8Vec => unreachable!("OutU8Vec handled by gen_py_method"),
        Vec(inner) => {
            if matches!(inner.as_ref(), FieldType::U8) {
                format!("{pad}{w}.write_bytes({expr})\n")
            } else {
                panic!(
                    "dynspire-codegen: Vec<{}> input param is not supported in Python codegen",
                    inner.canonical()
                )
            }
        }
        DStr | DSlice(_) => format!(
            "{pad}{w}.write_u64(int({expr}.ptr));\n{pad}{w}.write_u64(int({expr}.len))\n"
        ),
        DString | DVec(_) => format!(
            "{pad}{w}.write_u64(int({expr}.allocator));\n{pad}{w}.write_u64(int({expr}.ptr));\n{pad}{w}.write_u64(int({expr}.len));\n{pad}{w}.write_u64(int({expr}.cap))\n"
        ),
        DOption(inner) => {
            let inner_w = gen_py_write(inner, expr, w, types, indent + 4);
            format!(
                "{pad}if {expr} is None:\n{inpad}{w}.write_u64(0)\n{pad}else:\n{inpad}{w}.write_u64(1)\n{inner_w}"
            )
        }
        Option(inner) => {
            let inner_w = gen_py_write(inner, expr, w, types, indent + 4);
            format!(
                "{pad}if {expr} is None:\n{inpad}{w}.write_u64(0)\n{pad}else:\n{inpad}{w}.write_u64(1)\n{inner_w}"
            )
        }
        Tuple(elems) => {
            let mut s = std::string::String::new();
            for (i, e) in elems.iter().enumerate() {
                s.push_str(&gen_py_write(e, &format!("{expr}[{i}]"), w, types, indent));
            }
            s
        }
        Array(inner, len) => {
            if matches!(inner.as_ref(), FieldType::U8) && *len % 8 == 0 {
                let mut s = std::string::String::new();
                for i in 0..*len / 8 {
                    let start = i * 8;
                    s.push_str(&format!(
                        "{pad}{w}.write_u64(int.from_bytes({expr}[{start}..{}], 'little'))\n",
                        start + 8
                    ));
                }
                s
            } else {
                let mut s = std::string::String::new();
                for i in 0..*len {
                    s.push_str(&gen_py_write(inner, &format!("{expr}[{i}]"), w, types, indent));
                }
                s
            }
        }
        Named(name) => match find_type(types, name) {
            TypeDecl::Struct(_) | TypeDecl::Opaque(_) => format!("{pad}{w}.write_opaque({expr})\n"),
            TypeDecl::Enum(e) => gen_py_enum_write(e, expr, w, types, indent),
        },
    }
}

fn gen_py_enum_write(e: &EnumDecl, expr: &str, w: &str, types: &[TypeDecl], indent: usize) -> String {
    let pad = " ".repeat(indent);
    let inpad = " ".repeat(indent + 4);
    let n = &e.name;
    let mut s = String::new();
    for (i, v) in e.variants.iter().enumerate() {
        let kw = if i == 0 { "if" } else { "elif" };
        s.push_str(&format!("{pad}{kw} {expr}.variant == '{vn}':\n", vn = v.name));
        s.push_str(&format!("{inpad}{w}.write_u64({i})\n"));
        for (fi, fty) in v.fields.iter().enumerate() {
            s.push_str(&gen_py_write(fty, &format!("{expr}.fields[{fi}]"), w, types, indent + 4));
        }
    }
    s.push_str(&format!(
        "{pad}else:\n{inpad}raise ValueError('invalid {n} variant: ' + repr({expr}.variant))\n"
    ));
    s
}

// --- decode: slot stream -> Python value (mirrors gen_read_receive) ---
//
// `gen_py_read_expr` yields a single Python expression (safe to inline). Enum
// and empty-struct returns need statement blocks and are handled by
// `gen_py_decode_block`.

fn gen_py_read_expr(ft: &FieldType, r: &str, types: &[TypeDecl]) -> std::string::String {
    use FieldType::*;
    match ft {
        Unit => "None".into(),
        Bool => format!("{r}.read_bool()"),
        U8 => format!("{r}.read_u8()"),
        U16 => format!("{r}.read_u16()"),
        U32 => format!("{r}.read_u32()"),
        U64 => format!("{r}.read_u64()"),
        I8 => format!("{r}.read_i8()"),
        I16 => format!("{r}.read_i16()"),
        I32 => format!("{r}.read_i32()"),
        I64 => format!("{r}.read_i64()"),
        F32 => format!("{r}.read_f32()"),
        F64 => format!("{r}.read_f64()"),
        Str | U8Slice => format!("{r}.read_bytes()"),
        String => format!("{r}.read_string()"),
        Vec(inner) => {
            if matches!(inner.as_ref(), FieldType::U8) {
                format!("{r}.read_bytes()")
            } else if matches!(inner.as_ref(), FieldType::String) {
                format!("{r}.read_vec_string()")
            } else if matches!(inner.as_ref(), FieldType::Vec(ref b) if matches!(b.as_ref(), FieldType::U8)) {
                format!("{r}.read_vec_bytes()")
            } else {
                panic!(
                    "dynspire-codegen: Vec<{}> return is not supported in Python codegen",
                    inner.canonical()
                )
            }
        }
        OutU8Vec => "None".into(),
        Option(inner) => format!("None if {r}.read() == 0 else {}", gen_py_read_expr(inner, r, types)),
        Tuple(elems) => {
            let parts: std::vec::Vec<std::string::String> = elems.iter().map(|e| gen_py_read_expr(e, r, types)).collect();
            format!("({})", parts.join(", "))
        }
        Array(inner, len) => {
            if matches!(inner.as_ref(), FieldType::U8) && *len % 8 == 0 {
                let parts: std::vec::Vec<std::string::String> = (0..*len / 8)
                    .map(|_| format!("{r}.read_u64().to_bytes(8, 'little')"))
                    .collect();
                format!("b''.join([{parts}])", parts = parts.join(", "))
            } else {
                let parts: std::vec::Vec<std::string::String> = (0..*len)
                    .map(|_| gen_py_read_expr(inner, r, types))
                    .collect();
                format!("[{}]", parts.join(", "))
            }
        }
        DStr => format!(
            "(lambda __p, __n: ctypes.string_at(__p, __n).decode('utf-8', 'replace') if __p else \"\")({r}.read(), {r}.read())"
        ),
        DSlice(_) => format!(
            "(lambda __p, __n: ctypes.string_at(__p, __n) if __p else b\"\")({r}.read(), {r}.read())"
        ),
        DString => format!(
            "(lambda __a, __p, __n, __c: DStringHandle(self, __p, __n, __a))({r}.read(), {r}.read(), {r}.read(), {r}.read())"
        ),
        DVec(_) => format!(
            "(lambda __a, __p, __n, __c: OwnedDVec(self, __p, __n, __a))({r}.read(), {r}.read(), {r}.read(), {r}.read())"
        ),
        DOption(inner) => format!("None if {r}.read() == 0 else {}", gen_py_read_expr(inner, r, types)),
        Named(name) => match find_type(types, name) {
            TypeDecl::Struct(s) if s.fields.is_empty() => {
                panic!("dynspire-codegen: empty struct '{name}' return is not supported in Python codegen")
            }
            TypeDecl::Struct(_) | TypeDecl::Opaque(_) => {
                format!("{name}._from_ptr(self, {r}.read())")
            }
            TypeDecl::Enum(_) => panic!("dynspire-codegen: enum decode must go through gen_py_decode_block"),
        },
    }
}

/// Statements (at `indent`) that decode the Ok payload into a local `_v`.
/// Empty for `Unit` (caller returns `None` directly). Owned leaf values
/// (strings/bytes/vecs) append their allocation pointer to the reader's
/// `_release_list`; the caller releases them via `release_owned()` once the
/// Python value has been copied out. Opaque handles manage their own release
/// on `__del__`.
fn gen_py_decode_block(ft: &FieldType, r: &str, types: &[TypeDecl], indent: usize) -> String {
    let pad = " ".repeat(indent);
    match ft {
        FieldType::Unit => String::new(),
        FieldType::Named(name) => match find_type(types, name) {
            TypeDecl::Enum(e) => gen_py_enum_read_block(e, r, types, indent),
            TypeDecl::Struct(_) | TypeDecl::Opaque(_) => {
                format!("{pad}_v = {name}._from_ptr(self, {r}.read())\n")
            }
        },
        FieldType::Option(inner) => {
            let inpad = " ".repeat(indent + 4);
            let mut s = format!("{pad}_opt = {r}.read()\n");
            s.push_str(&format!("{pad}if _opt == 0:\n"));
            s.push_str(&format!("{inpad}_v = None\n"));
            s.push_str(&format!("{pad}else:\n"));
            s.push_str(&gen_py_decode_block(inner, r, types, indent + 4));
            s
        }
        _ => format!("{pad}_v = {}\n", gen_py_read_expr(ft, r, types)),
    }
}

fn gen_py_enum_read_block(e: &EnumDecl, r: &str, _types: &[TypeDecl], indent: usize) -> String {
    let pad = " ".repeat(indent);
    let inpad = " ".repeat(indent + 4);
    let n = &e.name;
    let mut s = format!("{pad}_disc = {r}.read()\n");
    for (i, v) in e.variants.iter().enumerate() {
        let kw = if i == 0 { "if" } else { "elif" };
        s.push_str(&format!("{pad}{kw} _disc == {i}:\n"));
        if v.fields.is_empty() {
            s.push_str(&format!("{inpad}_v = {n}('{vn}')\n", vn = v.name));
        } else {
            let args: Vec<String> = v.fields.iter().map(|fty| gen_py_read_expr(fty, r, _types)).collect();
            s.push_str(&format!("{inpad}_v = {n}('{vn}', {args})\n", vn = v.name, args = args.join(", ")));
        }
    }
    s.push_str(&format!(
        "{pad}else:\n{inpad}raise DynSpireError('invalid {n} discriminant ' + str(_disc))\n"
    ));
    s
}

// --- type classes ---

fn gen_py_types(iface: &Interface) -> String {
    let mut s = String::from("# --- Types ---\n\n");
    for ty in &iface.types {
        match ty {
            TypeDecl::Struct(st) => {
                if st.fields.is_empty() {
                    s.push_str(&format!("class {name}(OpaqueHandle):\n    pass\n\n\n", name = st.name));
                } else {
                    s.push_str(&gen_py_struct_class(st, &iface.types));
                }
            }
            TypeDecl::Opaque(o) => {
                s.push_str(&format!("class {name}(OpaqueHandle):\n    pass\n\n\n", name = o.name));
            }
            TypeDecl::Enum(e) => s.push_str(&gen_py_enum_class(e)),
        }
    }
    s
}

/// Map a FieldType to a Python ctypes field expression for `_fields_`.
#[allow(clippy::only_used_in_recursion)]
fn py_ctypes_field(ft: &FieldType, types: &[TypeDecl]) -> String {
    use FieldType::*;
    match ft {
        Bool => "ctypes.c_uint8".into(),
        U8 => "ctypes.c_uint8".into(),
        U16 => "ctypes.c_uint16".into(),
        U32 => "ctypes.c_uint32".into(),
        U64 => "ctypes.c_uint64".into(),
        I8 => "ctypes.c_int8".into(),
        I16 => "ctypes.c_int16".into(),
        I32 => "ctypes.c_int32".into(),
        I64 => "ctypes.c_int64".into(),
        F32 => "ctypes.c_float".into(),
        F64 => "ctypes.c_double".into(),
        String | DString => "DString".into(),
        Vec(_) | DVec(_) => "DVec".into(),
        Str | U8Slice | DStr | DSlice(_) => "DStr".into(),
        DOption(_) => "DOption".into(),
        Option(_) => "DOption".into(),
        Named(name) => format!("{name}Ctypes"),
        Tuple(elems) => {
            let inner: std::vec::Vec<std::string::String> = elems.iter().map(|e| py_ctypes_field(e, types)).collect();
            format!("({})", inner.join(", "))
        }
        Array(inner, len) => {
            let ct = py_ctypes_field(inner, types);
            format!("({ct} * {len})")
        }
        _ => "ctypes.c_uint64".into(),
    }
}

/// Map a FieldType to a Python default-value expression for `__init__`.
fn py_default_value(ft: &FieldType) -> String {
    use FieldType::*;
    match ft {
        Bool | U8 | U16 | U32 | U64 | I8 | I16 | I32 | I64 => "0".into(),
        F32 | F64 => "0.0".into(),
        String | DString | Str | U8Slice | DStr | DSlice(_) | Vec(_) | DVec(_) => "None".into(),
        Option(_) | DOption(_) => "None".into(),
        Named(name) => format!("{name}._default()"),
        Tuple(_) => "None".into(),
        Array(_, _) => "None".into(),
        Unit => "None".into(),
        OutU8Vec => "None".into(),
    }
}

/// Generate a Python class for an IDL struct with ctypes mirror + field accessors.
fn gen_py_struct_class(s: &StructDecl, types: &[TypeDecl]) -> String {
    let n = &s.name;
    let mut out = String::new();

    // --- ctypes mirror class ---
    out.push_str(&format!("class {n}Ctypes(ctypes.Structure):\n"));
    out.push_str("    _fields_ = [\n");
    for (fname, fty) in &s.fields {
        let ct = py_ctypes_field(fty, types);
        out.push_str(&format!("        ('{fname}', {ct}),\n"));
    }
    out.push_str("    ]\n\n\n");

    // --- wrapper class ---
    let slots: Vec<String> = s.fields.iter().map(|(f, _)| format!("'_{f}'")).collect();
    let mut all_slots = slots.clone();
    all_slots.push("'_client'".into());
    all_slots.push("'_ptr'".into());
    all_slots.push("'_cbuf'".into());
    out.push_str(&format!("class {n}(OpaqueHandle):\n"));
    out.push_str(&format!("    __slots__ = ({})\n\n", all_slots.join(", ")));

    // __init__
    let init_params: Vec<String> = s.fields.iter().map(|(f, _)| f.clone()).collect();
    out.push_str(&format!("    def __init__(self, {}):\n", init_params.join(", ")));
    out.push_str("        self._client = None\n");
    out.push_str("        self._cbuf = None\n");
    out.push_str("        self._ptr = 0\n");
    for (fname, fty) in &s.fields {
        let default = py_default_value(fty);
        out.push_str(&format!("        self._{fname} = {fname} if {fname} is not None else {default}\n"));
    }
    // build ctypes buffer from field values
    let ctypes_args: Vec<String> = s.fields.iter().map(|(f, _)| format!("self._{f}")).collect();
    out.push_str(&format!("        self._cbuf = {n}Ctypes({})\n", ctypes_args.join(", ")));
    out.push_str("        self._ptr = ctypes.addressof(self._cbuf)\n");
    out.push('\n');

    // _from_ptr classmethod
    out.push_str("    @classmethod\n");
    out.push_str("    def _from_ptr(cls, client, ptr):\n");
    out.push_str(&format!("        buf = {n}Ctypes()\n"));
    out.push_str("        ctypes.memmove(ctypes.addressof(buf), ptr, ctypes.sizeof(buf))\n");
    let field_reads: Vec<String> = s.fields.iter().map(|(f, _)| format!("buf.{f}")).collect();
    out.push_str(&format!("        obj = cls({})\n", field_reads.join(", ")));
    out.push_str("        obj._client = client\n");
    out.push_str("        obj._ptr = ptr\n");
    out.push_str("        return obj\n\n");

    // _default classmethod (for nested struct defaults)
    let defaults: Vec<String> = s.fields.iter().map(|(f, fty)| {
        let d = py_default_value(fty);
        format!("{f}={d}")
    }).collect();
    out.push_str("    @classmethod\n");
    out.push_str("    def _default(cls):\n");
    out.push_str(&format!("        return cls({})\n\n", defaults.join(", ")));

    // @property accessors
    for (fname, _fty) in &s.fields {
        out.push_str("    @property\n");
        out.push_str(&format!("    def {fname}(self):\n"));
        out.push_str(&format!("        return self._{fname}\n\n"));
    }

    // __repr__
    let repr_parts: Vec<String> = s.fields.iter().map(|(f, _)| {
        format!("'{f}=' + repr(self._{f})")
    }).collect();
    out.push_str("    def __repr__(self):\n");
    out.push_str(&format!("        return '{n}(' + {} + ')'\n\n", repr_parts.join(" + ', ' + ")));

    // __eq__
    let eq_parts: Vec<String> = s.fields.iter().map(|(f, _)| {
        format!("self._{f} == other._{f}")
    }).collect();
    out.push_str("    def __eq__(self, other):\n");
    out.push_str(&format!("        return isinstance(other, {n}) and {}\n\n", eq_parts.join(" and ")));

    // __hash__
    let hash_fields: Vec<String> = s.fields.iter().map(|(f, _)| format!("self._{f}")).collect();
    out.push_str("    def __hash__(self):\n");
    out.push_str(&format!("        return hash(({},))\n\n", hash_fields.join(", ")));

    out
}

fn gen_py_enum_class(e: &EnumDecl) -> String {
    let n = &e.name;
    let mut s = format!("class {n}:\n");
    s.push_str("    __slots__ = ('variant', 'fields')\n\n");
    s.push_str("    def __init__(self, variant, *fields):\n");
    s.push_str("        self.variant = variant\n");
    s.push_str("        self.fields = fields\n\n");
    s.push_str("    def __eq__(self, other):\n");
    s.push_str(&format!("        return isinstance(other, {n}) and self.variant == other.variant and self.fields == other.fields\n\n"));
    s.push_str("    def __hash__(self):\n");
    s.push_str("        return hash((self.variant, self.fields))\n\n");
    s.push_str("    def __repr__(self):\n");
    s.push_str("        if self.fields:\n");
    s.push_str("            inner = ', '.join(repr(f) for f in self.fields)\n");
    s.push_str(&format!("            return '{n}.' + self.variant + '(' + inner + ')'\n"));
    s.push_str(&format!("        return '{n}.' + self.variant\n\n"));
    for v in &e.variants {
        if v.fields.is_empty() {
            s.push_str(&format!("    @classmethod\n    def {vn}(cls):\n        return cls('{vn}')\n\n", vn = v.name));
        } else {
            let args: Vec<String> = (0..v.fields.len()).map(|i| format!("_f{i}")).collect();
            let pass = args.join(", ");
            s.push_str(&format!(
                "    @classmethod\n    def {vn}(cls, {args}):\n        return cls('{vn}', {pass})\n\n",
                vn = v.name,
                args = pass,
                pass = pass
            ));
        }
    }
    s.push('\n');
    s
}

// --- client class ---

fn gen_py_client(iface: &Interface) -> String {
    let cn = &iface.name;
    let mut methods = String::new();
    for m in &iface.methods {
        methods.push_str(&gen_py_method(m, &iface.types));
    }
    format!(
        "# --- Client ---\n\nclass {cn}(SpierClient):\n    _spier_name = '{name}'\n    _idl_hash_const = _IDL_HASH\n\n{methods}",
        cn = cn,
        name = iface.name.to_lowercase(),
        methods = methods
    )
}

fn gen_py_method(m: &Method, types: &[TypeDecl]) -> String {
    let inpad = "        ";
    let mut s = String::new();

    let user_params: Vec<&str> = m
        .params
        .iter()
        .filter(|p| !matches!(p.ty, FieldType::OutU8Vec))
        .map(|p| p.name.as_str())
        .collect();
    let sig = if user_params.is_empty() {
        String::new()
    } else {
        format!(", {}", user_params.join(", "))
    };
    s.push_str(&format!("    def {}(self{}):\n", m.name, sig));
    s.push_str(&format!("{inpad}_w = SlotWriter()\n"));

    let mut ov_count = 0usize;
    for p in &m.params {
        match &p.ty {
            FieldType::OutU8Vec => {
                s.push_str(&format!("{inpad}_ov{ov_count} = self._new_outvec()\n"));
                s.push_str(&format!("{inpad}_w.write_u64(_ov{ov_count})\n"));
                ov_count += 1;
            }
            _ => s.push_str(&gen_py_write(&p.ty, &p.name, "_w", types, 8)),
        }
    }

    s.push_str(&format!("{inpad}_out = new_out_array()\n"));
    s.push_str(&format!("{inpad}self._dispatch('{}', _w, _out)\n", m.name));
    s.push_str(&format!("{inpad}_r = OutReader(_out, self)\n"));
    s.push_str(&format!("{inpad}_tag = _r.read()\n"));
    s.push_str(&format!("{inpad}if _tag == 1:\n"));
    s.push_str(&format!("{inpad}    _err = _r.read_string()\n"));
    s.push_str(&format!("{inpad}    _r.release_owned()\n"));
    s.push_str(&format!("{inpad}    raise DynSpireError(_err)\n"));

    let is_unit = matches!(m.return_type, FieldType::Unit);
    let owned = is_immediate_owned(&m.return_type, types);

    if !is_unit {
        s.push_str(&gen_py_decode_block(&m.return_type, "_r", types, 8));
        if owned {
            s.push_str(&format!("{inpad}_r.release_owned()\n"));
        }
    }

    if ov_count > 0 {
        let lst: Vec<String> = (0..ov_count).map(|i| format!("self._read_outvec({i})")).collect();
        s.push_str(&format!("{inpad}_outs = [{}]\n", lst.join(", ")));
        s.push_str(&format!("{inpad}self._outvecs = []\n"));
        if is_unit {
            s.push_str(&format!("{inpad}return None, _outs\n"));
        } else {
            s.push_str(&format!("{inpad}return _v, _outs\n"));
        }
    } else if is_unit {
        s.push_str(&format!("{inpad}return None\n"));
    } else {
        s.push_str(&format!("{inpad}return _v\n"));
    }
    s.push('\n');
    s
}

/// Inlined ctypes runtime — emitted verbatim into every generated .py so the
/// module is fully self-contained (only depends on the Python stdlib).
const PY_RUNTIME: &str = r#"import ctypes
import struct

MAX_OUT_SLOTS = 8


class DynSpireError(RuntimeError):
    pass


class DVecU8(ctypes.Structure):
    _fields_ = [
        ("allocator", ctypes.c_void_p),
        ("ptr", ctypes.c_void_p),
        ("len", ctypes.c_size_t),
        ("cap", ctypes.c_size_t),
    ]


class DynSpireAllocatorReport(ctypes.Structure):
    _fields_ = [
        ("live_bytes", ctypes.c_size_t),
        ("live_allocations", ctypes.c_size_t),
        ("peak_bytes", ctypes.c_size_t),
        ("total_allocations", ctypes.c_size_t),
    ]

    def __repr__(self):
        return (
            "AllocatorReport(live_bytes={}, live_allocations={}, "
            "peak_bytes={}, total_allocations={})"
        ).format(
            self.live_bytes, self.live_allocations,
            self.peak_bytes, self.total_allocations,
        )


class DStr(ctypes.Structure):
    _fields_ = [
        ("ptr", ctypes.c_void_p),
        ("len", ctypes.c_size_t),
    ]


class DSlice(ctypes.Structure):
    _fields_ = [
        ("ptr", ctypes.c_void_p),
        ("len", ctypes.c_size_t),
    ]


class DString(ctypes.Structure):
    _fields_ = [
        ("allocator", ctypes.c_void_p),
        ("ptr", ctypes.c_void_p),
        ("len", ctypes.c_size_t),
        ("cap", ctypes.c_size_t),
    ]


class DVec(ctypes.Structure):
    _fields_ = [
        ("allocator", ctypes.c_void_p),
        ("ptr", ctypes.c_void_p),
        ("len", ctypes.c_size_t),
        ("cap", ctypes.c_size_t),
    ]


class DOption(ctypes.Structure):
    _fields_ = [
        ("tag", ctypes.c_uint8),
        ("_pad", ctypes.c_uint8 * 7),
        ("value", ctypes.c_uint64),
    ]


class OwnedDVec:
    """Owning view of a `DVec<u8>` returned/crated across the FFI boundary.

    Holds the raw pointer; releases it on `__del__`. Provides zero-copy
    access via `as_bytes()` / `memoryview`.
    """

    __slots__ = ("_client", "allocator", "ptr", "len", "cap")

    def __init__(self, client, ptr, length, alloc, cap=None):
        self._client = client
        self.allocator = alloc
        self.ptr = ptr
        self.len = length
        self.cap = cap if cap is not None else length

    def as_bytes(self):
        if not self.ptr:
            return b""
        return ctypes.string_at(self.ptr, int(self.len))

    def __len__(self):
        return int(self.len)

    def __del__(self):
        try:
            if self.ptr:
                self._client._release_fn(self.ptr)
        except Exception:
            pass


class DStringHandle:
    """Owning view of a `DString` returned across the FFI boundary."""

    __slots__ = ("_client", "allocator", "ptr", "len", "cap")

    def __init__(self, client, ptr, length, alloc, cap=None):
        self._client = client
        self.allocator = alloc
        self.ptr = ptr
        self.len = length
        self.cap = cap if cap is not None else length

    def as_str(self):
        if not self.ptr:
            return ""
        return ctypes.string_at(self.ptr, int(self.len)).decode("utf-8", "replace")

    def __len__(self):
        return int(self.len)

    def __del__(self):
        try:
            if self.ptr:
                self._client._release_fn(self.ptr)
        except Exception:
            pass


class SlotWriter:
    def __init__(self):
        self._vals = []
        self._keepalive = []

    def write_u64(self, v):
        self._vals.append(v & 0xFFFFFFFFFFFFFFFF)

    def write_bool(self, b):
        self.write_u64(1 if b else 0)

    def write_u8(self, v):
        self.write_u64(v)

    def write_u16(self, v):
        self.write_u64(v)

    def write_u32(self, v):
        self.write_u64(v)

    def write_i8(self, v):
        self.write_u64(v & 0xFF)

    def write_i16(self, v):
        self.write_u64(v & 0xFFFF)

    def write_i32(self, v):
        self.write_u64(v & 0xFFFFFFFF)

    def write_i64(self, v):
        self.write_u64(v & 0xFFFFFFFFFFFFFFFF)

    def write_f32(self, v):
        self.write_u64(struct.unpack("<I", struct.pack("<f", v))[0])

    def write_f64(self, v):
        self.write_u64(struct.unpack("<Q", struct.pack("<d", v))[0])

    def write_bytes(self, data):
        ptr, n = self._pin_bytes(data)
        self.write_u64(ptr)
        self.write_u64(n)

    def write_str(self, s):
        if isinstance(s, str):
            self.write_bytes(s.encode("utf-8"))
        else:
            self.write_bytes(s)

    def write_opaque(self, handle):
        self.write_u64(handle._ptr)

    def _pin_bytes(self, data):
        if not isinstance(data, (bytes, bytearray)):
            raise TypeError("expected bytes, got " + type(data).__name__)
        n = len(data)
        if n == 0:
            return 0, 0
        arr = (ctypes.c_uint8 * n).from_buffer_copy(bytes(data))
        self._keepalive.append(arr)
        return ctypes.addressof(arr), n

    def array(self):
        if not self._vals:
            return None
        return (ctypes.c_uint64 * len(self._vals))(*self._vals)

    def __len__(self):
        return len(self._vals)


class OutReader:
    def __init__(self, arr, client=None):
        self._arr = arr
        self._pos = 0
        self._client = client
        self._release_list = []

    def read(self):
        v = self._arr[self._pos]
        self._pos += 1
        return v

    def pos(self):
        return self._pos

    def read_bool(self):
        return self.read() != 0

    def read_u8(self):
        return self.read() & 0xFF

    def read_u16(self):
        return self.read() & 0xFFFF

    def read_u32(self):
        return self.read() & 0xFFFFFFFF

    def read_u64(self):
        return self.read()

    def read_i8(self):
        v = self.read() & 0xFF
        return v - 0x100 if v >= 0x80 else v

    def read_i16(self):
        v = self.read() & 0xFFFF
        return v - 0x10000 if v >= 0x8000 else v

    def read_i32(self):
        v = self.read() & 0xFFFFFFFF
        return v - 0x100000000 if v >= 0x80000000 else v

    def read_i64(self):
        v = self.read()
        return v - 0x10000000000000000 if v >= 0x8000000000000000 else v

    def read_f32(self):
        return struct.unpack("<f", struct.pack("<I", self.read() & 0xFFFFFFFF))[0]

    def read_f64(self):
        return struct.unpack("<d", struct.pack("<Q", self.read()))[0]

    def read_string(self):
        ptr, n = self.read(), self.read()
        if not ptr or not n:
            return ""
        if ptr:
            self._release_list.append(ptr)
        return ctypes.string_at(ptr, n).decode("utf-8", "replace")

    def read_bytes(self):
        ptr, n = self.read(), self.read()
        if not ptr or not n:
            return b""
        if ptr:
            self._release_list.append(ptr)
        return ctypes.string_at(ptr, n)

    def read_vec_string(self):
        ptr, n = self.read(), self.read()
        if not ptr or not n:
            return []
        self._release_list.append(ptr)
        out = []
        for i in range(n):
            view = self._client._vec_view_at_fn(ptr, i)
            out.append("" if not view.len else ctypes.string_at(view.ptr, view.len).decode("utf-8", "replace"))
        return out

    def read_vec_bytes(self):
        ptr, n = self.read(), self.read()
        if not ptr or not n:
            return []
        self._release_list.append(ptr)
        out = []
        for i in range(n):
            view = self._client._vec_view_at_fn(ptr, i)
            out.append(b"" if not view.len else ctypes.string_at(view.ptr, view.len))
        return out

    def release_owned(self):
        for p in self._release_list:
            if p:
                self._client._release_fn(p)
        self._release_list = []


def new_out_array():
    return (ctypes.c_uint64 * MAX_OUT_SLOTS)()


class OpaqueHandle:
    __slots__ = ("_client", "_ptr", "__weakref__")

    def __init__(self, client, ptr):
        self._client = client
        self._ptr = ptr

    @property
    def type_name(self):
        return type(self).__name__

    def __repr__(self):
        return type(self).__name__ + "(ptr=0x" + format(self._ptr, "x") + ")"

    def __del__(self):
        try:
            if self._ptr:
                self._client._release_fn(self._ptr)
        except Exception:
            pass


class SpierClient:
    _spier_name = None
    _idl_hash_const = 0

    def __init__(self, lib_path, config=None, debug=False):
        self._lib = ctypes.CDLL(lib_path)
        self._configure_symbols()
        actual = self._idl_hash_fn()
        expected = self._idl_hash_const
        if actual != expected:
            raise DynSpireError(
                "IDL hash mismatch: host expects 0x{:016x}, spier '{}' has 0x{:016x}".format(expected, self._spier_name, actual)
            )
        cfg = _encode_config(config)
        self._alloc_ptr = self._debug_alloc_fn() if debug else self._alloc_fn()
        self._handle = self._create_fn(self._alloc_ptr, cfg, len(cfg))
        if not self._handle:
            raise DynSpireError("spier '{}' create failed".format(self._spier_name))
        self._closed = False
        self._dispatch_cache = {}
        self._outvecs = []

    def _configure_symbols(self):
        f = self._lib.dynspire_default_allocator
        f.restype = ctypes.c_void_p
        f.argtypes = []
        self._alloc_fn = f

        f = self._lib.dynspire_alloc
        f.restype = ctypes.c_void_p
        f.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_size_t]
        self._raw_alloc_fn = f

        f = self._lib.dynspire_debug_allocator
        f.restype = ctypes.c_void_p
        f.argtypes = []
        self._debug_alloc_fn = f

        f = self._lib.dynspire_allocator_report
        f.restype = DynSpireAllocatorReport
        f.argtypes = [ctypes.c_void_p]
        self._report_fn = f

        f = self._lib.dynspire_create
        f.restype = ctypes.c_void_p
        f.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_size_t]
        self._create_fn = f

        f = self._lib.dynspire_destroy
        f.argtypes = [ctypes.c_void_p]
        self._destroy_fn = f

        f = self._lib.dynspire_idl_hash
        f.restype = ctypes.c_uint64
        self._idl_hash_fn = f

        f = self._lib.dynspire_release
        f.argtypes = [ctypes.c_void_p]
        f.restype = None
        self._release_fn = f

    def _dispatch_fn(self, method):
        fn = self._dispatch_cache.get(method)
        if fn is None:
            fn = getattr(self._lib, "dynspire_dispatch_" + method)
            fn.restype = ctypes.c_uint8
            fn.argtypes = [
                ctypes.c_void_p,
                ctypes.c_void_p,
                ctypes.c_size_t,
                ctypes.POINTER(ctypes.c_uint64),
                ctypes.c_size_t,
            ]
            self._dispatch_cache[method] = fn
        return fn

    def _dispatch(self, method, writer, out):
        in_arr = writer.array()
        in_ptr = ctypes.cast(in_arr, ctypes.c_void_p) if in_arr is not None else None
        status = self._dispatch_fn(method)(self._handle, in_ptr, len(writer), out, MAX_OUT_SLOTS)
        if status == 2:
            raise DynSpireError("out-slot overflow dispatching '" + method + "'")
        if status != 0:
            raise DynSpireError("dispatch '" + method + "' failed (status " + str(status) + ")")

    def _new_outvec(self):
        d = DVecU8()
        d.allocator = self._alloc_ptr
        d.ptr = None
        d.len = ctypes.c_size_t(0)
        d.cap = ctypes.c_size_t(0)
        self._outvecs.append(d)
        return ctypes.addressof(d)

    def _read_outvec(self, i):
        d = self._outvecs[i]
        if not d.ptr:
            return b""
        n = int(d.len)
        arr_t = ctypes.c_ubyte * n
        data = bytes(ctypes.cast(d.ptr, ctypes.POINTER(arr_t))[0])
        self._release_fn(d.ptr)
        return data

    def allocator_report(self):
        """Snapshot of the allocator's memory occupation.

        Only meaningful when the client was created with debug=True; otherwise
        returns all zeros.
        """
        return self._report_fn(self._alloc_ptr)

    def _alloc(self, size, align):
        return self._raw_alloc_fn(self._alloc_ptr, size, align)

    def new_dvec(self, cap):
        """Allocate an owning `DVec<u8>` in the host allocator (for passing
        owned buffers into spier methods that take a `DVec` parameter)."""
        ptr = self._alloc(cap, 1) if cap > 0 else None
        return OwnedDVec(self, ptr, 0, self._alloc_ptr, cap)

    def new_dstring(self, s):
        """Allocate an owning `DString` in the host allocator."""
        b = s.encode("utf-8") if isinstance(s, str) else bytes(s)
        n = len(b)
        ptr = self._alloc(n, 1) if n > 0 else None
        if ptr:
            arr = (ctypes.c_uint8 * n).from_buffer_copy(b)
            ctypes.memmove(ptr, ctypes.addressof(arr), n)
        return DStringHandle(self, ptr, n, self._alloc_ptr, n)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False

    def close(self):
        if getattr(self, "_closed", True):
            return
        self._closed = True
        h = getattr(self, "_handle", None)
        if h:
            self._handle = None
            self._destroy_fn(h)

    def __del__(self):
        self.close()


def _encode_config(config):
    if not config:
        return b""
    import urllib.parse
    return urllib.parse.urlencode(config).encode("utf-8")
"#;

/// Generate a complete pure-Python ctypes client module from an [`Interface`].
pub fn generate_python(iface: &Interface) -> String {
    generate_python_with_ctx(iface, &mut BuildContext::new())
}

fn generate_python_with_ctx(iface: &Interface, _ctx: &mut BuildContext) -> String {
    let hash = fnv1a_64(iface.canonical_sig().as_bytes());

    let mut out = String::new();
    out.push_str("# Auto-generated by dynspire-codegen. Do not edit.\n");
    out.push_str("from __future__ import annotations\n\n");
    out.push_str(PY_RUNTIME);
    out.push_str("\n\n");
    out.push_str(&format!("_IDL_HASH = 0x{:016x}\n\n\n", hash));
    out.push_str(&gen_py_types(iface));
    out.push_str(&gen_py_client(iface));
    out.push('\n');
    out
}

/// Entry point for `build.rs`.
///
/// Reads the `.dspi` file, resolves includes, validates, generates Rust
/// source, and writes to `OUT_DIR`.
///
/// For multiple interfaces that may share types, use [`BuildContext::build`]
/// instead to deduplicate type definitions.
/// Recursively resolve `include` directives, collecting types from fragments.
///
/// - `stack` tracks the current include chain for cycle detection.
/// - `processed` deduplicates files across the diamond (same file via
///   different paths is only read once).
fn resolve_includes(
    includes: &[String],
    base_dir: &std::path::Path,
    stack: &mut Vec<std::path::PathBuf>,
    processed: &mut std::collections::HashSet<std::path::PathBuf>,
    collected: &mut Vec<crate::ast::TypeDecl>,
    rerun_paths: &mut Vec<String>,
) {
    for inc_path in includes {
        let full_path = base_dir.join(inc_path);
        let canonical = full_path.canonicalize().unwrap_or_else(|e| {
            panic!("dynspire-codegen: include file not found: {}: {e}", full_path.display())
        });

        if stack.contains(&canonical) {
            panic!("dynspire-codegen: circular include detected: {}", canonical.display());
        }
        if !processed.insert(canonical.clone()) {
            continue;
        }

        rerun_paths.push(full_path.to_string_lossy().into_owned());

        let src = std::fs::read_to_string(&full_path)
            .unwrap_or_else(|e| panic!("dynspire-codegen: failed to read include {}: {e}", full_path.display()));
        let (types, sub_includes) = parser::parse_type_fragment(&src)
            .unwrap_or_else(|e| panic!("dynspire-codegen: parse error in include {}: {e}", full_path.display()));

        collected.extend(types);

        stack.push(canonical);
        let sub_base = full_path.parent().unwrap_or_else(|| std::path::Path::new("."));
        resolve_includes(&sub_includes, sub_base, stack, processed, collected, rerun_paths);
        stack.pop();
    }
}

/// Merge included types into the interface's types, checking for conflicts.
///
/// Included types are prepended (so they appear first in canonical_sig,
/// keeping the hash deterministic). Conflicts — same name from two different
/// sources — are hard errors.
fn merge_types(iface: &mut crate::ast::Interface, included: Vec<crate::ast::TypeDecl>) {
    use std::collections::HashSet;

    let mut seen: HashSet<&str> = HashSet::new();
    for ty in &included {
        if !seen.insert(ty.name()) {
            panic!(
                "dynspire-codegen: conflicting type definitions: `{}` defined in multiple included files",
                ty.name()
            );
        }
    }

    for local_ty in &iface.types {
        if seen.contains(local_ty.name()) {
            panic!(
                "dynspire-codegen: type `{}` is both declared locally and included from a fragment",
                local_ty.name()
            );
        }
    }

    let mut merged = included;
    merged.append(&mut iface.types);
    iface.types = merged;
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const RLE_DSPI: &str = include_str!("test_data/rle.dspi");

    fn parse_rle() -> Interface {
        parser::parse(RLE_DSPI).unwrap()
    }

    #[test]
    fn generated_code_contains_trait() {
        let iface = parse_rle();
        let code = generate(&iface);
        assert!(code.contains("pub trait RleEngine: Send + Sync"));
        assert!(code.contains("fn compress(&self, data: &[u8]) -> Result<Vec<u8>, String>"));
    }

    #[test]
    fn generated_code_contains_types() {
        let iface = parse_rle();
        let code = generate(&iface);
        assert!(code.contains("pub struct CompressionReport"));
        assert!(code.contains("pub enum Tone"));
        assert!(!code.contains("impl dynspire::slots::SlotEncode for CompressionReport"), "no trait impls for DSL types");
        assert!(!code.contains("impl dynspire::slots::SlotEncode for Tone"), "no trait impls for DSL types");
    }

    #[test]
    fn generated_enum_repr_fielded_vs_fieldless() {
        let iface = parse_rle();
        let code = generate(&iface);
        // Tone has Loud(u8) → fielded enum → must keep #[repr(C, u32)]
        assert!(
            code.contains("#[repr(C, u32)]\n#[derive(Clone, Debug, PartialEq)]\npub enum Tone"),
            "fielded enum Tone must use #[repr(C, u32)]"
        );

        // RleOp is fieldless → must use #[repr(u8)] (separate generation path)
        assert!(code.contains("#[repr(u8)]"));
        assert!(
            !code.contains("#[repr(C, u32)]\n#[derive(Clone, Debug, PartialEq)]\npub enum RleOp"),
            "RleOp is generated independently and must not get #[repr(C, u32)]"
        );

        // Synthetic check: a fieldless DSL enum should get #[repr(u32)]
        let src = r#"
interface Test {
  enum Color { Red, Green, Blue }
  fn pick() -> Color;
}
"#;
        let iface2 = crate::parser::parse(src).unwrap();
        let code2 = generate(&iface2);
        assert!(
            code2.contains("#[repr(u32)]\n#[derive(Clone, Debug, PartialEq)]\npub enum Color"),
            "fieldless enum Color must use #[repr(u32)], got:\n{}",
            code2
        );
    }

    #[test]
    fn generated_enum_dtype_fields_use_dtype_codec() {
        let src = r#"
interface Codec {
  enum Tag {
    Empty,
    Labelled(String),
    Numbered(Vec<u32>),
    Maybe(Option<String>),
  }
  fn make_tag(s: &str) -> Tag;
}
"#;
        let iface = crate::parser::parse(src).unwrap();
        let code = generate(&iface);

        // The enum definition must use DType field types
        assert!(
            code.contains("Labelled(dynspire::managed::DString)"),
            "enum variant with String field must use DString:\n{}", code
        );
        assert!(
            code.contains("Numbered(dynspire::managed::DVec<u32>)"),
            "enum variant with Vec<u32> field must use DVec<u32>:\n{}", code
        );
        assert!(
            code.contains("Maybe(dynspire::managed::DOption<dynspire::managed::DString>)"),
            "enum variant with Option<String> field must use DOption<DString>:\n{}", code
        );

        // Writer for Labelled variant must emit DString's 4-slot layout
        // (allocator, ptr, len, cap) — not String's 2-slot (ptr, len)
        assert!(
            code.contains("__f0.allocator") && code.contains("__f0.ptr")
                && code.contains("__f0.len") && code.contains("__f0.cap"),
            "enum encode must write DString's 4 fields (allocator/ptr/len/cap):\n{}", code
        );

        // Reader for Option<String> variant must produce DOption<DString>
        // (3-slot decode: tag + inner DString's 4) — not Option<String>
        assert!(
            code.contains("DOption::<dynspire::managed::DString>"),
            "enum decode for Option<String> field must produce DOption<DString>:\n{}", code
        );
    }

    #[test]
    fn generated_code_contains_op_enum() {
        let iface = parse_rle();
        let code = generate(&iface);
        assert!(code.contains("pub enum RleOp"));
        assert!(code.contains("Compress = 0"));
        assert!(code.contains("impl dynspire::SpierOp for RleOp"));
    }

    #[test]
    fn generated_code_contains_free_and_hash() {
        let iface = parse_rle();
        let code = generate(&iface);
        // Returns are released by the host directly via the RC header, so there
        // is no `dynspire_free`-style type_index dispatch export.
        assert!(!code.contains("dynspire_free"), "dynspire_free must be removed");
        assert!(code.contains("dynspire_release"), "returns must be released via dynspire_release");
        assert!(code.contains("dynspire_idl_hash"));
        assert!(!code.contains("__IDL_SCHEMA"), "schema statics removed");
        assert!(!code.contains("idl_schema()"), "idl_schema() accessor removed");
        assert!(!code.contains("__IDL_TYPE_TABLE"), "type table removed");
    }

    #[test]
    fn generated_returns_use_allocator_rc() {
        let iface = parse_rle();
        let code = generate(&iface);
        // Spier-side returns are allocated through the host allocator stored at
        // `dynspire_create`, not via a global `Box::into_raw`/into_boxed_slice
        // handoff.
        assert!(
            code.contains("dynspire::managed::dynspire_alloc(__alloc,"),
            "spier must allocate returns via dynspire_alloc"
        );
        // Both the host receive path and the spier's `dynspire_free` must release
        // the returned payload through the RC header.
        assert!(
            code.contains("dynspire::managed::dynspire_release"),
            "return payloads must be released via dynspire_release"
        );
        // The old global-Box return handoff must be gone.
        assert!(
            !code.contains("into_boxed_slice"),
            "return handoff must not use into_boxed_slice"
        );
    }

    #[test]
    fn generated_struct_with_dynamic_fields_has_drop_fn() {
        let iface = parse_rle();
        let code = generate(&iface);
        // NamedRun has a DString field → should get a drop_fn
        assert!(
            code.contains("unsafe extern \"C\" fn drop_NamedRun("),
            "struct with DString field must generate drop_NamedRun"
        );
        // The drop_fn must release the DString's buffer
        assert!(
            code.contains("dynspire::managed::dynspire_release"),
            "drop_fn must release dynamic field buffers"
        );
        // make_named_run returns NamedRun → heap-alloc with drop_fn
        assert!(
            code.contains("dynspire::managed::dyn_alloc(__alloc,"),
            "struct with dynamic fields must use dyn_alloc with drop_fn"
        );
        // Host receive must use dealloc_only (skip drop_fn) for NamedRun
        assert!(
            code.contains("dynspire::managed::dynspire_dealloc_only"),
            "host receive for struct with dynamic fields must use dynspire_dealloc_only"
        );
        // CompressionReport has no dynamic fields → must NOT get a drop_fn
        assert!(
            !code.contains("unsafe extern \"C\" fn drop_CompressionReport("),
            "struct without dynamic fields must NOT generate a drop_fn"
        );
    }

    #[test]
    fn generated_struct_fields_use_dtypes() {
        let iface = parse_rle();
        let code = generate(&iface);
        // NamedRun's label field is DString, not String
        assert!(
            code.contains("pub label: dynspire::managed::DString"),
            "DString field in struct must use DString type"
        );
    }

    #[test]
    fn generated_drop_fn_releases_dstring_field() {
        let iface = parse_rle();
        let code = generate(&iface);
        // The drop_fn should release label.ptr (DString's buffer)
        assert!(
            code.contains("drop_NamedRun") && code.contains("(*__s).label.ptr as *mut u8"),
            "drop_NamedRun must release the DString buffer via label.ptr"
        );
    }

    #[test]
    fn generated_code_contains_tower() {
        let iface = parse_rle();
        let code = generate(&iface);
        assert!(code.contains("pub struct DynSpireRle"));
        assert!(code.contains("impl RleEngine for DynSpireRle"));
        assert!(code.contains("DynSpireClient::connect"));
        assert!(!code.contains("pub client:"), "client field must be private");
    }

    #[test]
    fn generated_code_contains_spier_macro() {
        let iface = parse_rle();
        let code = generate(&iface);
        assert!(code.contains("macro_rules! impl_rle_spier"));
        assert!(code.contains("dynspire_dispatch_compress"));
        assert!(code.contains("dynspire_create"));
        assert!(code.contains("dynspire_destroy"));
        assert!(code.contains("dynspire_idl_hash"));
    }

    #[test]
    fn generated_code_contains_hash() {
        let iface = parse_rle();
        let code = generate(&iface);
        assert!(code.contains("pub const RLE_IDL_HASH: u64"));
    }

    #[test]
    fn generated_code_no_enum_descriptors() {
        let iface = parse_rle();
        let code = generate(&iface);
        assert!(!code.contains("__ENUM_DESC"), "enum descriptors removed");
        assert!(!code.contains("EnumVariantDesc"), "EnumVariantDesc removed");
        assert!(!code.contains("__STRUCT_DESC"), "struct descriptors removed");
    }

    #[test]
    fn generated_code_dispatch_uses_inline_decode() {
        let iface = parse_rle();
        let code = generate(&iface);
        assert!(!code.contains("dynspire::slots::decode_param"), "decode_param should not be used");
        assert!(!code.contains("dynspire::slots::write_to_ffi"), "write_to_ffi should not be used");
        assert!(code.contains("let mut __r = dynspire::slots::SlotReader::new(__in_data)"));
        assert!(code.contains("let mut __w = dynspire::slots::SlotWriter::new()"));
    }

    #[test]
    fn generated_code_tower_uses_inline_dispatch() {
        let iface = parse_rle();
        let code = generate(&iface);
        assert!(!code.contains("self.client.call("), "call() should not be used");
        assert!(code.contains("self.client.dispatch("));
        assert!(code.contains("let mut __w = dynspire::slots::SlotWriter::new()"));
        assert!(code.contains("let mut __r = dynspire::slots::SlotReader::new(&__out)"));
    }

    #[test]
    fn generated_dtype_methods() {
        let iface = parse_rle();
        let code = generate(&iface);
        // Spier trait declares owning guards for owned-DType returns.
        assert!(code.contains("fn echo_bytes(&self, data: &[u8]) -> Result<dynspire::managed::OwnedDVec<u8>, String>"));
        assert!(code.contains("fn consume_dvec(&self, data: dynspire::managed::DVec<u8>) -> Result<u64, String>"));
        // Host client matches the trait via rust_output_type (guarded return).
        assert!(code.contains("fn echo_bytes(&self, data: &[u8]) -> Result<dynspire::managed::OwnedDVec<u8>, String>"));
        // DView params are passed by value (no Copy needed).
        assert!(code.contains("fn view_slice(&self, data: dynspire::managed::DSlice<u8>) -> Result<u64, String>"));
        // DOption is a managed struct on both sides, not Rust Option.
        assert!(code.contains("fn probe(&self, data: &[u8]) -> Result<dynspire::managed::DOption<u8>, String>"));
        // The spier dispatch converts owned guards to raw (into_raw) before write.
        assert!(code.contains("into_raw()"), "owned returns must hand raw DType to host");
        // The host receive path reconstructs an owning guard.
        assert!(code.contains("OwnedDVec::<u8>::from_raw"), "host must reconstruct OwnedDVec");
        assert!(code.contains("OwnedDString::from_raw"), "host must reconstruct OwnedDString");
        // DOption decode writes a tag, not a Rust Some/None pattern, on spier side.
        assert!(code.contains(".tag == 0"), "DOption decode must branch on tag field");
    }

    #[test]
    fn generated_python_dtype_methods() {
        let iface = parse_rle();
        let py = generate_python(&iface);
        assert!(py.contains("def echo_bytes(self, data)"), "python client must expose DVec return");
        assert!(py.contains("def consume_dvec(self, data)"), "python client must accept DVec");
        assert!(py.contains("def view_slice(self, data)"), "python client must accept DSlice");
        assert!(py.contains("def probe(self, data)"), "python client must return DOption");
        assert!(py.contains("class OwnedDVec"), "python must define owning DVec wrapper");
        assert!(py.contains("class DStringHandle"), "python must define owning DString wrapper");
        assert!(py.contains("def new_dvec(self, cap)"), "python client must expose allocator helper");
        assert!(py.contains("def new_dstring(self, s)"), "python client must expose allocator helper");
        // DOption (IDL) decode maps to None or the inner value, not a struct.
        assert!(py.contains("None if"), "python DOption must map to None/value");
    }


    #[test]
    fn generated_python_struct_with_dynamic_fields() {
        let iface = parse_rle();
        let py = generate_python(&iface);
        // NamedRun struct should be defined as an OpaqueHandle subclass
        assert!(
            py.contains("class NamedRun(OpaqueHandle)"),
            "NamedRun must be an OpaqueHandle subclass"
        );
        // make_named_run method should exist
        assert!(
            py.contains("def make_named_run(self"),
            "Python client must expose make_named_run"
        );
    }

    #[test]
    fn generated_python_struct_class() {
        let iface = parse_rle();
        let py = generate_python(&iface);

        // ctypes mirror class
        assert!(
            py.contains("class NamedRunCtypes(ctypes.Structure)"),
            "NamedRunCtypes ctypes structure class must be generated"
        );
        assert!(
            py.contains("('label', DString)"),
            "NamedRunCtypes must have label field of type DString"
        );
        assert!(
            py.contains("('count', ctypes.c_uint64)"),
            "NamedRunCtypes must have count field of type c_uint64"
        );

        // wrapper class with __slots__
        assert!(
            py.contains("    __slots__ = "),
            "NamedRun wrapper must have __slots__"
        );

        // __init__ with both fields
        assert!(
            py.contains("def __init__(self, label, count):"),
            "NamedRun must have __init__ with label and count"
        );

        // _from_ptr classmethod
        assert!(
            py.contains("def _from_ptr(cls, client, ptr):"),
            "NamedRun must have _from_ptr classmethod"
        );

        // _default classmethod
        assert!(
            py.contains("def _default(cls):"),
            "NamedRun must have _default classmethod"
        );

        // property accessors for both fields
        assert!(
            py.contains("@property\n    def label(self):"),
            "NamedRun must have label property"
        );
        assert!(
            py.contains("@property\n    def count(self):"),
            "NamedRun must have count property"
        );

        // __repr__
        assert!(
            py.contains("def __repr__(self):"),
            "NamedRun must have __repr__"
        );

        // __eq__
        assert!(
            py.contains("def __eq__(self, other):"),
            "NamedRun must have __eq__"
        );

        // __hash__
        assert!(
            py.contains("def __hash__(self):"),
            "NamedRun must have __hash__"
        );

        // _from_ptr must be used in method decode (not direct constructor)
        assert!(
            py.contains("NamedRun._from_ptr(self,"),
            "Method returning NamedRun must use _from_ptr, not direct constructor"
        );
        // old pattern must NOT appear
        assert!(
            !py.contains("NamedRun(self,") || py.contains("_from_ptr"),
            "NamedRun(self, ...) without _from_ptr must not appear in decode"
        );
    }

    #[test]
    fn generated_code_is_deterministic() {
        let iface = parse_rle();
        let code1 = generate(&iface);
        let code2 = generate(&iface);
        assert_eq!(code1, code2);
    }

    #[test]
    fn generated_code_array_type() {
        let iface = parser::parse(
            "interface Foo { fn a(id: [u8; 16]) -> [u8; 16]; }",
        ).unwrap();
        let code = generate(&iface);
        assert!(code.contains("fn a(&self, id: [u8; 16]) -> Result<[u8; 16], String>"));
    }

    // --- Include resolution tests ---

    use std::path::{Path, PathBuf};
    use std::collections::HashSet;

    fn test_data_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/test_data")
    }

    fn resolve_test(
        includes: &[String],
        base_dir: &Path,
    ) -> (Vec<crate::ast::TypeDecl>, Vec<String>) {
        let mut collected = Vec::new();
        let mut rerun = Vec::new();
        let mut stack = Vec::new();
        let mut processed = HashSet::new();
        resolve_includes(
            includes,
            base_dir,
            &mut stack,
            &mut processed,
            &mut collected,
            &mut rerun,
        );
        (collected, rerun)
    }

    #[test]
    fn test_include_resolves_types() {
        let base = test_data_dir();
        let (types, _) = resolve_test(
            &["fragments/shared_types.dspi".into()],
            &base,
        );
        assert_eq!(types.len(), 3);
        let names: Vec<&str> = types.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"SharedHandle"));
        assert!(names.contains(&"SharedConfig"));
        assert!(names.contains(&"SharedStatus"));
    }

    #[test]
    fn test_include_nested() {
        let base = test_data_dir();
        // nested_fragment includes shared_types
        let (types, _) = resolve_test(
            &["fragments/nested_fragment.dspi".into()],
            &base,
        );
        assert_eq!(types.len(), 4); // WrapperHandle + 3 shared
        let names: Vec<&str> = types.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"WrapperHandle"));
        assert!(names.contains(&"SharedHandle"));
    }

    #[test]
    fn test_include_diamond() {
        let base = test_data_dir();
        let (types, _) = resolve_test(
            &["fragments/diamond_top.dspi".into()],
            &base,
        );
        // DiamondBottom should appear only once despite being included
        // via both diamond_left and diamond_right
        let bottom_count = types.iter().filter(|t| t.name() == "DiamondBottom").count();
        assert_eq!(bottom_count, 1);
        assert_eq!(types.len(), 4); // top + left + right + bottom(once)
    }

    #[test]
    #[should_panic(expected = "circular include detected")]
    fn test_include_cycle_detection() {
        let base = test_data_dir();
        resolve_test(
            &["fragments/cycle_a.dspi".into()],
            &base,
        );
    }

    #[test]
    fn test_include_hash_composition() {
        let base = test_data_dir();
        let (included_types, _) = resolve_test(
            &["fragments/shared_types.dspi".into()],
            &base,
        );
        let mut iface = parser::parse(
            r#"include "fragments/shared_types.dspi";
               interface Demo {
                   fn open(config: SharedConfig) -> SharedHandle;
               }"#,
        ).unwrap();
        merge_types(&mut iface, included_types);

        let sig = iface.canonical_sig();
        assert!(sig.contains("SharedHandle"));
        assert!(sig.contains("SharedConfig"));
        assert!(sig.contains("SharedStatus"));
        assert_ne!(crate::ast::fnv1a_64(sig.as_bytes()), 0);
    }

    #[test]
    #[should_panic(expected = "both declared locally and included")]
    fn test_include_conflict_local() {
        use crate::ast::{OpaqueDecl, TypeDecl};
        let mut iface = crate::ast::Interface {
            name: "Foo".into(),
            includes: vec![],
            types: vec![TypeDecl::Opaque(OpaqueDecl { name: "Dup".into() })],
            methods: vec![],
        };
        let included = vec![TypeDecl::Opaque(OpaqueDecl { name: "Dup".into() })];
        merge_types(&mut iface, included);
    }

    #[test]
    #[should_panic(expected = "conflicting type definitions")]
    fn test_include_conflict_between_includes() {
        use crate::ast::{OpaqueDecl, TypeDecl};
        let mut iface = crate::ast::Interface {
            name: "Foo".into(),
            includes: vec![],
            types: vec![],
            methods: vec![],
        };
        let included = vec![
            TypeDecl::Opaque(OpaqueDecl { name: "Dup".into() }),
            TypeDecl::Opaque(OpaqueDecl { name: "Dup".into() }),
        ];
        merge_types(&mut iface, included);
    }

    #[test]
    fn test_include_generates_code_for_included_types() {
        let base = test_data_dir();
        let (included_types, _) = resolve_test(
            &["fragments/shared_types.dspi".into()],
            &base,
        );
        let mut iface = parser::parse(
            r#"include "fragments/shared_types.dspi";
               interface Demo {
                   fn open(config: SharedConfig) -> SharedHandle;
               }"#,
        ).unwrap();
        merge_types(&mut iface, included_types);
        parser::validate(&iface).unwrap();

        let code = generate(&iface);
        assert!(!code.contains("impl dynspire::slots::SlotEncode for SharedHandle"), "no trait impls for DSL types");
        assert!(code.contains("pub struct SharedConfig"));
        assert!(code.contains("pub enum SharedStatus"));
    }

    // --- BuildContext deduplication tests ---

    #[test]
    fn test_ctx_dedup_skips_identical_type() {
        let mut ctx = BuildContext::new();
        let iface_a = parser::parse(
            r#"interface A {
                struct Pair { x: u64, y: u64, }
                fn get() -> Pair;
            }"#,
        ).unwrap();
        let code_a = generate_with_ctx(&iface_a, &mut ctx);
        assert!(code_a.contains("pub struct Pair"));

        let iface_b = parser::parse(
            r#"interface B {
                struct Pair { x: u64, y: u64, }
                fn put(p: Pair) -> ();
            }"#,
        ).unwrap();
        let code_b = generate_with_ctx(&iface_b, &mut ctx);
        assert!(!code_b.contains("pub struct Pair"), "Pair should be skipped — already emitted");
        assert!(code_b.contains("pub trait BEngine"), "B-specific code still generated");
    }

    #[test]
    fn test_ctx_dedup_across_three_interfaces() {
        let mut ctx = BuildContext::new();
        let dspi = r#"struct Point { x: u64, y: u64, }"#;

        for name in &["A", "B", "C"] {
            let iface = parser::parse(&format!(
                r#"interface {name} {{ {dspi} fn f() -> Point; }}"#
            )).unwrap();
            let code = generate_with_ctx(&iface, &mut ctx);
            if *name == "A" {
                assert!(code.contains("pub struct Point"));
            } else {
                assert!(!code.contains("pub struct Point"), "Point should be skipped in {name}");
            }
        }
    }

    #[test]
    #[should_panic(expected = "conflicting type `Handle`")]
    fn test_ctx_dedup_panics_on_conflict() {
        let mut ctx = BuildContext::new();
        let iface_a = parser::parse(
            r#"interface A { struct Handle { id: u64, } fn f() -> Handle; }"#,
        ).unwrap();
        generate_with_ctx(&iface_a, &mut ctx);

        let iface_b = parser::parse(
            r#"interface B { struct Handle { name: String, } fn f() -> Handle; }"#,
        ).unwrap();
        generate_with_ctx(&iface_b, &mut ctx);
    }

    #[test]
    fn test_ctx_dedup_skips_enum() {
        let mut ctx = BuildContext::new();
        let iface_a = parser::parse(
            r#"interface A { enum Color { Red, Green, Blue, } fn f() -> Color; }"#,
        ).unwrap();
        let code_a = generate_with_ctx(&iface_a, &mut ctx);
        assert!(code_a.contains("pub enum Color"));

        let iface_b = parser::parse(
            r#"interface B { enum Color { Red, Green, Blue, } fn f(c: Color) -> (); }"#,
        ).unwrap();
        let code_b = generate_with_ctx(&iface_b, &mut ctx);
        assert!(!code_b.contains("pub enum Color"), "Color definition should be skipped");
    }

    #[test]
    fn test_ctx_dedup_skips_opaque() {
        let mut ctx = BuildContext::new();
        let iface_a = parser::parse(
            r#"interface A { struct Handle { id: u64, } fn f() -> Handle; }"#,
        ).unwrap();
        let code_a = generate_with_ctx(&iface_a, &mut ctx);
        assert!(code_a.contains("pub struct Handle"));

        let iface_b = parser::parse(
            r#"interface B { struct Handle { id: u64, } fn f(h: Handle) -> (); }"#,
        ).unwrap();
        let code_b = generate_with_ctx(&iface_b, &mut ctx);
        assert!(!code_b.contains("pub struct Handle"), "Handle definition should be skipped");
    }
}
