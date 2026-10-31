//! `graph.cnav` decoder for JavaScript hosts (the VS Code extension), built with
//! `cargo build -p codenav-wasm --release --target wasm32-unknown-unknown`.
//!
//! Plain exports, no bindings generator: the host copies the file into memory from
//! `alloc`, calls `decode`, reads the JSON it returns and frees both buffers with
//! `dealloc`.

use codenav::graph::Graph;

/// The graph in `data` (a `graph.cnav` file) as JSON, or `{"error": "…"}`.
pub fn decode_to_json(data: &[u8]) -> Vec<u8> {
    let json = Graph::from_bytes(data).and_then(|g| Ok(serde_json::to_vec(&g)?));
    json.unwrap_or_else(|e| {
        serde_json::to_vec(&serde_json::json!({ "error": format!("{e:#}") })).unwrap_or_default()
    })
}

/// A buffer of `len` bytes for the host to write into.
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut buffer = Vec::<u8>::with_capacity(len);
    let ptr = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    ptr
}

/// Frees a buffer from `alloc` or `decode`.
///
/// # Safety
/// `ptr` and `len` must describe a buffer returned by `alloc(len)` or by `decode`, not
/// freed yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dealloc(ptr: *mut u8, len: usize) {
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len) });
}

/// Decodes the `len` bytes at `ptr` (from `alloc`, which this frees) into JSON; returns
/// its address in the high 32 bits and its length in the low ones.
///
/// # Safety
/// `ptr` must come from `alloc(len)` and hold `len` initialized bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn decode(ptr: *mut u8, len: usize) -> u64 {
    let input = unsafe { Vec::from_raw_parts(ptr, len, len) };
    let json = decode_to_json(&input).into_boxed_slice();
    let (out_len, out_ptr) = (json.len(), Box::into_raw(json) as *mut u8);
    ((out_ptr as usize as u64) << 32) | out_len as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_saved_graph_and_reports_errors() {
        let g = Graph::new(std::path::Path::new("/repo"));
        let json: serde_json::Value =
            serde_json::from_slice(&decode_to_json(&g.to_bytes().unwrap())).unwrap();
        assert_eq!(json["root"], "/repo");
        let err: serde_json::Value = serde_json::from_slice(&decode_to_json(b"nope")).unwrap();
        assert!(
            err["error"].as_str().is_some_and(|e| !e.is_empty()),
            "{err}"
        );
    }
}
