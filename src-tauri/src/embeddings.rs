//! Embedding helpers — normalization, blob packing, sqlite-vec extension load.
//!
//! Cosine similarity = dot product when vectors are L2-normalized. We normalize
//! both at insert time and at query time, so sqlite-vec's default L2 distance
//! ranks identically to cosine distance for our use case.

use anyhow::{anyhow, Result};
use rusqlite::ffi::sqlite3_auto_extension;
use sqlite_vec::sqlite3_vec_init;
use std::sync::Once;
use zerocopy::IntoBytes;

/// Hardcoded embedding dimension. Must match the FLOAT[N] in schema.sql and
/// the output dim of `embedding_model`. Default model is
/// `Qwen/Qwen3-Embedding-8B` (4096 dim, SOTA on MTEB as of early 2026).
/// Changing this requires dropping `vec_beliefs` and re-embedding.
pub const EMBEDDING_DIM: usize = 4096;

pub const DEFAULT_EMBEDDING_MODEL: &str = "Qwen/Qwen3-Embedding-8B";

/// Cosine threshold above which two L2-normalized vectors are treated as
/// the same belief by the extraction-time dedup check. 0.85 catches
/// paraphrases (e.g. "User's name is Konstantin" vs "The user's name is
/// Konstantin Unruh") without merging distinct-but-related claims.
/// Tunable via the `dedup_cosine_threshold` setting.
pub const DEFAULT_DEDUP_COSINE_THRESHOLD: f64 = 0.85;

/// Lower bound for "review me" merge candidates surfaced in the audit panel.
/// Pairs in [SUGGEST, DEDUP) are presented for manual review; pairs >= DEDUP
/// are flagged as "definite" (and would have auto-merged at extraction time
/// if both were new). Tunable via the `dedup_suggest_threshold` setting.
pub const DEFAULT_SUGGEST_COSINE_THRESHOLD: f64 = 0.70;

/// Minimum cosine for a retrieved belief to actually land in the chat
/// system prompt. Below this we treat the match as noise and drop it
/// entirely. Calibrated from 2026-05-06 dogfood data: off-topic queries
/// (Rust code, Python code) maxed out at ~0.22 cosine; genuinely relevant
/// matches sat in 0.30–0.78. 0.35 keeps the signal, kills the noise.
/// Tunable via the `retrieval_min_cosine` setting.
pub const DEFAULT_RETRIEVAL_MIN_COSINE: f64 = 0.35;

/// Cosine similarity from sqlite-vec's L2 distance between unit vectors.
/// For unit vectors: d² = 2(1 - cos), so cos = 1 - d²/2.
/// Returns a value in [-1, 1].
pub fn cosine_from_l2(distance: f64) -> f64 {
    1.0 - (distance * distance) / 2.0
}

static INIT: Once = Once::new();

/// Register sqlite-vec as an auto-extension so every new SQLite connection
/// has `vec0` available. Must be called before opening any connection.
/// Idempotent.
pub fn register_vec_extension() {
    INIT.call_once(|| {
        // sqlite3_vec_init's signature uses raw pointer types from the sqlite-vec
        // crate; sqlite3_auto_extension expects rusqlite's ffi types. Both
        // resolve to identical C ABI signatures, so we transmute through a
        // matching fn pointer type. Using `_` lets the compiler infer the
        // exact target shape from the call site.
        unsafe {
            let init_fn: unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::ffi::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::ffi::c_int = std::mem::transmute(sqlite3_vec_init as *const ());
            sqlite3_auto_extension(Some(init_fn));
        }
    });
}

/// L2-normalize in place. Zero-vectors are left untouched (rare, only on empty
/// strings) — sqlite-vec will treat them as maximally distant from everything.
pub fn normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Pack a normalized f32 vector into the little-endian bytes that sqlite-vec
/// expects when inserted as a BLOB.
pub fn vec_to_blob(v: &[f32]) -> Result<Vec<u8>> {
    if v.len() != EMBEDDING_DIM {
        return Err(anyhow!(
            "embedding dim mismatch: expected {}, got {}",
            EMBEDDING_DIM,
            v.len()
        ));
    }
    Ok(v.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_unit_vector() {
        let mut v = vec![3.0_f32, 4.0_f32];
        normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn normalize_zero_vector_is_safe() {
        let mut v = vec![0.0_f32; 4];
        normalize(&mut v);
        assert!(v.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn blob_round_trip_preserves_bytes() {
        let mut v = vec![0.1_f32; EMBEDDING_DIM];
        v[0] = 1.0;
        v[EMBEDDING_DIM - 1] = -1.0;
        let blob = vec_to_blob(&v).unwrap();
        assert_eq!(blob.len(), EMBEDDING_DIM * 4);
        // First f32 bytes should be 0x3F800000 (1.0) in little-endian.
        assert_eq!(&blob[0..4], &[0x00, 0x00, 0x80, 0x3F]);
    }

    #[test]
    fn blob_rejects_wrong_dim() {
        let v = vec![0.0_f32; 10];
        assert!(vec_to_blob(&v).is_err());
    }

    #[test]
    fn cosine_from_l2_endpoints() {
        // Identical unit vectors → distance 0 → cosine 1.
        assert!((cosine_from_l2(0.0) - 1.0).abs() < 1e-9);
        // Orthogonal unit vectors → d² = 2 → d = sqrt(2) → cosine 0.
        assert!((cosine_from_l2((2.0_f64).sqrt()) - 0.0).abs() < 1e-9);
        // Opposite unit vectors → d² = 4 → d = 2 → cosine -1.
        assert!((cosine_from_l2(2.0) - (-1.0)).abs() < 1e-9);
    }
}
