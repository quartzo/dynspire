use std::collections::HashMap;

use dynspire_macro::{spier_dispatch, spier_storage};
use rle_idl::RleEngine;

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

#[spier_storage]
fn init(_config: &HashMap<String, String>) -> Result<RleState, String> {
    Ok(RleState)
}

#[spier_dispatch(name = "rle", idl = rle_idl::RLE_IDL_HASH)]
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
}
