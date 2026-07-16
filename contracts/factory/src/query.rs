use soroban_sdk::{Env, Vec};

pub fn paginate(env: &Env, v: Vec<u64>, offset: u32, limit: u32) -> Vec<u64> {
    let mut result = Vec::new(env);
    let start = offset as usize;
    // offset + limit is caller-controlled and can overflow u32; the release
    // profile enables overflow-checks, so a raw `+` here would panic this
    // read-only view call rather than just clamping to the Vec's length.
    let end = (offset as usize).saturating_add(limit as usize);
    for i in start..end.min(v.len() as usize) {
        result.push_back(v.get(i as u32).unwrap());
    }
    result
}
