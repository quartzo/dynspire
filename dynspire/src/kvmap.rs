use std::collections::HashMap;

/// Serialize a `HashMap<String, String>` into a URL-encoded byte string for FFI.
///
/// Produces `key=value&key2=value2` with percent-encoding for special chars.
pub fn serialize_kvmap(config: &HashMap<String, String>) -> Vec<u8> {
    let mut s = form_urlencoded::Serializer::new(String::new());
    for (k, v) in config {
        s.append_pair(k, v);
    }
    s.finish().into_bytes()
}

/// Deserialize a URL-encoded byte string back into `HashMap<String, String>`.
///
/// Inverse of [`serialize_kvmap`]. Returns an empty map on null/empty input.
pub fn deserialize_kvmap(data: &[u8]) -> HashMap<String, String> {
    if data.is_empty() {
        return HashMap::new();
    }
    form_urlencoded::parse(data).into_owned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let map = HashMap::new();
        let buf = serialize_kvmap(&map);
        let back = deserialize_kvmap(&buf);
        assert!(back.is_empty());
    }

    #[test]
    fn roundtrip_basic() {
        let mut map = HashMap::new();
        map.insert("backend".into(), "file".into());
        map.insert("path".into(), "/data/db".into());
        map.insert("read_only".into(), "true".into());
        let buf = serialize_kvmap(&map);
        let back = deserialize_kvmap(&buf);
        assert_eq!(back, map);
    }

    #[test]
    fn deserializes_empty_buffer() {
        let back = deserialize_kvmap(&[]);
        assert!(back.is_empty());
    }

    #[test]
    fn roundtrip_special_chars() {
        let mut map = HashMap::new();
        map.insert("path".into(), "/a&b=c%d".into());
        map.insert("region".into(), "us-east-1".into());
        let buf = serialize_kvmap(&map);
        let back = deserialize_kvmap(&buf);
        assert_eq!(back, map);
    }
}
