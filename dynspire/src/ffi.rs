pub fn new_uuid() -> [u8; 16] {
    *uuid::Uuid::new_v4().as_bytes()
}

pub fn uuid_to_hex(id: &[u8; 16]) -> String {
    uuid::Uuid::from_bytes(*id).simple().to_string()
}

pub fn uuid_from_hex(s: &str) -> Option<[u8; 16]> {
    uuid::Uuid::parse_str(s)
        .ok()
        .map(|u| *u.as_bytes())
}

