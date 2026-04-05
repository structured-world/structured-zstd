use alloc::vec;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy)]
pub struct FastCoverParams {
    pub k: usize,
    pub d: usize,
    pub f: u32,
    pub accel: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct FastCoverTuned {
    pub k: usize,
    pub d: usize,
    pub f: u32,
    pub accel: usize,
    pub score: usize,
}

pub const DEFAULT_K_CANDIDATES: &[usize] = &[64, 128, 256, 512, 1024, 2048];
pub const DEFAULT_D_CANDIDATES: &[usize] = &[6, 8, 12, 16];
pub const DEFAULT_F_CANDIDATES: &[u32] = &[16, 18, 20];

fn hash_dmer(dmer: &[u8]) -> u64 {
    // 64-bit FNV-1a, deterministic and cheap for d-mer hashing.
    let mut h = 0xcbf29ce484222325u64;
    for &b in dmer {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn clamp_table_bits(f: u32) -> u32 {
    f.clamp(8, 20)
}

fn build_frequency_table(sample: &[u8], d: usize, f: u32, accel: usize) -> Vec<u32> {
    let bits = clamp_table_bits(f);
    let size = 1usize << bits;
    let mask = size - 1;
    let step = accel.max(1);
    let mut table = vec![0u32; size];

    if sample.len() < d || d == 0 {
        return table;
    }

    let mut i = 0usize;
    while i + d <= sample.len() {
        let slot = (hash_dmer(&sample[i..i + d]) as usize) & mask;
        table[slot] = table[slot].saturating_add(1);
        i += step;
    }
    table
}

fn score_segment(segment: &[u8], d: usize, mask: usize, table: &[u32]) -> usize {
    if segment.len() < d || d == 0 {
        return 0;
    }
    let mut score = 0usize;
    for i in 0..=(segment.len() - d) {
        let slot = (hash_dmer(&segment[i..i + d]) as usize) & mask;
        score += table[slot] as usize;
    }
    score
}

fn build_raw_dict(sample: &[u8], dict_size: usize, params: FastCoverParams) -> Vec<u8> {
    if sample.is_empty() || dict_size == 0 {
        return Vec::new();
    }

    let k = params.k.max(params.d).max(16);
    let d = params.d.clamp(4, 32);
    let table = build_frequency_table(sample, d, params.f, params.accel);
    let mask = table.len().saturating_sub(1);

    let mut segments: Vec<(usize, &[u8])> = sample
        .chunks(k)
        .filter(|seg| seg.len() >= d)
        .map(|seg| (score_segment(seg, d, mask, &table), seg))
        .collect();
    segments.sort_by(|a, b| b.0.cmp(&a.0));

    let mut out = Vec::with_capacity(dict_size);
    for (_, seg) in segments {
        if out.len() >= dict_size {
            break;
        }
        let remaining = dict_size - out.len();
        if seg.len() <= remaining {
            out.extend_from_slice(seg);
        } else {
            out.extend_from_slice(&seg[..remaining]);
        }
    }
    out
}

fn coverage_score(dict: &[u8], eval: &[u8], d: usize, accel: usize) -> usize {
    if dict.len() < d || eval.len() < d || d == 0 {
        return 0;
    }
    let mut seen = std::collections::HashSet::with_capacity(dict.len() / d + 1);
    for i in 0..=(dict.len() - d) {
        seen.insert(hash_dmer(&dict[i..i + d]));
    }

    let mut hits = 0usize;
    let step = accel.max(1);
    let mut i = 0usize;
    while i + d <= eval.len() {
        if seen.contains(&hash_dmer(&eval[i..i + d])) {
            hits += 1;
        }
        i += step;
    }
    hits
}

pub fn train_fastcover_raw(sample: &[u8], dict_size: usize, params: FastCoverParams) -> Vec<u8> {
    build_raw_dict(sample, dict_size, params)
}

pub fn optimize_fastcover_raw(
    sample: &[u8],
    dict_size: usize,
    split_point: f64,
    accel: usize,
    d_candidates: &[usize],
    f_candidates: &[u32],
    k_values: &[usize],
) -> (Vec<u8>, FastCoverTuned) {
    let d_values = if d_candidates.is_empty() {
        DEFAULT_D_CANDIDATES
    } else {
        d_candidates
    };
    let f_values = if f_candidates.is_empty() {
        DEFAULT_F_CANDIDATES
    } else {
        f_candidates
    };
    let k_candidates = if k_values.is_empty() {
        DEFAULT_K_CANDIDATES
    } else {
        k_values
    };

    if sample.len() < 2 {
        let params = FastCoverParams {
            k: k_candidates[0],
            d: d_values[0],
            f: f_values[0],
            accel,
        };
        let mut dict = build_raw_dict(sample, dict_size, params);
        if dict.is_empty() && dict_size > 0 {
            let take = sample.len().min(dict_size);
            dict.extend_from_slice(&sample[..take]);
        }
        return (
            dict,
            FastCoverTuned {
                k: params.k,
                d: params.d,
                f: params.f,
                accel,
                score: 0,
            },
        );
    }

    let split = split_point.clamp(0.1, 0.95);
    let split_idx = ((sample.len() as f64) * split) as usize;
    let split_idx = split_idx.clamp(1, sample.len().saturating_sub(1));
    let (train, eval) = sample.split_at(split_idx);

    let mut best_dict = Vec::new();
    let mut best = FastCoverTuned {
        k: 0,
        d: 0,
        f: 0,
        accel,
        score: 0,
    };

    for &f in f_values {
        for &d in d_values {
            for &k in k_candidates {
                let params = FastCoverParams { k, d, f, accel };
                let dict = build_raw_dict(train, dict_size, params);
                let score = coverage_score(dict.as_slice(), eval, d, accel);
                if best_dict.is_empty() || score > best.score {
                    best.score = score;
                    best.k = k;
                    best.d = d;
                    best.f = f;
                    best_dict = dict;
                }
            }
        }
    }

    (best_dict, best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::format;

    fn corpus() -> Vec<u8> {
        let mut data = Vec::new();
        for i in 0..500u32 {
            data.extend_from_slice(
                format!("tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbccccdddd\n")
                    .as_bytes(),
            );
        }
        data
    }

    #[test]
    fn fastcover_raw_produces_non_empty_dict() {
        let sample = corpus();
        let dict = train_fastcover_raw(
            sample.as_slice(),
            4096,
            FastCoverParams {
                k: 256,
                d: 8,
                f: 20,
                accel: 1,
            },
        );
        assert!(!dict.is_empty());
        assert!(dict.len() <= 4096);
    }

    #[test]
    fn fastcover_optimizer_selects_valid_params() {
        let sample = corpus();
        let (dict, tuned) = optimize_fastcover_raw(
            sample.as_slice(),
            4096,
            0.75,
            1,
            &[6, 8],
            &[18, 20],
            &[128, 256],
        );
        assert!(!dict.is_empty());
        assert!([6, 8].contains(&tuned.d));
        assert!([18, 20].contains(&tuned.f));
        assert!([128, 256].contains(&tuned.k));
    }

    #[test]
    fn fastcover_optimizer_falls_back_when_k_candidates_empty() {
        let sample = corpus();
        let (dict, tuned) =
            optimize_fastcover_raw(sample.as_slice(), 4096, 0.75, 1, &[6, 8], &[18, 20], &[]);
        assert!(!dict.is_empty());
        assert!(DEFAULT_K_CANDIDATES.contains(&tuned.k));
    }

    #[test]
    fn fastcover_optimizer_handles_one_byte_sample_without_panic() {
        let sample = [0xAB];
        let (dict, tuned) = optimize_fastcover_raw(&sample, 16, 0.75, 1, &[], &[], &[]);
        assert!(!dict.is_empty());
        assert!(dict.len() <= 16);
        assert!(DEFAULT_K_CANDIDATES.contains(&tuned.k));
        assert!(DEFAULT_D_CANDIDATES.contains(&tuned.d));
        assert!(DEFAULT_F_CANDIDATES.contains(&tuned.f));
    }

    #[test]
    fn fastcover_optimizer_seeds_winner_when_all_scores_are_zero() {
        let sample = b"abcdefghijklmnopqrst";
        let (dict, tuned) = optimize_fastcover_raw(sample, 16, 0.9, 1, &[6], &[16], &[8]);
        assert!(!dict.is_empty());
        assert_eq!(tuned.k, 8);
        assert_eq!(tuned.d, 6);
        assert_eq!(tuned.f, 16);
        assert_eq!(tuned.score, 0);
    }
}
