//! DynSpire IDL compiler — parses `.dspi` files and generates Rust code.
//!
//! ## Quick start
//!
//! In your spier's `build.rs`:
//!
//! ```ignore
//! fn main() {
//!     let mut ctx = dynspire_codegen::BuildContext::new();
//!     ctx.build_spier("src/my_interface.dspi");
//! }
//! ```
//!
//! In your host's `build.rs` (pointing at the same `.dspi`):
//!
//! ```ignore
//! fn main() {
//!     let mut ctx = dynspire_codegen::BuildContext::new();
//!     ctx.build_host("../my-spier/src/my_interface.dspi");
//! }
//! ```
//!
//! For multiple interfaces that share types:
//!
//! ```ignore
//! fn main() {
//!     let mut ctx = dynspire_codegen::BuildContext::new();
//!     ctx.build_spier("src/a.dspi");
//!     ctx.build_spier("src/b.dspi"); // shared types from a.dspi are skipped
//! }
//! ```
//!
//! In your spier's `lib.rs`:
//!
//! ```ignore
//! include!(concat!(env!("OUT_DIR"), "/my_spier.rs"));
//! ```
//!
//! In your host's `lib.rs`:
//!
//! ```ignore
//! include!(concat!(env!("OUT_DIR"), "/my_host.rs"));
//! ```
//!
//! The spier side contains the trait, types, Op enum, hash, and spier dispatch macro.
//! The host side contains the trait, types, Op enum, hash, IDL descriptor, and tower client.

pub mod ast;
mod gen;
mod lexer;
mod parser;

pub use ast::*;
pub use gen::{generate, generate_host, generate_python, generate_spier, BuildContext};
pub use parser::{parse, parse_type_fragment, validate, ParseError};
