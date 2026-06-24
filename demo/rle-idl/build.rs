fn main() {
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/gen.rs");
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/ast.rs");
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/parser.rs");
    dynspire_codegen::build("src/rle.dspi");
}
