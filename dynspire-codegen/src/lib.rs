//! DynSpire IDL compiler — parses `.dspi` files and generates Rust code.
//!
//! ## Quick start
//!
//! In your IDL crate's `build.rs`:
//!
//! ```ignore
//! fn main() {
//!     dynspire_codegen::build("src/my_interface.dspi");
//! }
//! ```
//!
//! In your IDL crate's `lib.rs`:
//!
//! ```ignore
//! include!(concat!(env!("OUT_DIR"), "/my_idl.rs"));
//! ```
//!
//! The generated code contains the trait, types, Op enum, schema, tower
//! client wrapper, and spier dispatch macro.

pub mod ast;
mod gen;
mod lexer;
mod parser;

pub use ast::*;
pub use gen::{build, generate};
pub use parser::{parse, parse_type_fragment, validate, ParseError};
