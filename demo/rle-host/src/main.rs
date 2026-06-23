use std::collections::HashMap;

use dynspire::DynSpireClient;
use rle_idl::{CompressionReport, RleEngine, RleOp, IDL};

pub struct DynSpireRle {
    client: DynSpireClient,
}

impl DynSpireRle {
    pub fn connect(
        spier_name: &str,
        config: &HashMap<String, String>,
    ) -> Result<Self, String> {
        let client = DynSpireClient::connect(spier_name, &IDL, config)?;
        Ok(Self { client })
    }
}

impl RleEngine for DynSpireRle {
    fn compress(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        self.client.call(RleOp::Compress, (data,))
    }
    fn decompress(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        self.client.call(RleOp::Decompress, (data,))
    }
    fn compress_into(&self, data: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        self.client.call(RleOp::CompressInto, (data, out))
    }
    fn stats(&self, data: &[u8]) -> Result<(u64, u64), String> {
        self.client.call(RleOp::Stats, (data,))
    }
    fn analyze(&self, data: &[u8]) -> Result<CompressionReport, String> {
        self.client.call(RleOp::Analyze, (data,))
    }
    fn report_summary(&self, report: CompressionReport) -> Result<String, String> {
        self.client.call(RleOp::ReportSummary, (report,))
    }
    fn run_labels(&self, data: &[u8]) -> Result<Vec<String>, String> {
        self.client.call(RleOp::RunLabels, (data,))
    }
    fn split_runs(&self, data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        self.client.call(RleOp::SplitRuns, (data,))
    }
    fn compress_into_checked(&self, data: &[u8], out: &mut Vec<u8>) -> Result<bool, String> {
        self.client.call(RleOp::CompressIntoChecked, (data, out))
    }
    fn first_byte(&self, data: &[u8]) -> Result<Option<u8>, String> {
        self.client.call(RleOp::FirstByte, (data,))
    }
    fn classify(&self, data: &[u8]) -> Result<rle_idl::Tone, String> {
        self.client.call(RleOp::Classify, (data,))
    }
    fn describe_tone(&self, tone: rle_idl::Tone) -> Result<String, String> {
        self.client.call(RleOp::DescribeTone, (tone,))
    }
    fn delay(&self, ms: u64) -> Result<(), String> {
        self.client.call(RleOp::Delay, (ms,))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    let client = DynSpireRle::connect("rle_spier", &HashMap::new())
        .unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1);
        });

    let input = b"AAAABBBCCCCDDDDDEEEEFFFFFFGGG";

    println!("=== DynSpire RLE Compression Demo ===");
    println!();
    println!("  hash   : 0x{:016x}", rle_idl::RLE_IDL_HASH);
    println!("  input  : \"{}\" ({} bytes)", String::from_utf8_lossy(input), input.len());
    println!();

    // --- compress: &[u8] -> Result<Vec<u8>, String> ---
    let compressed = client.compress(&input[..]).expect("compress failed");
    println!("compress()");
    println!("  -> [{}] ({} bytes)", hex(&compressed), compressed.len());
    println!();

    // --- decompress: round-trip verification ---
    let decompressed = client.decompress(&compressed[..]).expect("decompress failed");
    let roundtrip_ok = decompressed.as_slice() == input;
    println!("decompress()");
    println!(
        "  -> \"{}\" ({} bytes) {}",
        String::from_utf8_lossy(&decompressed),
        decompressed.len(),
        if roundtrip_ok { "[round-trip OK]" } else { "[MISMATCH]" },
    );
    println!();

    // --- compress_into: (&[u8], &mut Vec<u8>) -> Result<(), String> ---
    // The spier writes directly into the caller's Vec via a raw pointer
    // passed through the slot system — no copy, no return allocation.
    let mut buf: Vec<u8> = Vec::new();
    client.compress_into(&input[..], &mut buf).expect("compress_into failed");
    let mut_ok = buf == compressed;
    println!("compress_into(&mut Vec<u8>)");
    println!("  caller buffer before: [] (empty)");
    println!(
        "  caller buffer after : [{}] ({} bytes) {}",
        hex(&buf),
        buf.len(),
        if mut_ok { "[matches compress]" } else { "[MISMATCH]" },
    );
    println!();

    // --- stats: &[u8] -> Result<(u64, u64), String> ---
    let (orig, comp) = client.stats(&input[..]).expect("stats failed");
    let ratio = if orig > 0 {
        comp as f64 * 100.0 / orig as f64
    } else {
        0.0
    };
    println!("stats()");
    println!("  original  : {orig} bytes");
    println!("  compressed: {comp} bytes");
    println!("  ratio     : {ratio:.1}%");
    println!();

    // --- analyze: &[u8] -> Result<CompressionReport, String> ---
    // #[slot_struct] crosses FFI as 1 opaque slot (boxed pointer).
    // Rust host accesses fields natively — no serialization, no navigator.
    let report: CompressionReport = client.analyze(&input[..]).expect("analyze failed");
    println!("analyze() -> CompressionReport (opaque box, 1 slot)");
    println!("  original_size  : {}", report.original_size);
    println!("  compressed_size: {}", report.compressed_size);
    println!("  ratio          : {:.1}%", report.ratio * 100.0);
    println!("  runs           : {}", report.runs);
    println!();

    // --- report_summary: pass struct back through FFI as opaque handle ---
    let summary = client
        .report_summary(report.clone())
        .expect("report_summary failed");
    println!("report_summary(CompressionReport)");
    println!("  -> \"{summary}\"");

    println!();
    println!("Done. Spier was loaded, verified, and dispatched entirely at runtime.");
}
