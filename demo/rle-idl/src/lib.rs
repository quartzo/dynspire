use dynspire_macro::{modulo_interface, slot_enum, slot_struct};

#[slot_struct]
#[derive(Clone, Debug, PartialEq)]
pub struct CompressionReport {
    pub original_size: u64,
    pub compressed_size: u64,
    pub ratio: f64,
    pub runs: u64,
}

#[slot_enum]
#[derive(Clone, Debug, PartialEq)]
pub enum Tone {
    Quiet,
    Normal,
    Loud(u8),
}

#[modulo_interface(enums(Tone))]
pub trait RleEngine {
    fn compress(&self, data: &[u8]) -> Result<Vec<u8>, String>;
    fn decompress(&self, data: &[u8]) -> Result<Vec<u8>, String>;
    fn compress_into(&self, data: &[u8], out: &mut Vec<u8>) -> Result<(), String>;
    fn stats(&self, data: &[u8]) -> Result<(u64, u64), String>;
    fn analyze(&self, data: &[u8]) -> Result<CompressionReport, String>;
    fn report_summary(&self, report: CompressionReport) -> Result<String, String>;
    fn run_labels(&self, data: &[u8]) -> Result<Vec<String>, String>;
    fn split_runs(&self, data: &[u8]) -> Result<Vec<Vec<u8>>, String>;
    fn compress_into_checked(&self, data: &[u8], out: &mut Vec<u8>) -> Result<bool, String>;
    fn first_byte(&self, data: &[u8]) -> Result<Option<u8>, String>;
    fn classify(&self, data: &[u8]) -> Result<Tone, String>;
    fn describe_tone(&self, tone: Tone) -> Result<String, String>;
    fn delay(&self, ms: u64) -> Result<(), String>;
}
