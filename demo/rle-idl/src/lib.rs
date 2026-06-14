use dynspire_macro::modulo_interface;

#[modulo_interface]
pub trait RleEngine {
    fn compress(&self, data: &[u8]) -> Result<Vec<u8>, String>;
    fn decompress(&self, data: &[u8]) -> Result<Vec<u8>, String>;
    fn compress_into(&self, data: &[u8], out: &mut Vec<u8>) -> Result<(), String>;
    fn stats(&self, data: &[u8]) -> Result<(u64, u64), String>;
}
