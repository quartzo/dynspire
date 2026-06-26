fn main() {
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/gen.rs");
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/ast.rs");
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/parser.rs");
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/lexer.rs");
    dynspire_codegen::build_spier("src/rle.dspi");
}
