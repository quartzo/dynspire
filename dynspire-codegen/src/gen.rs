//! Code generator: AST → Rust source string.
//!
//! [`generate`] produces a single `.rs` file containing everything the IDL
//! crate needs: trait, types, Op enum, hash, schema, tower client, and the
//! spier dispatch macro.

use crate::ast::*;
use crate::parser;

use std::collections::HashMap;

// ===========================================================================
// TypeTable — flat array of IdlTypeNode entries
// ===========================================================================

struct TypeNode {
    kind: &'static str,
    size: u32,
    children: Vec<i32>,
}

struct TypeTable {
    nodes: Vec<TypeNode>,
    enum_indices: HashMap<String, i32>,
    struct_indices: HashMap<String, i32>,
}

impl TypeTable {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            enum_indices: HashMap::new(),
            struct_indices: HashMap::new(),
        }
    }

    fn add(&mut self, ty: &FieldType) -> u32 {
        let node = match ty {
            FieldType::Unit => mk("IDL_UNIT", 0, &[]),
            FieldType::Bool => mk("IDL_BOOL", 0, &[]),
            FieldType::U8 | FieldType::I8 => mk("IDL_U8", 0, &[]),
            FieldType::U16 | FieldType::I16 | FieldType::U32 | FieldType::I32 => {
                mk("IDL_U32", 0, &[])
            }
            FieldType::U64 | FieldType::I64 => mk("IDL_U64", 0, &[]),
            FieldType::F32 => mk("IDL_F32", 0, &[]),
            FieldType::F64 => mk("IDL_F64", 0, &[]),
            FieldType::Str => mk("IDL_STR", 0, &[]),
            FieldType::U8Slice => {
                let child = self.add(&FieldType::U8) as i32;
                mk("IDL_SLICE", 0, &[child])
            }
            FieldType::OutU8Vec => mk("IDL_OUT_VEC", 0, &[]),
            FieldType::String => mk("IDL_STRING", 0, &[]),
            FieldType::Vec(inner) => {
                let child = self.add(inner) as i32;
                mk("IDL_VEC", 0, &[child])
            }
            FieldType::Option(inner) => {
                let child = self.add(inner) as i32;
                mk("IDL_OPTION", 0, &[child])
            }
            FieldType::Tuple(elems) => {
                let children: Vec<i32> = elems.iter().map(|e| self.add(e) as i32).collect();
                mk("IDL_TUPLE", 0, &children)
            }
            FieldType::Array(inner, len) => {
                let child = self.add(inner) as i32;
                mk("IDL_ARRAY", *len as u32, &[child])
            }
            FieldType::Named(name) => {
                if self.enum_indices.contains_key(name) {
                    let ei = self.enum_indices[name];
                    mk("IDL_ENUM", 0, &[ei])
                } else {
                    let si = {
                        let len = self.struct_indices.len() as i32;
                        *self.struct_indices.entry(name.clone()).or_insert(len)
                    };
                    mk("IDL_STRUCT", 0, &[si])
                }
            }
        };
        let idx = self.nodes.len() as u32;
        self.nodes.push(node);
        idx
    }

    fn emit(&self) -> String {
        let mut out = String::new();
        for n in &self.nodes {
            let mut children = [-1i32; 8];
            for (i, &c) in n.children.iter().enumerate().take(8) {
                children[i] = c;
            }
            out.push_str(&format!(
                "    dynspire::ffi::IdlTypeNode {{ kind: dynspire::ffi::{}, child_count: {}, _pad: [0; 2], size: {}, children: [{}, {}, {}, {}, {}, {}, {}, {}] }},\n",
                n.kind, n.children.len(), n.size,
                children[0], children[1], children[2], children[3],
                children[4], children[5], children[6], children[7],
            ));
        }
        out
    }
}

fn mk(kind: &'static str, size: u32, children: &[i32]) -> TypeNode {
    TypeNode { kind, size, children: children.to_vec() }
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
             let __boxed = {expr}.into_bytes().into_boxed_slice(); \
             let __ptr = __boxed.as_ptr() as usize; \
             core::mem::forget(__boxed); \
             {w}.write_u64(__ptr as u64); \
             {w}.write_u64(__len as u64); }}"
        ),
        FieldType::Vec(_) => format!(
            "if {expr}.is_empty() {{ {w}.write_u64(0); {w}.write_u64(0); }} else {{ \
             let __len = {expr}.len(); \
             let __boxed = {expr}.into_boxed_slice(); \
             let __ptr = __boxed.as_ptr() as usize; \
             core::mem::forget(__boxed); \
             {w}.write_u64(__ptr as u64); \
             {w}.write_u64(__len as u64); }}"
        ),
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
                TypeDecl::Struct(_) | TypeDecl::Opaque(_) => {
                    format!("{w}.write_u64(Box::into_raw(Box::new({expr})) as u64);")
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
             else {{ String::from_utf8_unchecked(Box::from_raw(core::ptr::slice_from_raw_parts_mut(__ptr, __len)).into_vec()) }} }}"
        ),
        FieldType::Vec(inner) => {
            let rt = inner.rust_type();
            format!(
                "unsafe {{ \
                 let __ptr = {r}.read_u64() as *mut {rt}; \
                 let __len = {r}.read_u64() as usize; \
                 if __ptr.is_null() || __len == 0 {{ Vec::new() }} \
                 else {{ Box::from_raw(core::ptr::slice_from_raw_parts_mut(__ptr, __len)).into_vec() }} }}"
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
                TypeDecl::Struct(_) | TypeDecl::Opaque(_) => {
                    format!("unsafe {{ *Box::from_raw({r}.read_u64() as *mut {name}) }}")
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
                let needs_deref = matches!(fty, FieldType::U8 | FieldType::I8 | FieldType::U16 | FieldType::I16 | FieldType::U32 | FieldType::I32 | FieldType::U64 | FieldType::I64 | FieldType::F32 | FieldType::F64 | FieldType::Bool);
                let actual_expr = if needs_deref { format!("*{fname}") } else { fname.clone() };
                field_stmts.push_str(&gen_write_encode(fty, &actual_expr, w, types));
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
                field_stmts.push_str(&gen_write_return(fty, fname, w, types));
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
            let fields: Vec<String> = v.fields.iter().map(|fty| gen_read_decode(fty, r, types)).collect();
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
            let fields: Vec<String> = v.fields.iter().map(|fty| gen_read_receive(fty, r, types)).collect();
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
            .chain(m.params.iter().map(|p| format!("{}: {}", p.name, p.ty.rust_type())))
            .collect();
        out.push_str(&format!(
            "    fn {}({}) -> Result<{}, String>;\n",
            m.name,
            params.join(", "),
            m.return_type.rust_type(),
        ));
    }
    out.push_str("}\n\n");
    out
}

// ===========================================================================
// gen_types — struct/enum/opaque definitions + slot impls + descriptors
// ===========================================================================

fn gen_types(iface: &Interface) -> String {
    let mut out = String::new();

    for ty in &iface.types {
        match ty {
            TypeDecl::Struct(s) => {
                if !s.fields.is_empty() {
                    out.push_str(&gen_struct_def(s));
                }
                out.push_str(&gen_struct_descriptor(&s.name));
            }
            TypeDecl::Enum(e) => {
                out.push_str(&gen_enum_def(e));
                out.push_str(&gen_enum_descriptor(e));
            }
            TypeDecl::Opaque(o) => {
                out.push_str(&gen_struct_descriptor(&o.name));
            }
        }
    }
    out
}

fn gen_struct_def(s: &StructDecl) -> String {
    let mut out = format!(
        "#[derive(Clone, Debug, PartialEq)]\npub struct {} {{\n",
        s.name
    );
    for (fname, fty) in &s.fields {
        out.push_str(&format!("    pub {}: {},\n", fname, fty.rust_type()));
    }
    out.push_str("}\n\n");
    out
}

fn gen_enum_def(e: &EnumDecl) -> String {
    let mut out = format!("#[derive(Clone, Debug, PartialEq)]\npub enum {} {{\n", e.name);
    for v in &e.variants {
        if v.fields.is_empty() {
            out.push_str(&format!("    {},\n", v.name));
        } else {
            let fields: Vec<String> = v.fields.iter().map(|t| t.rust_type()).collect();
            out.push_str(&format!("    {}({}),\n", v.name, fields.join(", ")));
        }
    }
    out.push_str("}\n\n");
    out
}

fn gen_struct_descriptor(name: &str) -> String {
    format!(
        r#"#[doc(hidden)]
pub static __STRUCT_DESC_{n}: dynspire::ffi::StructDescriptor = dynspire::ffi::StructDescriptor {{
    name: b"{n}\0".as_ptr(),
    name_len: {len},
}};

"#,
        n = name,
        len = name.len(),
    )
}

fn gen_enum_descriptor(e: &EnumDecl) -> String {
    let n = &e.name;
    let var_count = e.variants.len();

    let mut tt = TypeTable::new();
    let mut all_field_indices: Vec<u32> = Vec::new();
    let mut variant_entries: Vec<String> = Vec::new();

    let mut disc: u32 = 0;
    for v in &e.variants {
        let field_offset = all_field_indices.len() as u32;
        let field_types: Vec<u32> = v.fields.iter().map(|t| tt.add(t)).collect();
        let field_count = field_types.len() as u32;
        all_field_indices.extend(field_types);

        variant_entries.push(format!(
            "    dynspire::ffi::EnumVariantDesc {{ disc: {d}, name: b\"{vn}\\0\".as_ptr(), name_len: {vnl}, field_count: {fc}, field_type_offset: {fo} }},",
            d=disc, vn=v.name, vnl=v.name.len(), fc=field_count, fo=field_offset,
        ));
        disc += 1;
    }

    let type_count = tt.nodes.len();
    let field_type_count = all_field_indices.len();

    format!(
        r#"#[doc(hidden)]
pub static __ENUM_TYPES_{n}: &[dynspire::ffi::IdlTypeNode] = &[
{tt}];

#[doc(hidden)]
pub static __ENUM_FIELD_TYPES_{n}: &[u32] = &[{ft}];

#[doc(hidden)]
pub static __ENUM_VARIANTS_{n}: &[dynspire::ffi::EnumVariantDesc] = &[
{ve}
];

#[doc(hidden)]
pub static __ENUM_DESC_{n}: dynspire::ffi::EnumDescriptor = dynspire::ffi::EnumDescriptor {{
    name: b"{n}\0".as_ptr(),
    name_len: {nl},
    variant_count: {vc},
    variants: __ENUM_VARIANTS_{n}.as_ptr(),
    type_table: __ENUM_TYPES_{n}.as_ptr(),
    type_count: {tc},
    field_types: __ENUM_FIELD_TYPES_{n}.as_ptr(),
    field_type_count: {ftc},
}};

"#,
        n=n,
        tt=tt.emit(),
        ft=all_field_indices.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", "),
        ve=variant_entries.join("\n"),
        nl=n.len(),
        vc=var_count,
        tc=type_count,
        ftc=field_type_count,
    )
}

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
// gen_schema — type table, method table, DynSpireIdl, dynspire_free
// ===========================================================================

fn gen_schema(iface: &Interface) -> String {
    let hcn = hash_const_name(iface);
    let mut tt = TypeTable::new();

    // Populate enum indices
    let mut enum_i = 0i32;
    for ty in &iface.types {
        if let TypeDecl::Enum(e) = ty {
            tt.enum_indices.insert(e.name.clone(), enum_i);
            enum_i += 1;
        }
    }
    // Populate struct indices
    let mut struct_i = 0i32;
    for ty in &iface.types {
        match ty {
            TypeDecl::Struct(s) if !s.fields.is_empty() => {
                tt.struct_indices.insert(s.name.clone(), struct_i);
                struct_i += 1;
            }
            TypeDecl::Opaque(o) => {
                tt.struct_indices.insert(o.name.clone(), struct_i);
                struct_i += 1;
            }
            _ => {}
        }
    }

    let enum_count = enum_i as usize;
    let struct_count = struct_i as usize;

    // Build method entries
    let mut method_entries: Vec<String> = Vec::new();
    let mut free_arms: Vec<String> = Vec::new();

    for m in &iface.methods {
        let mut param_entries: Vec<String> = Vec::new();
        for p in &m.params {
            let pidx = tt.add(&p.ty);
            param_entries.push(format!(
                "        dynspire::ffi::IdlParam {{ name: b\"{}\\0\".as_ptr(), name_len: {}, type_idx: {} }}",
                p.name, p.name.len(), pidx,
            ));
        }
        let ret_idx = tt.add(&m.return_type);
        let pc = param_entries.len();

        let params_field = if pc == 0 {
            "params: core::ptr::null(), param_count: 0,".to_string()
        } else {
            format!("params: [{}].as_ptr(), param_count: {},", param_entries.join(", "), pc)
        };

        method_entries.push(format!(
            "    dynspire::ffi::IdlMethod {{ name: b\"{}\\0\".as_ptr(), name_len: {}, {params_field} return_type_idx: {ret}, _pad: [0; 4] }},",
            m.name, m.name.len(), ret=ret_idx,
        ));

        let recv_expr = gen_read_receive(&m.return_type, "r", &iface.types);
        free_arms.push(format!(
            "            {ret_idx} => {{ let _ = {recv_expr}; }}",
        ));
    }

    let type_count = tt.nodes.len();
    let method_count = method_entries.len();

    // Collect enum descriptor pointers
    let enum_names: Vec<String> = iface.types.iter()
        .filter_map(|t| if let TypeDecl::Enum(e) = t { Some(e.name.clone()) } else { None })
        .collect();
    let enum_ptr_entries: Vec<String> = enum_names.iter()
        .map(|n| format!("    &__ENUM_DESC_{n},"))
        .collect();

    // Collect struct descriptor pointers
    let struct_names: Vec<String> = iface.types.iter()
        .filter_map(|t| match t {
            TypeDecl::Struct(s) if !s.fields.is_empty() => Some(s.name.clone()),
            TypeDecl::Opaque(o) => Some(o.name.clone()),
            _ => None,
        })
        .collect();
    let struct_ptr_entries: Vec<String> = struct_names.iter()
        .map(|n| format!("    &__STRUCT_DESC_{n},"))
        .collect();

    let enum_ptrs_init = if enum_count == 0 {
        "core::ptr::null()".to_string()
    } else {
        format!("__IDL_ENUM_PTRS.as_ptr()", )
    };
    let struct_ptrs_init = if struct_count == 0 {
        "core::ptr::null()".to_string()
    } else {
        "__IDL_STRUCT_PTRS.as_ptr()".to_string()
    };

    let enum_ptrs_static = if enum_count > 0 {
        format!(
            "#[doc(hidden)]\npub static __IDL_ENUM_PTRS: &[&'static dynspire::ffi::EnumDescriptor] = &[\n{}\n];\n\n",
            enum_ptr_entries.join("\n"),
        )
    } else {
        String::new()
    };

    let struct_ptrs_static = if struct_count > 0 {
        format!(
            "#[doc(hidden)]\npub static __IDL_STRUCT_PTRS: &[&'static dynspire::ffi::StructDescriptor] = &[\n{}\n];\n\n",
            struct_ptr_entries.join("\n"),
        )
    } else {
        String::new()
    };

    format!(
        r#"{enum_ptrs}{struct_ptrs}
#[doc(hidden)]
pub static __IDL_TYPE_TABLE: &[dynspire::ffi::IdlTypeNode] = &[
{tt}];

#[doc(hidden)]
pub static __IDL_METHODS: &[dynspire::ffi::IdlMethod] = &[
{me}
];

pub static __IDL_SCHEMA: dynspire::ffi::DynSpireIdl = dynspire::ffi::DynSpireIdl {{
    name: b"\0".as_ptr(),
    name_len: 0,
    hash: {hcn},
    type_table: __IDL_TYPE_TABLE.as_ptr(),
    type_count: {tc},
    methods: __IDL_METHODS.as_ptr(),
    method_count: {mc},
    enum_table: {epi} as *const *const dynspire::ffi::EnumDescriptor,
    enum_count: {ec},
    struct_table: {spi} as *const *const dynspire::ffi::StructDescriptor,
    struct_count: {sc},
    free_fn: dynspire_free,
}};

pub fn idl_schema() -> &'static dynspire::ffi::DynSpireIdl {{
    &__IDL_SCHEMA
}}

pub unsafe extern "C" fn dynspire_free(
    type_index: u32,
    slots: *const u64,
    slot_count: usize,
) {{
    if slots.is_null() || slot_count == 0 {{ return; }}
    let slice = core::slice::from_raw_parts(slots, slot_count);
    let mut r = dynspire::slots::SlotReader::new(slice);
    let tag = r.read_u64();
    if tag == 1 {{
        let _ = unsafe {{
            let __ptr = r.read_u64() as *mut u8;
            let __len = r.read_u64() as usize;
            if __ptr.is_null() || __len == 0 {{ String::new() }}
            else {{ String::from_utf8_unchecked(Box::from_raw(core::ptr::slice_from_raw_parts_mut(__ptr, __len)).into_vec()) }}
        }};
        return;
    }}
    match type_index {{
{free}
        _ => {{}}
    }}
}}

"#,
        enum_ptrs=enum_ptrs_static,
        struct_ptrs=struct_ptrs_static,
        tt=tt.emit(),
        me=method_entries.join("\n"),
        hcn=hcn,
        tc=type_count,
        mc=method_count,
        epi=enum_ptrs_init,
        ec=enum_count,
        spi=struct_ptrs_init,
        sc=struct_count,
        free=free_arms.join("\n"),
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
        for p in &m.params {
            encode_stmts.push_str(&gen_write_encode(&p.ty, &p.name, "__w", types));
        }

        let result_recv = gen_read_result_receive(&m.return_type, "__r", types);

        methods.push_str(&format!(
            "    fn {}({}) -> Result<{}, String> {{\n\
             let mut __w = dynspire::slots::SlotWriter::new();\n\
             {encode_stmts}\
             let mut __out = [0u64; dynspire::slots::MAX_OUT_SLOTS];\n\
             self.client.dispatch({}::{} as usize, __w.as_slice(), &mut __out)?;\n\
             let mut __r = dynspire::slots::SlotReader::new(&__out);\n\
             {result_recv}\n    }}\n",
            m.name,
            params.join(", "),
            m.return_type.rust_type(),
            opn,
            pascal(&m.name),
        ));
    }

    format!(
        r#"pub struct {cn} {{
    client: dynspire::DynSpireClient,
}}

impl {cn} {{
    pub fn connect(spier_name: &str, config: &std::collections::HashMap<String, String>) -> Result<Self, String> {{
        let client = dynspire::DynSpireClient::connect(spier_name, &IDL, config)?;
        Ok(Self {{ client }})
    }}
}}

impl {tn} for {cn} {{
{methods}}}

"#,
        cn=cn, tn=tn, methods=methods,
    )
}

// ===========================================================================
// gen_spier_macro — macro_rules! for dispatch + storage
// ===========================================================================

fn gen_spier_macro(iface: &Interface) -> String {
    let mn = spier_macro_name(iface);
    let tn = trait_name(iface);
    let types = &iface.types;

    let mut dispatch_fns = String::new();

    for m in &iface.methods {
        let fn_name = format!("dynspire_dispatch_{}", m.name);

        let decode_block = if m.params.is_empty() {
            String::new()
        } else {
            let mut block = String::from(
                "            let __in_data = if !in_slots.is_null() && in_count > 0 {\n                unsafe { core::slice::from_raw_parts(in_slots, in_count) }\n            } else { &[] };\n            let mut __r = dynspire::slots::SlotReader::new(__in_data);\n",
            );
            for p in &m.params {
                let decode_expr = gen_read_decode(&p.ty, "__r", types);
                block.push_str(&format!(
                    "            let {}: {} = {decode_expr};\n",
                    p.name, p.ty.rust_type(),
                ));
            }
            block
        };

        let null_check = "            if state_handle.is_null() {\n\
                           let mut __w = dynspire::slots::SlotWriter::new();\n\
                           __w.write_u64(1);\n\
                           let __err = \"null handle\".to_string();\n\
                           if __err.is_empty() { __w.write_u64(0); __w.write_u64(0); } else {\n\
                           let __len = __err.len(); let __boxed = __err.into_bytes().into_boxed_slice();\n\
                           let __ptr = __boxed.as_ptr() as usize; core::mem::forget(__boxed);\n\
                           __w.write_u64(__ptr as u64); __w.write_u64(__len as u64); }\n\
                           let __n = __w.len(); if __n > out_capacity { return 2; }\n\
                           if __n > 0 { unsafe { core::ptr::copy_nonoverlapping(__w.as_slice().as_ptr(), out_slots, __n); } }\n\
                           return 0; }\n";

        let state_cast = "            let state = &*(state_handle as *const $state);\n";

        let param_names: Vec<String> = std::iter::once("state".to_string())
            .chain(m.params.iter().map(|p| p.name.clone()))
            .collect();
        let call_args = format!("({})", param_names.join(", "));

        let ok_return = gen_write_return(&m.return_type, "__v", "__w", types);
        let err_return = gen_write_return(&FieldType::String, "__e", "__w", types);
        let write_out = gen_write_out_epilogue("__w", "out_slots", "out_capacity");

        let result_encode = if ok_return.is_empty() && m.return_type == FieldType::Unit {
            format!(
                "match _result {{\n\
                 Ok(__v) => {{ __w.write_u64(0); }}\n\
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
            in_slots: *const u64,
            in_count: usize,
            out_slots: *mut u64,
            out_capacity: usize,
        ) -> u8 {{
{null_check}{state_cast}{decode}            let _result = <$state as $crate::{tn}>::{method}{call_args};
            let mut __w = dynspire::slots::SlotWriter::new();
            {result_encode}            {write_out}
        }}
"#,
            fn_name=fn_name,
            null_check=null_check,
            state_cast=state_cast,
            decode=decode_block,
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
        #[no_mangle]
        pub extern "C" fn dynspire_create(
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
                Ok(state) => Box::into_raw(Box::new(state)) as *mut std::ffi::c_void,
                Err(e) => {{
                    eprintln!("spier init failed: {{e}}");
                    std::ptr::null_mut()
                }}
            }}
        }}

        #[no_mangle]
        pub extern "C" fn dynspire_destroy(handle: *mut std::ffi::c_void) {{
            if !handle.is_null() {{
                unsafe {{ drop(Box::from_raw(handle as *mut $state)); }}
            }}
        }}

        #[no_mangle]
        pub extern "C" fn dynspire_idl_hash() -> u64 {{
            $crate::idl_schema().hash
        }}

        #[no_mangle]
        pub extern "C" fn dynspire_spier_name() -> *const u8 {{
            concat!($name, "\0").as_ptr()
        }}

        #[no_mangle]
        pub extern "C" fn dynspire_idl_schema() -> *const dynspire::ffi::DynSpireIdl {{
            $crate::idl_schema()
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
    let hash = fnv1a_64(iface.canonical_sig().as_bytes());
    let hcn = hash_const_name(iface);

    let mut out = String::new();
    out.push_str("// Auto-generated by dynspire-codegen — do not edit.\n\n");

    out.push_str(&gen_trait(iface));
    out.push_str(&gen_types(iface));
    out.push_str(&gen_op_enum(iface));
    out.push_str(&format!("pub const {}: u64 = {};\n\n", hcn, hash));
    out.push_str(&format!(
        "pub static IDL: dynspire::IdlDescriptor = dynspire::IdlDescriptor {{\n    hash: {},\n    methods: &[{}],\n}};\n\n",
        hcn,
        iface.methods.iter()
            .map(|m| format!("\"{}\"", m.name))
            .collect::<Vec<_>>()
            .join(", "),
    ));
    out.push_str(&gen_schema(iface));
    out.push_str(&gen_tower(iface));
    out.push_str(&gen_spier_macro(iface));

    out
}

/// Entry point for `build.rs`.
///
/// Reads the `.dspi` file, resolves includes, validates, generates Rust
/// source, and writes to `OUT_DIR`.
pub fn build(dspi_path: &str) {
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

    let code = generate(&iface);
    let out_dir = std::env::var("OUT_DIR")
        .expect("dynspire-codegen: OUT_DIR not set (must be called from build.rs)");
    let file_name = format!("{out_dir}/{}_idl.rs", iface.name.to_lowercase());
    std::fs::write(&file_name, &code)
        .unwrap_or_else(|e| panic!("dynspire-codegen: failed to write {file_name}: {e}"));

    for path in &rerun_paths {
        println!("cargo:rerun-if-changed={path}");
    }
}

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
    merged.extend(iface.types.drain(..));
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
    fn generated_code_contains_op_enum() {
        let iface = parse_rle();
        let code = generate(&iface);
        assert!(code.contains("pub enum RleOp"));
        assert!(code.contains("Compress = 0"));
        assert!(code.contains("impl dynspire::SpierOp for RleOp"));
    }

    #[test]
    fn generated_code_contains_schema() {
        let iface = parse_rle();
        let code = generate(&iface);
        assert!(code.contains("__IDL_TYPE_TABLE"));
        assert!(code.contains("__IDL_METHODS"));
        assert!(code.contains("__IDL_SCHEMA"));
        assert!(code.contains("idl_schema()"));
        assert!(code.contains("dynspire_free"));
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
    fn generated_code_contains_enum_descriptor() {
        let iface = parse_rle();
        let code = generate(&iface);
        assert!(code.contains("__ENUM_DESC_Tone"));
        assert!(code.contains("EnumVariantDesc"));
        assert!(!code.contains("SlotEnumDescriptor for Tone"), "SlotEnumDescriptor trait impls removed");
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
        assert!(code.contains("IDL_ARRAY"));
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
}
