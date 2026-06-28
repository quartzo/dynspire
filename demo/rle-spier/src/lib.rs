use std::collections::HashMap;

include!(concat!(env!("OUT_DIR"), "/rle_spier.rs"));

pub struct RleState;

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

impl RleEngine for RleState {
    fn compress(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        Ok(rle_compress(data))
    }

    fn decompress(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        rle_decompress(data)
    }

    fn compress_into(&self, data: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        out.extend_from_slice(&rle_compress(data));
        Ok(())
    }

    fn stats(&self, data: &[u8]) -> Result<(u64, u64), String> {
        let compressed = rle_compress(data);
        Ok((data.len() as u64, compressed.len() as u64))
    }

    fn analyze(&self, data: &[u8]) -> Result<CompressionReport, String> {
        let compressed = rle_compress(data);
        let runs = compressed.len() as u64 / 2;
        let ratio = if data.is_empty() {
            0.0
        } else {
            compressed.len() as f64 / data.len() as f64
        };
        Ok(CompressionReport {
            original_size: data.len() as u64,
            compressed_size: compressed.len() as u64,
            ratio,
            runs,
        })
    }

    fn report_summary(&self, report: CompressionReport) -> Result<String, String> {
        Ok(format!(
            "original={} compressed={} ratio={:.1}% runs={}",
            report.original_size,
            report.compressed_size,
            report.ratio * 100.0,
            report.runs,
        ))
    }

    fn run_labels(&self, data: &[u8]) -> Result<Vec<String>, String> {
        let compressed = rle_compress(data);
        let labels = compressed
            .chunks_exact(2)
            .map(|pair| format!("{}x{}", pair[0], pair[1] as char))
            .collect();
        Ok(labels)
    }

    fn split_runs(&self, data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let compressed = rle_compress(data);
        let runs = compressed
            .chunks_exact(2)
            .map(|pair| vec![pair[1]; pair[0] as usize])
            .collect();
        Ok(runs)
    }

    fn compress_into_checked(&self, data: &[u8], out: &mut Vec<u8>) -> Result<bool, String> {
        out.extend_from_slice(&rle_compress(data));
        Ok(!out.is_empty())
    }

    fn first_byte(&self, data: &[u8]) -> Result<Option<u8>, String> {
        Ok(data.first().copied())
    }

    fn classify(&self, data: &[u8]) -> Result<Tone, String> {
        if data.is_empty() {
            Ok(Tone::Quiet)
        } else {
            let max = *data.iter().max().unwrap();
            if max < 64 {
                Ok(Tone::Normal)
            } else {
                Ok(Tone::Loud(max))
            }
        }
    }

    fn describe_tone(&self, tone: Tone) -> Result<String, String> {
        Ok(match tone {
            Tone::Quiet => "silence".to_string(),
            Tone::Normal => "audible".to_string(),
            Tone::Loud(v) => format!("loud({v})"),
        })
    }

    fn try_classify(&self, data: &[u8]) -> Result<Option<Tone>, String> {
        if data.is_empty() {
            Ok(None)
        } else {
            Ok(Some(self.classify(data)?))
        }
    }

    fn try_analyze(&self, data: &[u8]) -> Result<Option<CompressionReport>, String> {
        if data.is_empty() {
            Ok(None)
        } else {
            Ok(Some(self.analyze(data)?))
        }
    }

    fn delay(&self, ms: u64) -> Result<(), String> {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(())
    }
}

impl_rle_spier!(RleState, init, "rle");
