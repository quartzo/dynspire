fn main() {
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/gen.rs");
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/ast.rs");
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/parser.rs");
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/lexer.rs");
    println!("cargo:rerun-if-changed=../rle-spier/src/rle.dspi");
    let mut ctx = dynspire_codegen::BuildContext::new();
    ctx.build_host("../rle-spier/src/rle.dspi");
}
