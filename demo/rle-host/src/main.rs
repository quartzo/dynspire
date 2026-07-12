use std::collections::HashMap;

use dynspire::managed::{DOption, DSlice, DStr, DVec};

include!(concat!(env!("OUT_DIR"), "/rle_host.rs"));

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    let client = DynSpireRle::connect("rle_spier", &HashMap::new(), false)
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
    // Out-vec: the host passes a DVec<u8> backed by the host allocator; the
    // spier fills it (via dynspire_realloc) and the host copies the bytes back
    // into the caller's Vec, then releases the buffer.
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
    // opaque struct crosses FFI as 1 slot (boxed pointer).
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

    // --- Optional managed types (DVec / DString) ---
    // echo_bytes allocates a DVec in the spier allocator (owned return). The host
    // receives an OwnedDVec and is the sole owner: it frees the buffer on drop.
    let echoed = client.echo_bytes(&input[..]).expect("echo_bytes failed");
    println!("echo_bytes(&[u8]) -> DVec<u8> (owned, zero-copy)");
    println!(
        "  -> [{}] ({} bytes)",
        hex(&echoed.as_slice()),
        echoed.len()
    );
    println!();

    // consume_dvec takes the raw DVec back (Copy view); spier only reads it, the
    // host still owns and frees it. This exercises the full round-trip.
    let dv: DVec<u8> = *echoed;
    let consumed = client.consume_dvec(dv).expect("consume_dvec failed");
    println!("consume_dvec(DVec<u8>)");
    println!("  -> {consumed} (host still owns the buffer, freed on drop)");
    println!();

    // build_string allocates a DString in the spier allocator (owned return).
    let built = client.build_string(&input[..]).expect("build_string failed");
    println!("build_string(&[u8]) -> DString (owned, zero-copy)");
    println!("  -> \"{}\" ({} bytes)", built.as_str(), built.len());
    println!();

    let ds: dynspire::managed::DString = *built;
    let consumed_s = client.consume_dstring(ds).expect("consume_dstring failed");
    println!("consume_dstring(DString)");
    println!("  -> {consumed_s} (host still owns the buffer, freed on drop)");
    println!();

    // Views: pass a DStr / DSlice pointing at host-owned memory (no copy).
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
    let present = client.probe(&input[..]).expect("probe failed");
    let maxv = client
        .opt_classify(&input[..])
        .expect("opt_classify failed");
    let show = |o: DOption<u8>| -> String {
        if o.tag == 0 {
            "None".to_string()
        } else {
            format!("Some({})", o.value)
        }
    };
    println!("probe(&[u8]) / opt_classify(&[u8]) -> DOption<u8>");
    println!("  probe        -> {}", show(present));
    println!("  opt_classify -> {} (max byte)", show(maxv));
    println!();

    println!("Done. Spier was loaded, verified, and dispatched entirely at runtime.");
    println!();

    println!();
    println!("Done. Spier was loaded, verified, and dispatched entirely at runtime.");
    println!();

    // --- allocator report (debug allocator) ---
    // A separate client backed by the debug allocator tracks live/peak/total
    // memory occupation across all spier allocations (owned returns, out-vecs).
    let debug_client = DynSpireRle::connect("rle_spier", &HashMap::new(), true)
        .expect("debug connect failed");
    let _ = debug_client.compress(&input[..]);
    let _ = debug_client.analyze(&input[..]);
    let report = debug_client.allocator_report();
    println!("allocator_report() (debug allocator)");
    println!("  live bytes        : {}", report.live_bytes);
    println!("  live allocations  : {}", report.live_allocations);
    println!("  peak bytes        : {}", report.peak_bytes);
    println!("  total allocations : {}", report.total_allocations);
}
