use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    parse_macro_input, FnArg, Ident, ItemEnum, ItemFn, ItemTrait, Type,
};

/// Generates the IDL infrastructure for a spier trait.
///
/// Given a trait named `{Name}Engine`, produces:
///
/// - `{PREFIX}_IDL_HASH: u64` — FNV-1a hash of the canonical method signatures.
///   Used by the host to verify spier compatibility at load time.
/// - `pub static IDL: IdlDescriptor` — bundle of hash + method names for
///   `DynSpireClient::connect()`.
/// - `{Prefix}Op` — `#[repr(u8)]` enum with one variant per method
///   (snake_case → PascalCase). Pass to `client.call(Op::Method, args)`.
/// - `pub mod tower` — internal schema statics (`IDL_TYPE_TABLE`,
///   `IDL_METHODS`, `IDL_SCHEMA`).
/// - `idl_schema()` — returns `&'static DynSpireIdl`.
///
/// The prefix is derived from the trait name by stripping an `Engine` suffix.
/// For example, `RleEngine` → prefix `Rle` → `RLE_IDL_HASH`, `RleOp`, `IDL`.
/// Without the suffix, the full trait name is used.
///
/// # Example
///
/// ```ignore
/// #[modulo_interface]
/// pub trait RleEngine {
///     fn compress(&self, data: &[u8]) -> Result<Vec<u8>, String>;
/// }
///
/// // Generated:
/// //   pub const RLE_IDL_HASH: u64 = ...;
/// //   pub static IDL: IdlDescriptor = ...;
/// //   pub enum RleOp { Compress, ... }
/// ```
///
/// # Usage in hosts and spiers
///
/// IDL crate (shared):
/// ```ignore
/// #[modulo_interface]
/// pub trait MyEngine { ... }
/// ```
///
/// Host:
/// ```ignore
/// let client = DynSpireClient::connect("my_spier", &my_idl::IDL, &config)?;
/// let result: Vec<u8> = client.call(MyOp::DoThing, (&input[..]))?;
/// ```
///
/// Spier:
/// ```ignore
/// #[spier_dispatch(name = "my", idl = my_idl::MY_IDL_HASH)]
/// impl MyEngine for MyState { ... }
/// ```
///
/// Accepts an optional `enums(Name1, Name2, ...)` attribute to register
/// `#[slot_enum]` types that are not directly referenced by any method.
#[proc_macro_attribute]
pub fn modulo_interface(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut item_trait = parse_macro_input!(item as ItemTrait);

    let extra_enums = parse_extra_enum_names(&attr.to_string());

    let mut variant_names: Vec<syn::Ident> = Vec::new();
    let mut method_refs: Vec<&syn::TraitItemFn> = Vec::new();

    for trait_item in &mut item_trait.items {
        if let syn::TraitItem::Fn(ref mut method) = trait_item {
            let pascal = snake_to_pascal(&method.sig.ident.to_string());
            variant_names.push(syn::Ident::new(&pascal, proc_macro2::Span::call_site()));
            method_refs.push(method);
        }
    }

    item_trait.supertraits.push(syn::parse_quote!(Send));
    item_trait.supertraits.push(syn::parse_quote!(Sync));

    let trait_name = item_trait.ident.to_string();
    let sig = build_canonical_sig(&trait_name, &method_refs);
    let idl_hash = fnv1a_64(sig.as_bytes());
    let idl_hash_lit = syn::LitInt::new(&format!("{idl_hash}"), proc_macro2::Span::call_site());

    let prefix = trait_name.strip_suffix("Engine").unwrap_or(&trait_name);
    let op_ident = Ident::new(&format!("{prefix}Op"), proc_macro2::Span::call_site());
    let hash_name = format!("{}_IDL_HASH", prefix.to_uppercase());
    let hash_ident = Ident::new(&hash_name, proc_macro2::Span::call_site());

    let discriminants: Vec<syn::LitInt> = (0..variant_names.len())
        .map(|i| syn::LitInt::new(&format!("{i}"), proc_macro2::Span::call_site()))
        .collect();
    let from_u8_arms = discriminants.iter().zip(&variant_names).map(|(d, name)| {
        quote! { #d => Some(Self::#name) }
    });

    let mut tower_method_names = Vec::new();

    for method in method_refs.iter() {
        let method_name_str = method.sig.ident.to_string();
        let method_name_lit = syn::LitStr::new(&method_name_str, proc_macro2::Span::call_site());
        tower_method_names.push(method_name_lit);
    }

    let mut type_table = TypeTableBuilder::new();
    let mut schema_methods: Vec<TokenStream2> = Vec::new();
    let mut free_types: Vec<(syn::LitInt, Type)> = Vec::new();

    for method in method_refs.iter() {
        let method_name_str = method.sig.ident.to_string();
        let name_bytes = format!("b\"{}\\0\"", method_name_str);
        let name_lit: TokenStream2 = name_bytes.parse().unwrap();

        let params = extract_method_params(&method.sig);
        let mut param_entries: Vec<TokenStream2> = Vec::new();

        for (pname, ptype) in &params {
            let pidx = type_table.add(ptype) as u32;
            let pn = pname.to_string();
            let pn_bytes = format!("b\"{}\\0\"", pn);
            let pn_lit: TokenStream2 = pn_bytes.parse().unwrap();
            param_entries.push(quote! {
                dynspire::ffi::IdlParam { name: #pn_lit.as_ptr(), name_len: #pn.len(), type_idx: #pidx }
            });
        }

        let return_type: Type = match &method.sig.output {
            syn::ReturnType::Type(_, ty) => *ty.clone(),
            syn::ReturnType::Default => syn::parse_quote!(()),
        };
        let ok_type = extract_ok_type(&return_type).unwrap_or_else(|| syn::parse_quote!(()));
        let ret_idx = type_table.add(&ok_type) as u32;
        let ret_idx_lit = syn::LitInt::new(&format!("{ret_idx}"), proc_macro2::Span::call_site());
        free_types.push((ret_idx_lit, ok_type.clone()));

        let pc = param_entries.len();

        if param_entries.is_empty() {
            schema_methods.push(quote! {
                dynspire::ffi::IdlMethod {
                    name: #name_lit.as_ptr(), name_len: #method_name_str.len(),
                    params: std::ptr::null(), param_count: 0,
                    return_type_idx: #ret_idx, _pad: [0; 4],
                }
            });
        } else {
            schema_methods.push(quote! {
                dynspire::ffi::IdlMethod {
                    name: #name_lit.as_ptr(), name_len: #method_name_str.len(),
                    params: [#(#param_entries),*].as_ptr(), param_count: #pc,
                    return_type_idx: #ret_idx, _pad: [0; 4],
                }
            });
        }
    }

    let type_nodes = type_table.nodes;
    let type_count = type_nodes.len();
    let method_count = schema_methods.len();

    let free_idxs: Vec<&syn::LitInt> = free_types.iter().map(|(i, _)| i).collect();
    let free_tys: Vec<&Type> = free_types.iter().map(|(_, t)| t).collect();

    // Add extra enums (not referenced by any method but exported via attribute)
    let mut enum_type_names = type_table.enum_type_names.clone();
    for name in &extra_enums {
        if !enum_type_names.contains(name) {
            enum_type_names.push(name.clone());
        }
    }
    let enum_count = enum_type_names.len();
    let enum_desc_idents: Vec<Ident> = enum_type_names.iter()
        .map(|name| Ident::new(
            &format!("__SLOT_ENUM_DESCRIPTOR_{}", name),
            proc_macro2::Span::call_site(),
        ))
        .collect();

    let struct_type_names = &type_table.struct_type_names;
    let struct_count = struct_type_names.len();
    let struct_desc_idents: Vec<Ident> = struct_type_names.iter()
        .map(|name| Ident::new(
            &format!("__SLOT_STRUCT_DESCRIPTOR_{}", name),
            proc_macro2::Span::call_site(),
        ))
        .collect();
    let struct_name_lits: Vec<TokenStream2> = struct_type_names.iter()
        .map(|name| format!("b\"{}\\0\"", name).parse::<TokenStream2>().unwrap())
        .collect();
    let struct_name_lens: Vec<usize> = struct_type_names.iter().map(|n| n.len()).collect();

    let expanded = quote! {
        #[allow(dead_code)]
        #item_trait

        pub const #hash_ident: u64 = #idl_hash_lit;

        pub fn idl_schema() -> &'static dynspire::ffi::DynSpireIdl {
            &tower::IDL_SCHEMA
        }

        #[repr(u8)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum #op_ident {
            #(
                #variant_names = #discriminants,
            )*
        }

        impl #op_ident {
            pub fn from_u8(v: u8) -> Option<Self> {
                match v {
                    #(#from_u8_arms,)*
                    _ => None,
                }
            }
        }

        impl dynspire::SpierOp for #op_ident {
            fn as_index(self) -> usize { self as usize }
        }

        #(
            #[doc(hidden)]
            pub static #struct_desc_idents: dynspire::ffi::StructDescriptor = dynspire::ffi::StructDescriptor {
                name: #struct_name_lits.as_ptr(),
                name_len: #struct_name_lens,
            };
        )*

        pub mod tower {
            use super::*;

            pub const METHODS: &[&str] = &[#(#tower_method_names),*];

            pub static IDL_TYPE_TABLE: &[dynspire::ffi::IdlTypeNode] = &[
                #(#type_nodes),*
            ];

            pub static IDL_METHODS: &[dynspire::ffi::IdlMethod] = &[
                #(#schema_methods),*
            ];

            pub static IDL_ENUM_PTRS: &[&'static dynspire::ffi::EnumDescriptor] = &[
                #( &(super::#enum_desc_idents) ),*
            ];

            pub static IDL_STRUCT_PTRS: &[&'static dynspire::ffi::StructDescriptor] = &[
                #( &(super::#struct_desc_idents) ),*
            ];

            pub static IDL_SCHEMA: dynspire::ffi::DynSpireIdl = dynspire::ffi::DynSpireIdl {
                name: b"\0".as_ptr(),
                name_len: 0,
                hash: #idl_hash_lit,
                type_table: IDL_TYPE_TABLE.as_ptr(),
                type_count: #type_count,
                methods: IDL_METHODS.as_ptr(),
                method_count: #method_count,
                enum_table: IDL_ENUM_PTRS.as_ptr() as *const *const dynspire::ffi::EnumDescriptor,
                enum_count: #enum_count,
                struct_table: IDL_STRUCT_PTRS.as_ptr() as *const *const dynspire::ffi::StructDescriptor,
                struct_count: #struct_count,
                free_fn: super::dynspire_free,
            };
        }

        pub static IDL: dynspire::IdlDescriptor = dynspire::IdlDescriptor {
            hash: #idl_hash_lit,
            methods: tower::METHODS,
        };

        pub unsafe extern "C" fn dynspire_free(
            type_index: u32,
            slots: *const u64,
            slot_count: usize,
        ) {
            if slots.is_null() || slot_count == 0 { return; }
            let slice = std::slice::from_raw_parts(slots, slot_count);
            let mut r = dynspire::slots::SlotReader::new(slice);
            let tag = r.read_u64();
            if tag == 1 {
                let _: String = dynspire::slots::SlotReceive::from_slots(&mut r);
                return;
            }
            match type_index {
                #(
                    #free_idxs => {
                        let _: #free_tys = dynspire::slots::SlotReceive::from_slots(&mut r);
                    }
                )*
                _ => {}
            }
        }

    };
    TokenStream::from(expanded)
}

fn extract_ok_type(ty: &Type) -> Option<Type> {
    if let Type::Path(tp) = ty {
        if let Some(seg) = tp.path.segments.last() {
            if seg.ident == "Result" {
                if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                    if let Some(syn::GenericArgument::Type(ok_ty)) = args.args.first() {
                        return Some(ok_ty.clone());
                    }
                }
            }
        }
    }
    None
}

fn build_canonical_sig(trait_name: &str, methods: &[&syn::TraitItemFn]) -> String {
    let mut parts = Vec::new();
    for m in methods {
        let name = m.sig.ident.to_string();
        let params: String = m.sig.inputs.iter()
            .filter_map(|arg| {
                if let syn::FnArg::Typed(pt) = arg {
                    let ty = &*pt.ty;
                    Some(clean_type_str(&quote!(#ty).to_string()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        let ret = match &m.sig.output {
            syn::ReturnType::Default => "()".to_string(),
            syn::ReturnType::Type(_, t) => clean_type_str(&quote!(#t).to_string()),
        };
        parts.push(format!("{}({})->{}", name, params, ret));
    }
    format!("{}{{{}}}", trait_name, parts.join(","))
}

fn clean_type_str(s: &str) -> String {
    s.replace(" ", "")
}

fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn snake_to_pascal(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect()
}

fn extract_method_params(sig: &syn::Signature) -> Vec<(Ident, Type)> {
    sig.inputs
        .iter()
        .filter_map(|arg| {
            if let FnArg::Typed(pt) = arg {
                if let syn::Pat::Ident(pi) = &*pt.pat {
                    if pi.ident != "self" {
                        return Some((pi.ident.clone(), *pt.ty.clone()));
                    }
                }
            }
            None
        })
        .collect()
}

struct TypeTableBuilder {
    nodes: Vec<TokenStream2>,
    enum_type_names: Vec<String>,
    struct_type_names: Vec<String>,
}

impl TypeTableBuilder {
    fn new() -> Self {
        Self { nodes: Vec::new(), enum_type_names: Vec::new(), struct_type_names: Vec::new() }
    }

    fn add(&mut self, ty: &Type) -> usize {
        match ty {
            Type::Tuple(t) if t.elems.is_empty() => {
                let idx = self.nodes.len();
                self.nodes.push(quote! {
                    dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_UNIT, _pad: [0; 3], size: 0, child0: -1, child1: -1 }
                });
                idx
            }
            Type::Path(tp) => {
                let seg = tp.path.segments.last().unwrap();
                match seg.ident.to_string().as_str() {
                    "bool" => {
                        let idx = self.nodes.len();
                        self.nodes.push(quote! {
                            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_BOOL, _pad: [0; 3], size: 0, child0: -1, child1: -1 }
                        });
                        idx
                    }
                    "u64" => {
                        let idx = self.nodes.len();
                        self.nodes.push(quote! {
                            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_U64, _pad: [0; 3], size: 0, child0: -1, child1: -1 }
                        });
                        idx
                    }
                    "i8" | "u8" => {
                        let idx = self.nodes.len();
                        self.nodes.push(quote! {
                            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_U8, _pad: [0; 3], size: 0, child0: -1, child1: -1 }
                        });
                        idx
                    }
                    "i16" | "u16" | "i32" | "u32" => {
                        let idx = self.nodes.len();
                        self.nodes.push(quote! {
                            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_U32, _pad: [0; 3], size: 0, child0: -1, child1: -1 }
                        });
                        idx
                    }
                    "i64" | "isize" | "usize" => {
                        let idx = self.nodes.len();
                        self.nodes.push(quote! {
                            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_U64, _pad: [0; 3], size: 0, child0: -1, child1: -1 }
                        });
                        idx
                    }
                    "f32" => {
                        let idx = self.nodes.len();
                        self.nodes.push(quote! {
                            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_U32, _pad: [0; 3], size: 0, child0: -1, child1: -1 }
                        });
                        idx
                    }
                    "f64" => {
                        let idx = self.nodes.len();
                        self.nodes.push(quote! {
                            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_U64, _pad: [0; 3], size: 0, child0: -1, child1: -1 }
                        });
                        idx
                    }
                    "String" => {
                        let idx = self.nodes.len();
                        self.nodes.push(quote! {
                            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_STRING, _pad: [0; 3], size: 0, child0: -1, child1: -1 }
                        });
                        idx
                    }
                    "Value" | "ValueType" => {
                        let name = seg.ident.to_string();
                        let enum_idx = if let Some(pos) = self.enum_type_names.iter().position(|n| n == &name) {
                            pos
                        } else {
                            self.enum_type_names.push(name);
                            self.enum_type_names.len() - 1
                        };
                        let idx = self.nodes.len();
                        let enum_idx_i32 = enum_idx as i32;
                        self.nodes.push(quote! {
                            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_ENUM, _pad: [0; 3], size: 0, child0: #enum_idx_i32, child1: -1 }
                        });
                        idx
                    }
                    "Option" => {
                        if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                            if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                                let child = self.add(inner);
                                let idx = self.nodes.len();
                                let c = child as i32;
                                self.nodes.push(quote! {
                                    dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_OPTION, _pad: [0; 3], size: 0, child0: #c, child1: -1 }
                                });
                                return idx;
                            }
                        }
                        self.add_unit()
                    }
                    "Vec" => {
                        if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                            if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                                let child = self.add(inner);
                                let idx = self.nodes.len();
                                let c = child as i32;
                                self.nodes.push(quote! {
                                    dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_VEC, _pad: [0; 3], size: 0, child0: #c, child1: -1 }
                                });
                                return idx;
                            }
                        }
                        self.add_unit()
                    }
                    _ => self.add_struct(seg.ident.to_string()),
                }
            }
            Type::Reference(r) => {
                match r.elem.as_ref() {
                    Type::Path(tp) if tp.path.is_ident("str") => {
                        let idx = self.nodes.len();
                        self.nodes.push(quote! {
                            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_STR, _pad: [0; 3], size: 0, child0: -1, child1: -1 }
                        });
                        idx
                    }
                    Type::Slice(inner) => {
                        let child = self.add(&inner.elem);
                        let idx = self.nodes.len();
                        let c = child as i32;
                        self.nodes.push(quote! {
                            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_SLICE, _pad: [0; 3], size: 0, child0: #c, child1: -1 }
                        });
                        idx
                    }
                    Type::Path(tp)
                        if tp.path.segments.last().is_some_and(|s| s.ident == "Vec")
                            && r.mutability.is_some() =>
                    {
                        let idx = self.nodes.len();
                        self.nodes.push(quote! {
                            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_OUT_VEC, _pad: [0; 3], size: 0, child0: -1, child1: -1 }
                        });
                        idx
                    }
                    _ => self.add(r.elem.as_ref()),
                }
            }
            Type::Array(a) => {
                let child = self.add(&a.elem);
                let len_lit = match &a.len {
                    syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(lit), .. }) => lit.base10_digits().parse::<u32>().unwrap(),
                    _ => 0,
                };
                let idx = self.nodes.len();
                let c = child as i32;
                self.nodes.push(quote! {
                    dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_ARRAY, _pad: [0; 3], size: #len_lit, child0: #c, child1: -1 }
                });
                idx
            }
            Type::Tuple(t) => {
                let elems: Vec<&Type> = t.elems.iter().collect();
                if elems.len() == 2 {
                    let c0 = self.add(elems[0]);
                    let c1 = self.add(elems[1]);
                    let idx = self.nodes.len();
                    let a = c0 as i32;
                    let b = c1 as i32;
                    self.nodes.push(quote! {
                        dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_TUPLE, _pad: [0; 3], size: 0, child0: #a, child1: #b }
                    });
                    idx
                } else {
                    self.add_unit()
                }
            }
            _ => self.add_unit(),
        }
    }

    fn add_unit(&mut self) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(quote! {
            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_UNIT, _pad: [0; 3], size: 0, child0: -1, child1: -1 }
        });
        idx
    }

    fn add_struct(&mut self, name: String) -> usize {
        let struct_idx = if let Some(pos) = self.struct_type_names.iter().position(|n| n == &name) {
            pos
        } else {
            self.struct_type_names.push(name);
            self.struct_type_names.len() - 1
        };
        let idx = self.nodes.len();
        let struct_idx_i32 = struct_idx as i32;
        self.nodes.push(quote! {
            dynspire::ffi::IdlTypeNode { kind: dynspire::ffi::IDL_STRUCT, _pad: [0; 3], size: 0, child0: #struct_idx_i32, child1: -1 }
        });
        idx
    }
}

fn classify_return_type(ty: &Type) -> Option<Type> {
    if let Type::Path(tp) = ty {
        if let Some(seg) = tp.path.segments.last() {
            if seg.ident == "Result" {
                if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                    if let Some(syn::GenericArgument::Type(ok_ty)) = args.args.first() {
                        return Some(ok_ty.clone());
                    }
                }
            }
        }
    }
    None
}

/// Generates the C-ABI dispatch functions for a spier implementation.
///
/// Applied to `impl Trait for State`. For each method in the trait, generates
/// a `dynspire_dispatch_{method}` extern "C" function that decodes arguments
/// from slots, calls the method, and encodes the `Result<T, String>` return.
///
/// Also generates `dynspire_idl_hash()`, `dynspire_spier_name()`, and
/// `dynspire_idl_schema()`.
///
/// # Attributes
///
/// - `name = "..."` — the spier name (used for discovery).
/// - `idl = path::to::HASH` — the IDL hash constant from the IDL crate
///   (e.g., `my_idl::MY_IDL_HASH`).
///
/// # Example
///
/// ```ignore
/// #[spier_dispatch(name = "my", idl = my_idl::MY_IDL_HASH)]
/// impl MyEngine for MyState {
///     fn do_thing(&self, data: &[u8]) -> Result<Vec<u8>, String> { ... }
/// }
/// ```
#[proc_macro_attribute]
pub fn spier_dispatch(attr: TokenStream, item: TokenStream) -> TokenStream {
    spier_dispatch_impl(attr, item)
}

fn spier_dispatch_impl(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item_impl = parse_macro_input!(item as syn::ItemImpl);

    let (trait_path, state_type) = match (&item_impl.trait_, &item_impl.self_ty) {
        (Some((_, path, _)), ty) => (path.clone(), ty.as_ref().clone()),
        _ => {
            return syn::Error::new_spanned(
                &item_impl,
                "#[spier_dispatch] expects impl Trait for State",
            )
            .to_compile_error()
            .into();
        }
    };

    let attrs = parse_dispatch_attrs(attr);
    let spier_name_bytes = attrs.name;
    let idl_expr = attrs.idl;

    let idl_hash_ident = {
        let idl_str = idl_expr.to_string();
        let parent = if idl_str.contains("::") {
            let p = idl_str.trim_end_matches(|c: char| c != ':');
            p.trim_end_matches(':')
        } else {
            ""
        };
        if parent.is_empty() {
            quote! { idl_schema() }
        } else {
            let path: TokenStream2 = format!("{}::idl_schema()", parent).parse().unwrap();
            quote! { #path }
        }
    };

    let mut dispatch_fns = Vec::new();

    for impl_item in &item_impl.items {
        if let syn::ImplItem::Fn(method) = impl_item {
            let method_ident = &method.sig.ident;
            let method_name_str = method_ident.to_string();

            let fn_name = Ident::new(
                &format!("dynspire_dispatch_{method_name_str}"),
                proc_macro2::Span::call_site(),
            );

            let params = extract_method_params(&method.sig);
            let return_type: Type = match &method.sig.output {
                syn::ReturnType::Type(_, ty) => *ty.clone(),
                syn::ReturnType::Default => syn::parse_quote!(()),
            };
            if classify_return_type(&return_type).is_none() {
                return syn::Error::new_spanned(
                    method,
                    "dispatch methods must return Result<T, String>",
                )
                .to_compile_error()
                .into();
            }

            let param_names: Vec<&Ident> = params.iter().map(|(n, _)| n).collect();

            let decode_block = if params.is_empty() {
                quote! {}
            } else {
                let decode_stmts: Vec<TokenStream2> = params.iter().map(|(name, ty)| {
                    quote! { let #name: #ty = dynspire::slots::decode_param(&mut reader); }
                }).collect();
                quote! {
                    let in_data = if !in_slots.is_null() && in_count > 0 {
                        unsafe { std::slice::from_raw_parts(in_slots, in_count) }
                    } else { &[] };
                    let mut reader = dynspire::slots::SlotReader::new(in_data);
                    #(#decode_stmts)*
                }
            };

            let call_expr = quote! {
                <#state_type as #trait_path>::#method_ident(state, #(#param_names),*)
            };

            dispatch_fns.push(quote! {
                #[no_mangle]
                pub unsafe extern "C" fn #fn_name(
                    state_handle: *mut std::ffi::c_void,
                    in_slots: *const u64,
                    in_count: usize,
                    out_slots: *mut u64,
                    out_capacity: usize,
                ) -> u8 {
                    if state_handle.is_null() {
                        return dynspire::slots::write_to_ffi(
                            Err::<(), String>("null handle".to_string()),
                            out_slots, out_capacity,
                        );
                    }
                    let state = &*(state_handle as *const #state_type);
                    #decode_block
                    let _result = #call_expr;
                    dynspire::slots::write_to_ffi(_result, out_slots, out_capacity)
                }
            });
        }
    }

    let expanded = quote! {
        #item_impl

        #[no_mangle]
        pub extern "C" fn dynspire_idl_hash() -> u64 {
            #idl_expr
        }

        #[no_mangle]
        pub extern "C" fn dynspire_spier_name() -> *const u8 {
            #spier_name_bytes.as_ptr()
        }

        #[no_mangle]
        pub extern "C" fn dynspire_idl_schema() -> *const dynspire::ffi::DynSpireIdl {
            #idl_hash_ident
        }

        #(#dispatch_fns)*
    };

    TokenStream::from(expanded)
}

struct DispatchAttrs {
    name: TokenStream2,
    idl: TokenStream2,
}

fn parse_dispatch_attrs(attr: TokenStream) -> DispatchAttrs {
    let parsed: TokenStream2 = attr.into();
    let tokens: Vec<_> = parsed.into_iter().collect();

    let mut name = None;
    let mut idl = None;

    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            proc_macro2::TokenTree::Ident(ident) if ident == "name" => {
                i += 2;
                if let Some(proc_macro2::TokenTree::Literal(lit)) = tokens.get(i) {
                    let s = lit.to_string();
                    let inner = s.trim_matches('"');
                    let bytes_with_null = format!("b\"{}\\0\"", inner);
                    name = Some(bytes_with_null.parse::<TokenStream2>().unwrap());
                }
                i += 1;
            }
            proc_macro2::TokenTree::Ident(ident) if ident == "idl" => {
                i += 2;
                let mut idl_tokens = TokenStream2::new();
                while i < tokens.len() {
                    match &tokens[i] {
                        proc_macro2::TokenTree::Punct(p) if p.as_char() == ',' => break,
                        _ => {
                            idl_tokens.extend(std::iter::once(tokens[i].clone()));
                            i += 1;
                        }
                    }
                }
                idl = Some(idl_tokens);
            }
            _ => i += 1,
        }
    }

    DispatchAttrs {
        name: name.unwrap_or_else(|| quote! { b"unknown\0" }),
        idl: idl.unwrap_or_else(|| quote! { 0u64 }),
    }
}

/// Generates `dynspire_create` and `dynspire_destroy` C-ABI entry points.
///
/// Applied to an init function that takes `&HashMap<String, String>` and
/// returns `Result<StateType, String>`. Generates:
///
/// - `dynspire_create(data_ptr, data_len) -> *mut c_void` — deserializes the
///   config kvmap, calls the init function, returns `Box::into_raw(state)`.
/// - `dynspire_destroy(handle)` — drops the boxed state.
///
/// # Example
///
/// ```ignore
/// #[spier_storage]
/// fn init(config: &HashMap<String, String>) -> Result<MyState, String> {
///     Ok(MyState { ... })
/// }
/// ```
#[proc_macro_attribute]
pub fn spier_storage(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(item as ItemFn);
    let user_fn_name = &item_fn.sig.ident;
    let fn_vis = &item_fn.vis;

    let return_type = match &item_fn.sig.output {
        syn::ReturnType::Type(_, ty) => match ty.as_ref() {
            Type::Path(tp) => {
                let seg = tp.path.segments.last().unwrap();
                match &seg.arguments {
                    syn::PathArguments::AngleBracketed(args) => {
                        if let Some(syn::GenericArgument::Type(ty)) = args.args.first() {
                            Some(ty.clone())
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    };

    let state_type = match return_type {
        Some(ty) => ty,
        None => {
            return syn::Error::new_spanned(
                &item_fn.sig.output,
                "init function must return Result<StateType, String>",
            )
            .to_compile_error()
            .into()
        }
    };

    let expanded = quote! {
        #fn_vis #item_fn

        #[no_mangle]
        pub extern "C" fn dynspire_create(
            data_ptr: *const u8,
            data_len: usize,
        ) -> *mut std::ffi::c_void {
            let config = if data_ptr.is_null() || data_len == 0 {
                std::collections::HashMap::new()
            } else {
                let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };
                dynspire::deserialize_kvmap(data)
            };
            match #user_fn_name(&config) {
                Ok(state) => Box::into_raw(Box::new(state)) as *mut std::ffi::c_void,
                Err(e) => {
                    eprintln!("spier init failed: {e}");
                    std::ptr::null_mut()
                }
            }
        }

        #[no_mangle]
        pub extern "C" fn dynspire_destroy(handle: *mut std::ffi::c_void) {
            if !handle.is_null() {
                unsafe {
                    drop(Box::from_raw(handle as *mut #state_type));
                }
            }
        }
    };

    TokenStream::from(expanded)
}

/// Generates [`SlotEncode`], [`SlotDecode`], [`SlotReturn`], and [`SlotReceive`]
/// impls for an enum by flattening its variants into slots.
///
/// Each variant is encoded as `(discriminant, field0_slots, field1_slots, ...)`.
/// Also generates a static [`EnumDescriptor`] for schema reflection.
///
/// # Example
///
/// ```ignore
/// #[slot_enum]
/// pub enum Value {
///     Text(String),      // disc 0 + String slots
///     Int64(i64),        // disc 1 + i64 slot
///     Unknown(i8, u64),  // disc 2 + i8 slot + u64 slot
/// }
/// ```
///
/// [`SlotEncode`]: dynspire::slots::SlotEncode
/// [`SlotDecode`]: dynspire::slots::SlotDecode
/// [`SlotReturn`]: dynspire::slots::SlotReturn
/// [`SlotReceive`]: dynspire::slots::SlotReceive
/// [`EnumDescriptor`]: dynspire::ffi::EnumDescriptor
#[proc_macro_attribute]
pub fn slot_enum(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item_enum = parse_macro_input!(item as ItemEnum);
    slot_enum_impl(&item_enum)
        .unwrap_or_else(|e| e.to_compile_error().into())
        .into()
}

fn slot_enum_impl(item_enum: &ItemEnum) -> syn::Result<TokenStream> {
    let enum_name = &item_enum.ident;
    let (impl_generics, ty_generics, where_clause) = item_enum.generics.split_for_impl();

    let discriminants = extract_discriminants(item_enum);

    let mut encode_arms: Vec<TokenStream2> = Vec::new();
    let mut decode_arms: Vec<TokenStream2> = Vec::new();
    let mut return_arms: Vec<TokenStream2> = Vec::new();
    let mut receive_arms: Vec<TokenStream2> = Vec::new();

    for (variant, &disc) in item_enum.variants.iter().zip(&discriminants) {
        let disc_lit = syn::LitInt::new(&format!("{disc}"), proc_macro2::Span::call_site());
        let var_ident = &variant.ident;

        match &variant.fields {
            syn::Fields::Unit => {
                encode_arms.push(quote! {
                    #enum_name #ty_generics::#var_ident => { w.write_u64(#disc_lit); }
                });
                decode_arms.push(quote! { #disc_lit => #enum_name #ty_generics::#var_ident, });
                return_arms.push(quote! {
                    #enum_name #ty_generics::#var_ident => { w.write_u64(#disc_lit); }
                });
                receive_arms.push(quote! { #disc_lit => #enum_name #ty_generics::#var_ident, });
            }
            syn::Fields::Unnamed(fields) => {
                let field_count = fields.unnamed.len();
                let names: Vec<Ident> = (0..field_count)
                    .map(|i| Ident::new(&format!("f{i}"), proc_macro2::Span::call_site()))
                    .collect();
                let types: Vec<&Type> = fields.unnamed.iter().map(|f| &f.ty).collect();

                encode_arms.push(quote! {
                    #enum_name #ty_generics::#var_ident(#(ref #names),*) => {
                        w.write_u64(#disc_lit);
                        #(<#types as dynspire::slots::SlotEncode>::encode(#names, w);)*
                    }
                });

                let decode_fields: Vec<TokenStream2> = types.iter()
                    .map(|ty| quote! { <#ty as dynspire::slots::SlotDecode>::decode(r) })
                    .collect();
                decode_arms.push(quote! {
                    #disc_lit => #enum_name #ty_generics::#var_ident(#(#decode_fields),*),
                });

                return_arms.push(quote! {
                    #enum_name #ty_generics::#var_ident(#(#names),*) => {
                        w.write_u64(#disc_lit);
                        #(<#types as dynspire::slots::SlotReturn>::into_slots(#names, w);)*
                    }
                });

                let receive_fields: Vec<TokenStream2> = types.iter()
                    .map(|ty| quote! { <#ty as dynspire::slots::SlotReceive>::from_slots(r) })
                    .collect();
                receive_arms.push(quote! {
                    #disc_lit => #enum_name #ty_generics::#var_ident(#(#receive_fields),*),
                });
            }
            syn::Fields::Named(fields) => {
                let names: Vec<&Option<Ident>> = fields.named.iter().map(|f| &f.ident).collect();
                let types: Vec<&Type> = fields.named.iter().map(|f| &f.ty).collect();

                let ref_names: Vec<TokenStream2> = names.iter()
                    .map(|n| quote! { ref #n })
                    .collect();

                encode_arms.push(quote! {
                    #enum_name #ty_generics::#var_ident { #(#ref_names),* } => {
                        w.write_u64(#disc_lit);
                        #(<#types as dynspire::slots::SlotEncode>::encode(#names, w);)*
                    }
                });

                let decode_pairs: Vec<TokenStream2> = names.iter().zip(types.iter())
                    .map(|(n, ty)| quote! { #n: <#ty as dynspire::slots::SlotDecode>::decode(r) })
                    .collect();
                decode_arms.push(quote! {
                    #disc_lit => #enum_name #ty_generics::#var_ident { #(#decode_pairs),* },
                });

                return_arms.push(quote! {
                    #enum_name #ty_generics::#var_ident { #(#names),* } => {
                        w.write_u64(#disc_lit);
                        #(<#types as dynspire::slots::SlotReturn>::into_slots(#names, w);)*
                    }
                });

                let receive_pairs: Vec<TokenStream2> = names.iter().zip(types.iter())
                    .map(|(n, ty)| quote! { #n: <#ty as dynspire::slots::SlotReceive>::from_slots(r) })
                    .collect();
                receive_arms.push(quote! {
                    #disc_lit => #enum_name #ty_generics::#var_ident { #(#receive_pairs),* },
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // Enum descriptor: build a TypeTableBuilder for field types, then generate
    // statics for the descriptor. Naming convention: __SLOT_ENUM_*_{EnumName}
    // -----------------------------------------------------------------------
    let enum_name_str = enum_name.to_string();
    let mut desc_tt = TypeTableBuilder::new();
    let mut all_field_indices: Vec<u32> = Vec::new();
    let mut variant_entries: Vec<TokenStream2> = Vec::new();

    for (variant, &disc) in item_enum.variants.iter().zip(&discriminants) {
        let disc_u32 = disc as u32;
        let var_name_str = variant.ident.to_string();
        let var_name_bytes = format!("b\"{}\\0\"", var_name_str);
        let var_name_lit: TokenStream2 = var_name_bytes.parse().unwrap();
        let field_offset = all_field_indices.len() as u32;

        let field_types: Vec<u32> = match &variant.fields {
            syn::Fields::Unit => vec![],
            syn::Fields::Unnamed(fields) => {
                fields.unnamed.iter().map(|f| desc_tt.add(&f.ty) as u32).collect()
            }
            syn::Fields::Named(fields) => {
                fields.named.iter().map(|f| desc_tt.add(&f.ty) as u32).collect()
            }
        };
        let field_count = field_types.len() as u32;
        all_field_indices.extend(field_types);

        variant_entries.push(quote! {
            dynspire::ffi::EnumVariantDesc {
                disc: #disc_u32,
                name: #var_name_lit.as_ptr(),
                name_len: #var_name_str.len(),
                field_count: #field_count,
                field_type_offset: #field_offset,
            }
        });
    }

    let desc_type_nodes = &desc_tt.nodes;
    let desc_type_count = desc_tt.nodes.len();
    let field_type_count = all_field_indices.len();
    let variant_count = item_enum.variants.len();

    let types_static = Ident::new(
        &format!("__SLOT_ENUM_TYPES_{}", enum_name_str),
        proc_macro2::Span::call_site(),
    );
    let field_types_static = Ident::new(
        &format!("__SLOT_ENUM_FIELD_TYPES_{}", enum_name_str),
        proc_macro2::Span::call_site(),
    );
    let variants_static = Ident::new(
        &format!("__SLOT_ENUM_VARIANTS_{}", enum_name_str),
        proc_macro2::Span::call_site(),
    );
    let desc_static = Ident::new(
        &format!("__SLOT_ENUM_DESCRIPTOR_{}", enum_name_str),
        proc_macro2::Span::call_site(),
    );
    let enum_name_bytes = format!("b\"{}\\0\"", enum_name_str);
    let enum_name_lit: TokenStream2 = enum_name_bytes.parse().unwrap();

    let expanded = quote! {
        #item_enum

        impl #impl_generics dynspire::slots::SlotEncode for #enum_name #ty_generics #where_clause {
            fn encode(&self, w: &mut dynspire::slots::SlotWriter) {
                match self {
                    #(#encode_arms)*
                }
            }
        }

        impl #impl_generics dynspire::slots::SlotDecode<'_> for #enum_name #ty_generics #where_clause {
            unsafe fn decode(r: &mut dynspire::slots::SlotReader<'_>) -> Self {
                match r.read_u64() {
                    #(#decode_arms)*
                    _ => panic!("invalid discriminant for {}", stringify!(#enum_name)),
                }
            }
        }

        impl #impl_generics dynspire::slots::SlotReturn for #enum_name #ty_generics #where_clause {
            fn into_slots(self, w: &mut dynspire::slots::SlotWriter) {
                match self {
                    #(#return_arms)*
                }
            }
        }

        impl #impl_generics dynspire::slots::SlotReceive for #enum_name #ty_generics #where_clause {
            unsafe fn from_slots(r: &mut dynspire::slots::SlotReader) -> Self {
                match r.read_u64() {
                    #(#receive_arms)*
                    _ => panic!("invalid discriminant for {}", stringify!(#enum_name)),
                }
            }
        }

        #[doc(hidden)]
        pub static #types_static: &[dynspire::ffi::IdlTypeNode] = &[
            #(#desc_type_nodes),*
        ];

        #[doc(hidden)]
        pub static #field_types_static: &[u32] = &[#(#all_field_indices),*];

        #[doc(hidden)]
        pub static #variants_static: &[dynspire::ffi::EnumVariantDesc] = &[
            #(#variant_entries),*
        ];

        #[doc(hidden)]
        pub static #desc_static: dynspire::ffi::EnumDescriptor = dynspire::ffi::EnumDescriptor {
            name: #enum_name_lit.as_ptr(),
            name_len: #enum_name_str.len(),
            variant_count: #variant_count,
            variants: #variants_static.as_ptr(),
            type_table: #types_static.as_ptr(),
            type_count: #desc_type_count,
            field_types: #field_types_static.as_ptr(),
            field_type_count: #field_type_count,
        };

        impl #impl_generics dynspire::slots::SlotEnumDescriptor for #enum_name #ty_generics #where_clause {
            fn descriptor() -> &'static dynspire::ffi::EnumDescriptor {
                &#desc_static
            }
        }
    };

    Ok(TokenStream::from(expanded))
}

/// Generates [`SlotEncode`], [`SlotDecode`], [`SlotReturn`], and [`SlotReceive`]
/// impls for a struct using an opaque boxed pointer (1 slot).
///
/// The struct crosses the FFI boundary as `Box::into_raw` on the sender side
/// and `Box::from_raw` on the receiver side. Rust callers dereference the Box
/// and access fields natively. Python callers receive an opaque integer handle
/// and use explicit IDL methods for field access.
///
/// Requires `Clone` (used by `SlotDecode` for the input borrow pattern).
///
/// # Example
///
/// ```ignore
/// #[slot_struct]
/// #[derive(Clone)]
/// pub struct Stmt {
///     kind: u8,
///     table: String,
/// }
/// ```
///
/// The struct can then be used as a parameter type, return type, or nested
/// inside `Option`, `Result`, tuples, etc. — always consuming exactly 1 slot.
///
/// [`SlotEncode`]: dynspire::slots::SlotEncode
/// [`SlotDecode`]: dynspire::slots::SlotDecode
/// [`SlotReturn`]: dynspire::slots::SlotReturn
/// [`SlotReceive`]: dynspire::slots::SlotReceive
#[proc_macro_attribute]
pub fn slot_struct(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item_struct = parse_macro_input!(item as syn::ItemStruct);
    slot_struct_impl(&item_struct)
        .unwrap_or_else(|e| e.to_compile_error().into())
}

fn slot_struct_impl(item_struct: &syn::ItemStruct) -> syn::Result<TokenStream> {
    let struct_name = &item_struct.ident;
    let (impl_generics, ty_generics, where_clause) = item_struct.generics.split_for_impl();

    let expanded = quote! {
        #item_struct

        impl #impl_generics dynspire::slots::SlotEncode for #struct_name #ty_generics #where_clause {
            fn encode(&self, w: &mut dynspire::slots::SlotWriter) {
                w.write_u64(self as *const Self as u64);
            }
        }

        impl #impl_generics dynspire::slots::SlotDecode<'_> for #struct_name #ty_generics #where_clause {
            unsafe fn decode(r: &mut dynspire::slots::SlotReader<'_>) -> Self {
                let ptr = r.read_u64() as *const Self;
                (*ptr).clone()
            }
        }

        impl #impl_generics dynspire::slots::SlotReturn for #struct_name #ty_generics #where_clause {
            fn into_slots(self, w: &mut dynspire::slots::SlotWriter) {
                w.write_u64(Box::into_raw(Box::new(self)) as u64);
            }
        }

        impl #impl_generics dynspire::slots::SlotReceive for #struct_name #ty_generics #where_clause {
            unsafe fn from_slots(r: &mut dynspire::slots::SlotReader) -> Self {
                *Box::from_raw(r.read_u64() as *mut Self)
            }
        }
    };

    Ok(TokenStream::from(expanded))
}

/// Extract discriminant values from enum variants, respecting explicit `= N` assignments
/// and Rust's default increment rule (prev + 1, starting at 0).
fn extract_discriminants(item_enum: &ItemEnum) -> Vec<i64> {
    let mut discs = Vec::new();
    let mut next: i64 = 0;
    for variant in &item_enum.variants {
        let disc = if let Some((_, expr)) = &variant.discriminant {
            match expr {
                syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(lit), .. }) => {
                    lit.base10_parse().unwrap_or(next)
                }
                _ => next,
            }
        } else {
            next
        };
        discs.push(disc);
        next = disc + 1;
    }
    discs
}

/// Parse `enums(Name1, Name2, ...)` from the attribute string.
fn parse_extra_enum_names(attr: &str) -> Vec<String> {
    let attr = attr.trim();
    if attr.is_empty() {
        return Vec::new();
    }
    for part in attr.split(',') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("enums(").and_then(|s| s.strip_suffix(')')) {
            return rest.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    Vec::new()
}
