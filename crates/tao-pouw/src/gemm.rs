//! Shared NoisyGEMM primitives (integer, deterministic) used by both the
//! seed-derived [`crate::MatmulPow`] and the model-bound [`crate::utility_gate`].

/// Deterministically expand a seed into `count` signed entries.
pub(crate) fn fill(seed: &[u8; 32], domain: u8, count: usize) -> Vec<i64> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(seed);
    hasher.update(&[domain]);
    let mut xof = hasher.finalize_xof();
    let mut bytes = vec![0u8; count];
    xof.fill(&mut bytes);
    bytes.into_iter().map(|b| b as i8 as i64).collect()
}

/// Low-rank product `L · Rᵀ` → `rows × cols` (L is rows×rank, R is cols×rank).
pub(crate) fn lowrank(l: &[i64], r: &[i64], rows: usize, cols: usize, rank: usize) -> Vec<i64> {
    let mut out = vec![0i64; rows * cols];
    for i in 0..rows {
        for j in 0..cols {
            let mut acc = 0i64;
            for k in 0..rank {
                acc = acc.wrapping_add(l[i * rank + k].wrapping_mul(r[j * rank + k]));
            }
            out[i * cols + j] = acc;
        }
    }
    out
}

/// Dense `n × n` matrix product (wrapping arithmetic for determinism).
pub(crate) fn matmul(a: &[i64], b: &[i64], n: usize) -> Vec<i64> {
    let mut out = vec![0i64; n * n];
    for i in 0..n {
        for j in 0..n {
            let mut acc = 0i64;
            for k in 0..n {
                acc = acc.wrapping_add(a[i * n + k].wrapping_mul(b[k * n + j]));
            }
            out[i * n + j] = acc;
        }
    }
    out
}

/// Hash the transcript of the noised product `(A+E)·(B+F)`, where the low-rank
/// noise `E,F` is derived from `noise_seed`. This is the PoW value: computing it
/// requires the full `O(n³)` matmul, and the true product `A·B` is recoverable.
pub(crate) fn noisy_product_transcript(
    a: &[i64],
    b: &[i64],
    n: usize,
    rank: usize,
    noise_seed: &[u8; 32],
) -> [u8; 32] {
    let el = fill(noise_seed, 2, n * rank);
    let er = fill(noise_seed, 3, n * rank);
    let fl = fill(noise_seed, 4, n * rank);
    let fr = fill(noise_seed, 5, n * rank);
    let e = lowrank(&el, &er, n, n, rank);
    let f = lowrank(&fl, &fr, n, n, rank);
    let an: Vec<i64> = a.iter().zip(&e).map(|(x, y)| x.wrapping_add(*y)).collect();
    let bn: Vec<i64> = b.iter().zip(&f).map(|(x, y)| x.wrapping_add(*y)).collect();
    let cn = matmul(&an, &bn, n);

    let mut hasher = blake3::Hasher::new();
    for v in &cn {
        hasher.update(&v.to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}
