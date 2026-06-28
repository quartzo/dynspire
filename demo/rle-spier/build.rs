fn main() {
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/gen.rs");
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/ast.rs");
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/parser.rs");
    println!("cargo:rerun-if-changed=../../dynspire-codegen/src/lexer.rs");
    let mut ctx = dynspire_codegen::BuildContext::new();
    ctx.build_spier("src/rle.dspi");
    ctx.build_python("src/rle.dspi", "generated/rle.py");
}
