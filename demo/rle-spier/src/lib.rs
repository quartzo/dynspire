use std::collections::HashMap;

use dynspire::managed::{DVec, DString, DOption, DSlice, DStr, DynSpireStateExt};

include!(concat!(env!("OUT_DIR"), "/rle_spier.rs"));

pub struct RleState;

#[repr(C)]
pub struct Snapshot {
    pub data: Vec<u8>,
}

impl Clone for Snapshot {
    fn clone(&self) -> Self {
        Snapshot { data: self.data.clone() }
    }
}

fn rle_compress(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return vec![];
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let byte = data[i];
        let mut count: usize = 1;
        while i + count < data.len() && data[i + count] == byte && count < 255 {
            count += 1;
        }
        out.push(count as u8);
        out.push(byte);
        i += count;
    }
    out
}

fn rle_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() % 2 != 0 {
        return Err("corrupted RLE stream: odd length".into());
    }
    let mut out = Vec::new();
    for pair in data.chunks_exact(2) {
        out.resize(out.len() + pair[0] as usize, pair[1]);
    }
    Ok(out)
}

fn init(_config: &HashMap<String, String>) -> Result<RleState, String> {
    Ok(RleState)
}

/// Helper: build a `DVec<u8>` in the host allocator from a byte slice.
fn dvec_from_bytes(state: &RleState, slice: &[u8]) -> DVec<u8> {
    let mut out = state.new_dvec(slice.len());
    for &b in slice {
        out.push(b);
    }
    out
}

/// Helper: build a `DString` in the host allocator from a `&str`.
fn dstring_from_str(state: &RleState, s: &str) -> DString {
    state.new_dstring(s)
}

impl RleEngine for RleState {
    fn compress(&self, data: DSlice<u8>) -> Result<DVec<u8>, String> {
        let compressed = rle_compress(data.as_slice());
        Ok(dvec_from_bytes(self, &compressed))
    }

    fn decompress(&self, data: DSlice<u8>) -> Result<DVec<u8>, String> {
        let decompressed = rle_decompress(data.as_slice())?;
        Ok(dvec_from_bytes(self, &decompressed))
    }

    fn compress_into(&self, data: DSlice<u8>, out: &mut DVec<u8>) -> Result<(), String> {
        let compressed = rle_compress(data.as_slice());
        for &b in &compressed {
            out.push(b);
        }
        Ok(())
    }

    fn stats(&self, data: DSlice<u8>) -> Result<(u64, u64), String> {
        let slice = data.as_slice();
        let compressed = rle_compress(slice);
        Ok((slice.len() as u64, compressed.len() as u64))
    }

    fn analyze(&self, data: DSlice<u8>) -> Result<CompressionReport, String> {
        let slice = data.as_slice();
        let compressed = rle_compress(slice);
        let runs = compressed.len() as u64 / 2;
        let ratio = if slice.is_empty() {
            0.0
        } else {
            compressed.len() as f64 / slice.len() as f64
        };
        Ok(CompressionReport {
            original_size: slice.len() as u64,
            compressed_size: compressed.len() as u64,
            ratio,
            runs,
        })
    }

    fn report_summary(&self, report: CompressionReport) -> Result<DString, String> {
        Ok(dstring_from_str(self, &format!(
            "original={} compressed={} ratio={:.1}% runs={}",
            report.original_size,
            report.compressed_size,
            report.ratio * 100.0,
            report.runs,
        )))
    }

    fn run_labels(&self, data: DSlice<u8>) -> Result<DVec<DString>, String> {
        let compressed = rle_compress(data.as_slice());
        let alloc = unsafe { &*self.__dynspire_alloc() };
        let count = compressed.len() / 2;
        let mut outer: DVec<DString> = DVec::new_in(alloc, count);
        for pair in compressed.chunks_exact(2) {
            let label = format!("{}x{}", pair[0], pair[1] as char);
            let ds = dstring_from_str(self, &label);
            // Push the raw repr(C) struct into the outer DVec<DString>.
            // Use self.ptr directly — as_slice() would return &[] when len==0.
            unsafe {
                std::ptr::write(outer.ptr.add(outer.len), ds);
            }
            outer.len += 1;
        }
        Ok(outer)
    }

    fn split_runs(&self, data: DSlice<u8>) -> Result<DVec<DVec<u8>>, String> {
        let compressed = rle_compress(data.as_slice());
        let alloc = unsafe { &*self.__dynspire_alloc() };
        let count = compressed.len() / 2;
        let mut outer: DVec<DVec<u8>> = DVec::new_in(alloc, count);
        for pair in compressed.chunks_exact(2) {
            let inner_slice = vec![pair[1]; pair[0] as usize];
            let inner = dvec_from_bytes(self, &inner_slice);
            // Use self.ptr directly — as_slice() returns &[] when len==0.
            unsafe {
                std::ptr::write(outer.ptr.add(outer.len), inner);
            }
            outer.len += 1;
        }
        Ok(outer)
    }

    fn compress_into_checked(&self, data: DSlice<u8>, out: &mut DVec<u8>) -> Result<bool, String> {
        let compressed = rle_compress(data.as_slice());
        for &b in &compressed {
            out.push(b);
        }
        Ok(!out.is_empty())
    }

    fn first_byte(&self, data: DSlice<u8>) -> Result<DOption<u8>, String> {
        Ok(data.as_slice().first().copied().into())
    }

    fn classify(&self, data: DSlice<u8>) -> Result<Tone, String> {
        let slice = data.as_slice();
        if slice.is_empty() {
            Ok(Tone::Quiet)
        } else {
            let max = *slice.iter().max().unwrap();
            if max < 64 {
                Ok(Tone::Normal)
            } else {
                Ok(Tone::Loud(max))
            }
        }
    }

    fn describe_tone(&self, tone: Tone) -> Result<DString, String> {
        let s = match tone {
            Tone::Quiet => "silence".to_string(),
            Tone::Normal => "audible".to_string(),
            Tone::Loud(v) => format!("loud({v})"),
        };
        Ok(dstring_from_str(self, &s))
    }

    fn try_classify(&self, data: DSlice<u8>) -> Result<DOption<Tone>, String> {
        let slice = data.as_slice();
        if slice.is_empty() {
            Ok(unsafe { std::mem::zeroed() })
        } else {
            let tone = self.classify(data)?;
            let mut d: DOption<Tone> = unsafe { std::mem::zeroed() };
            d.tag = 1;
            d.value = tone;
            Ok(d)
        }
    }

    fn try_analyze(&self, data: DSlice<u8>) -> Result<DOption<CompressionReport>, String> {
        let slice = data.as_slice();
        if slice.is_empty() {
            Ok(unsafe { std::mem::zeroed() })
        } else {
            let report = self.analyze(data)?;
            let mut d: DOption<CompressionReport> = unsafe { std::mem::zeroed() };
            d.tag = 1;
            d.value = report;
            Ok(d)
        }
    }

    fn delay(&self, ms: u64) -> Result<(), String> {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(())
    }

    fn make_snapshot(&self, data: DSlice<u8>) -> Result<Snapshot, String> {
        Ok(Snapshot { data: data.as_slice().to_vec() })
    }

    fn snapshot_len(&self, snap: Snapshot) -> Result<u64, String> {
        Ok(snap.data.len() as u64)
    }

    // --- Optional managed types (zero-copy) ---

    fn echo_bytes(&self, data: DSlice<u8>) -> Result<DVec<u8>, String> {
        Ok(dvec_from_bytes(self, data.as_slice()))
    }

    fn build_string(&self, data: DSlice<u8>) -> Result<DString, String> {
        let s = String::from_utf8_lossy(data.as_slice());
        Ok(dstring_from_str(self, &s))
    }

    fn consume_dvec(&self, data: DVec<u8>) -> Result<u64, String> {
        Ok(data.as_slice().len() as u64)
    }

    fn consume_dstring(&self, s: DString) -> Result<u64, String> {
        Ok(s.len() as u64)
    }

    fn view_len(&self, s: DStr) -> Result<u64, String> {
        Ok(s.len() as u64)
    }

    fn view_slice(&self, data: DSlice<u8>) -> Result<u64, String> {
        Ok(data.as_slice().len() as u64)
    }

    fn probe(&self, data: DSlice<u8>) -> Result<DOption<u8>, String> {
        let slice = data.as_slice();
        if slice.is_empty() {
            Ok(None::<u8>.into())
        } else {
            Ok(Some(slice[0]).into())
        }
    }

    fn opt_classify(&self, data: DSlice<u8>) -> Result<DOption<u8>, String> {
        let slice = data.as_slice();
        if slice.is_empty() {
            Ok(None::<u8>.into())
        } else {
            Ok(Some(*slice.iter().max().unwrap()).into())
        }
    }
}

impl_rle_spier!(RleState, init, "rle");
