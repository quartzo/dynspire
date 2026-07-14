use std::collections::HashMap;

use dynspire::managed::{DOption, DSlice, DStr, DVec, DString};

include!(concat!(env!("OUT_DIR"), "/rle_host.rs"));

#[repr(C)]
pub struct Snapshot {
    pub data: Vec<u8>,
}

impl Clone for Snapshot {
    fn clone(&self) -> Self {
        Snapshot { data: self.data.clone() }
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
    println!("  hash   : 0x{:016x}", RLE_IDL_HASH);
    println!("  input  : \"{}\" ({} bytes)", String::from_utf8_lossy(input), input.len());
    println!();

    // Helper: wrap a byte slice as a DSlice<u8> for spier calls that
    // take a `&DVec<u8>` (which becomes DSlice<u8> on the Rust side).
    let input_view = || DSlice::<u8> {
        ptr: input.as_ptr(),
        len: input.len(),
    };

    // --- compress: DSlice<u8> -> Result<DVec<u8>, String> ---
    let compressed = client.compress(input_view()).expect("compress failed");
    println!("compress()");
    println!("  -> [{}] ({} bytes)", hex(compressed.as_slice()), compressed.len());
    println!();

    // --- decompress: round-trip verification ---
    let decompressed = client.decompress(DSlice::<u8> {
        ptr: compressed.as_ptr(),
        len: compressed.len(),
    }).expect("decompress failed");
    let roundtrip_ok = decompressed.as_slice() == input;
    println!("decompress()");
    println!(
        "  -> \"{}\" ({} bytes) {}",
        String::from_utf8_lossy(decompressed.as_slice()),
        decompressed.len(),
        if roundtrip_ok { "[round-trip OK]" } else { "[MISMATCH]" },
    );
    println!();

    // --- compress_into: (DSlice<u8>, &mut DVec<u8>) -> Result<(), String> ---
    let mut buf: DVec<u8> = client.new_dvec(0);
    client.compress_into(input_view(), &mut buf).expect("compress_into failed");
    let mut_ok = buf.as_slice() == compressed.as_slice();
    println!("compress_into(&mut DVec<u8>)");
    println!("  caller buffer before: [] (empty)");
    println!(
        "  caller buffer after : [{}] ({} bytes) {}",
        hex(buf.as_slice()),
        buf.len(),
        if mut_ok { "[matches compress]" } else { "[MISMATCH]" },
    );
    println!();

    // --- stats: DSlice<u8> -> Result<(u64, u64), String> ---
    let (orig, comp) = client.stats(input_view()).expect("stats failed");
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

    // --- analyze: DSlice<u8> -> Result<CompressionReport, String> ---
    let report: CompressionReport = client.analyze(input_view()).expect("analyze failed");
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
    println!("  -> \"{}\"", summary.as_str());
    println!();

    // --- Optional managed types (DVec / DString) ---
    let echoed = client.echo_bytes(input_view()).expect("echo_bytes failed");
    println!("echo_bytes(&DVec<u8>) -> DVec<u8> (owned, RC-aware)");
    println!(
        "  -> [{}] ({} bytes)",
        hex(echoed.as_slice()),
        echoed.len()
    );
    println!();

    // consume_dvec takes ownership of the DVec; spier reads it, host
    // still owns and releases via Drop (RC-aware single type).
    let consumed = client.consume_dvec(echoed).expect("consume_dvec failed");
    println!("consume_dvec(DVec<u8>)");
    println!("  -> {consumed} (RC-aware ownership transfer)");
    println!();

    let built = client.build_string(input_view()).expect("build_string failed");
    println!("build_string(&DVec<u8>) -> DString (owned, RC-aware)");
    println!("  -> \"{}\" ({} bytes)", built.as_str(), built.len());
    println!();

    let consumed_s = client.consume_dstring(built).expect("consume_dstring failed");
    println!("consume_dstring(DString)");
    println!("  -> {consumed_s}");
    println!();

    // Views: pass DStr / DSlice pointing at host-owned memory (no copy).
    let dstr = DStr {
        ptr: input.as_ptr(),
        len: input.len(),
    };
    let view_n = client.view_len(dstr).expect("view_len failed");
    println!("view_len(DStr)");
    println!("  -> {view_n} (zero-copy view over host memory)");
    println!();

    let dslice = DSlice::<u8> {
        ptr: input.as_ptr(),
        len: input.len(),
    };
    let view_s = client.view_slice(dslice).expect("view_slice failed");
    println!("view_slice(DSlice<u8>)");
    println!("  -> {view_s} (zero-copy view over host memory)");
    println!();

    // DOption return: managed tag+value, no boxing.
    let present = client.probe(input_view()).expect("probe failed");
    let maxv = client
        .opt_classify(input_view())
        .expect("opt_classify failed");
    let show = |o: DOption<u8>| -> String {
        if o.tag == 0 {
            "None".to_string()
        } else {
            format!("Some({})", o.value)
        }
    };
    println!("probe(&DVec<u8>) / opt_classify(&DVec<u8>) -> DOption<u8>");
    println!("  probe        -> {}", show(present));
    println!("  opt_classify -> {} (max byte)", show(maxv));
    println!();

    // --- Opaque type round-trip ---
    let snap = client.make_snapshot(input_view()).expect("make_snapshot failed");
    println!("make_snapshot(&DVec<u8>) -> Snapshot (opaque, boxed pointer)");
    println!("  -> {} bytes", snap.data.len());
    let snap_len = client.snapshot_len(snap).expect("snapshot_len failed");
    println!("snapshot_len(Snapshot)");
    println!("  -> {snap_len}");
    println!();

    println!("Done. Spier was loaded, verified, and dispatched entirely at runtime.");
    println!();

    // --- allocator report (debug allocator) ---
    let debug_client = DynSpireRle::connect_with_debug("rle_spier", &HashMap::new(), true)
        .expect("debug connect failed");
    let _ = debug_client.compress(input_view());
    let _ = debug_client.analyze(input_view());
    let report = debug_client.allocator_report();
    println!("allocator_report() (debug allocator)");
    println!("  live bytes        : {}", report.live_bytes);
    println!("  live allocations  : {}", report.live_allocations);
    println!("  peak bytes        : {}", report.peak_bytes);
    println!("  total allocations : {}", report.total_allocations);

    // Touch DString to avoid unused import warning.
    let _ds: Option<DString> = None;
}
