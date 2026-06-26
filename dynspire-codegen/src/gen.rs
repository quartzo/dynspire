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
                out.push_str(&gen_boxed_slot_impls(&s.name));
                out.push_str(&gen_struct_descriptor(&s.name));
            }
            TypeDecl::Enum(e) => {
                out.push_str(&gen_enum_def(e));
                out.push_str(&gen_enum_slot_impls(e));
                out.push_str(&gen_enum_descriptor(e));
            }
            TypeDecl::Opaque(o) => {
                out.push_str(&gen_boxed_slot_impls(&o.name));
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

/// Boxed-pointer slot impls — generated for DSL-declared structs/opaques.
fn gen_boxed_slot_impls(name: &str) -> String {
    format!(
        r#"impl dynspire::slots::SlotEncode for {n} {{
    fn encode(&self, w: &mut dynspire::slots::SlotWriter) {{
        w.write_u64(self as *const Self as u64);
    }}
}}
impl dynspire::slots::SlotDecode<'_> for {n} {{
    unsafe fn decode(r: &mut dynspire::slots::SlotReader<'_>) -> Self {{
        let ptr = r.read_u64() as *const Self;
        (*ptr).clone()
    }}
}}
impl dynspire::slots::SlotReturn for {n} {{
    fn into_slots(self, w: &mut dynspire::slots::SlotWriter) {{
        w.write_u64(Box::into_raw(Box::new(self)) as u64);
    }}
}}
impl dynspire::slots::SlotReceive for {n} {{
    unsafe fn from_slots(r: &mut dynspire::slots::SlotReader) -> Self {{
        *Box::from_raw(r.read_u64() as *mut Self)
    }}
}}
"#,
        n = name,
    )
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

/// Enum slot impls — discriminant + fields, generated for DSL-declared enums.
fn gen_enum_slot_impls(e: &EnumDecl) -> String {
    let n = &e.name;
    let discs: Vec<i64> = {
        let mut d = Vec::new();
        let mut next: i64 = 0;
        for _ in &e.variants {
            d.push(next);
            next += 1;
        }
        d
    };

    // encode
    let mut enc = String::new();
    let mut dec = String::new();
    let mut ret = String::new();
    let mut recv = String::new();

    for (v, &disc) in e.variants.iter().zip(&discs) {
        if v.fields.is_empty() {
            enc.push_str(&format!("        {n}::{vn} => {{ w.write_u64({d}); }}\n", n=n, vn=v.name, d=disc));
            dec.push_str(&format!("            {d} => {n}::{vn},\n", d=disc, n=n, vn=v.name));
            ret.push_str(&format!("        {n}::{vn} => {{ w.write_u64({d}); }}\n", n=n, vn=v.name, d=disc));
            recv.push_str(&format!("            {d} => {n}::{vn},\n", d=disc, n=n, vn=v.name));
        } else {
            let fnames: Vec<String> = (0..v.fields.len())
                .map(|i| format!("f{i}"))
                .collect();
            let ftypes: Vec<&FieldType> = v.fields.iter().collect();

            // encode: Variant(ref f0, ref f1) => { w.write_u64(disc); <T0 as SlotEncode>::encode(f0, w); ... }
            let ref_pats: Vec<String> = fnames.iter().map(|f| format!("ref {f}")).collect();
            let enc_fields: Vec<String> = ftypes.iter().zip(fnames.iter())
                .map(|(t, f)| format!("<{} as dynspire::slots::SlotEncode>::encode({}, w);", t.rust_type(), f))
                .collect();
            enc.push_str(&format!(
                "        {n}::{vn}({pats}) => {{ w.write_u64({d}); {ef} }}\n",
                n=n, vn=v.name, pats=ref_pats.join(", "), d=disc, ef=enc_fields.join(" "),
            ));

            // decode: disc => Variant(<T0 as SlotDecode>::decode(r), ...)
            let dec_fields: Vec<String> = ftypes.iter()
                .map(|t| format!("<{} as dynspire::slots::SlotDecode>::decode(r)", t.rust_type()))
                .collect();
            dec.push_str(&format!(
                "            {d} => {n}::{vn}({df}),\n",
                d=disc, n=n, vn=v.name, df=dec_fields.join(", "),
            ));

            // return: Variant(f0, f1) => { w.write_u64(disc); <T0 as SlotReturn>::into_slots(f0, w); ... }
            let ret_fields: Vec<String> = ftypes.iter().zip(fnames.iter())
                .map(|(t, f)| format!("<{} as dynspire::slots::SlotReturn>::into_slots({}, w);", t.rust_type(), f))
                .collect();
            ret.push_str(&format!(
                "        {n}::{vn}({pats}) => {{ w.write_u64({d}); {rf} }}\n",
                n=n, vn=v.name, pats=fnames.join(", "), d=disc, rf=ret_fields.join(" "),
            ));

            // receive: disc => Variant(<T0 as SlotReceive>::from_slots(r), ...)
            let recv_fields: Vec<String> = ftypes.iter()
                .map(|t| format!("<{} as dynspire::slots::SlotReceive>::from_slots(r)", t.rust_type()))
                .collect();
            recv.push_str(&format!(
                "            {d} => {n}::{vn}({rf}),\n",
                d=disc, n=n, vn=v.name, rf=recv_fields.join(", "),
            ));
        }
    }

    format!(
        r#"impl dynspire::slots::SlotEncode for {n} {{
    fn encode(&self, w: &mut dynspire::slots::SlotWriter) {{
        match self {{
{enc}        }}
    }}
}}
impl dynspire::slots::SlotDecode<'_> for {n} {{
    unsafe fn decode(r: &mut dynspire::slots::SlotReader<'_>) -> Self {{
        match r.read_u64() {{
{dec}            _ => panic!("invalid discriminant for {n}"),
        }}
    }}
}}
impl dynspire::slots::SlotReturn for {n} {{
    fn into_slots(self, w: &mut dynspire::slots::SlotWriter) {{
        match self {{
{ret}        }}
    }}
}}
impl dynspire::slots::SlotReceive for {n} {{
    unsafe fn from_slots(r: &mut dynspire::slots::SlotReader) -> Self {{
        match r.read_u64() {{
{recv}            _ => panic!("invalid discriminant for {n}"),
        }}
    }}
}}
impl dynspire::slots::SlotEnumDescriptor for {n} {{
    fn descriptor() -> &'static dynspire::ffi::EnumDescriptor {{
        &__ENUM_DESC_{n}
    }}
}}

"#,
        n=n, enc=enc, dec=dec, ret=ret, recv=recv,
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

        free_arms.push(format!(
            "            {} => {{ let _: {} = dynspire::slots::SlotReceive::from_slots(&mut r); }}",
            ret_idx,
            m.return_type.rust_type(),
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
        let _: String = dynspire::slots::SlotReceive::from_slots(&mut r);
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

    let mut methods = String::new();
    for m in &iface.methods {
        let params: Vec<String> = std::iter::once("&self".to_string())
            .chain(m.params.iter().map(|p| format!("{}: {}", p.name, p.ty.rust_type())))
            .collect();

        let arg_names: Vec<String> = m.params.iter().map(|p| p.name.clone()).collect();
        let args = match arg_names.len() {
            0 => "()".to_string(),
            1 => format!("({},)", arg_names[0]),
            _ => format!("({})", arg_names.join(", ")),
        };

        methods.push_str(&format!(
            "    fn {}({}) -> Result<{}, String> {{\n        self.client.call({}::{}, {})\n    }}\n",
            m.name,
            params.join(", "),
            m.return_type.rust_type(),
            opn,
            pascal(&m.name),
            args,
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

    let mut dispatch_fns = String::new();

    for m in &iface.methods {
        let fn_name = format!("dynspire_dispatch_{}", m.name);

        let decode_block = if m.params.is_empty() {
            String::new()
        } else {
            let mut block = String::from(
                "            let in_data = if !in_slots.is_null() && in_count > 0 {\n                unsafe { core::slice::from_raw_parts(in_slots, in_count) }\n            } else { &[] };\n            let mut reader = dynspire::slots::SlotReader::new(in_data);\n",
            );
            for p in &m.params {
                block.push_str(&format!(
                    "            let {}: {} = dynspire::slots::decode_param(&mut reader);\n",
                    p.name, p.ty.rust_type(),
                ));
            }
            block
        };

        let null_check = "            if state_handle.is_null() {\n                return dynspire::slots::write_to_ffi(\n                    Err::<(), String>(\"null handle\".to_string()),\n                    out_slots, out_capacity,\n                );\n            }\n";

        let state_cast = "            let state = &*(state_handle as *const $state);\n";

        let param_names: Vec<String> = std::iter::once("state".to_string())
            .chain(m.params.iter().map(|p| p.name.clone()))
            .collect();
        let call_args = format!("({})", param_names.join(", "));

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
            dynspire::slots::write_to_ffi(_result, out_slots, out_capacity)
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
        assert!(code.contains("SlotEncode for CompressionReport"));
        assert!(code.contains("SlotEncode for Tone"));
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
        assert!(code.contains("SlotEnumDescriptor for Tone"));
    }

    #[test]
    fn generated_code_dispatch_uses_correct_types() {
        let iface = parse_rle();
        let code = generate(&iface);
        // compress dispatch should decode &[u8]
        assert!(code.contains("let data: &[u8] = dynspire::slots::decode_param"));
        // delay dispatch should decode u64
        assert!(code.contains("let ms: u64 = dynspire::slots::decode_param"));
    }

    #[test]
    fn generated_code_tower_passes_correct_args() {
        let iface = parse_rle();
        let code = generate(&iface);
        // compress has 1 param: (data,)
        assert!(code.contains("self.client.call(RleOp::Compress, (data,))"));
        // compress_into has 2 params: (data, out)
        assert!(code.contains("self.client.call(RleOp::CompressInto, (data, out))"));
        // delay has 1 param: (ms,)
        assert!(code.contains("self.client.call(RleOp::Delay, (ms,))"));
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
        assert!(code.contains("SlotEncode for SharedHandle"));
        assert!(code.contains("pub struct SharedConfig"));
        assert!(code.contains("pub enum SharedStatus"));
    }
}
