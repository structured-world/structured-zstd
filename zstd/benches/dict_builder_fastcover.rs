use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::io::Cursor;
use structured_zstd::dictionary::{
    CoverOptions, FastCoverOptions, FinalizeOptions, create_raw_dict_from_source,
    optimize_cover_dict, optimize_fastcover_dict, train_cover_dict, train_fastcover_dict,
};

/// Log lines, one sample each.
fn corpus() -> (Vec<u8>, Vec<usize>) {
    let mut data = Vec::new();
    let mut sizes = Vec::new();
    for i in 0..2_000u32 {
        let line = format!(
            "tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbcccccdddddeeeeefffff\n"
        );
        sizes.push(line.len());
        data.extend_from_slice(line.as_bytes());
    }
    (data, sizes)
}

fn bench_dict_builder(c: &mut Criterion) {
    let (data, sizes) = corpus();
    let dict_size = 8 * 1024;
    let finalize = FinalizeOptions::default();
    let cover_fixed = CoverOptions {
        k: 256,
        ..CoverOptions::default()
    };
    let fastcover_fixed = FastCoverOptions {
        cover: CoverOptions {
            k: 256,
            ..FastCoverOptions::default().cover
        },
        ..FastCoverOptions::default()
    };

    c.bench_function("dict_builder/raw_content", |b| {
        b.iter(|| {
            let mut out = Vec::new();
            create_raw_dict_from_source(
                Cursor::new(data.as_slice()),
                data.len(),
                &mut out,
                black_box(dict_size),
            )
            .expect("raw training should succeed");
            black_box(out.len());
        })
    });

    c.bench_function("dict_builder/cover_opt", |b| {
        b.iter(|| {
            let (dict, chosen) = optimize_cover_dict(
                &data,
                &sizes,
                black_box(dict_size),
                &CoverOptions::default(),
                finalize,
            )
            .expect("cover training should succeed");
            black_box((dict.len(), chosen.k));
        })
    });

    c.bench_function("dict_builder/cover_fixed", |b| {
        b.iter(|| {
            let dict =
                train_cover_dict(&data, &sizes, black_box(dict_size), &cover_fixed, finalize)
                    .expect("cover training should succeed");
            black_box(dict.len());
        })
    });

    c.bench_function("dict_builder/fastcover_opt", |b| {
        b.iter(|| {
            let (dict, chosen) = optimize_fastcover_dict(
                &data,
                &sizes,
                black_box(dict_size),
                &FastCoverOptions::default(),
                finalize,
            )
            .expect("fastcover training should succeed");
            black_box((dict.len(), chosen.cover.k));
        })
    });

    c.bench_function("dict_builder/fastcover_fixed", |b| {
        b.iter(|| {
            let dict = train_fastcover_dict(
                &data,
                &sizes,
                black_box(dict_size),
                &fastcover_fixed,
                finalize,
            )
            .expect("fastcover training should succeed");
            black_box(dict.len());
        })
    });
}

criterion_group!(benches, bench_dict_builder);
criterion_main!(benches);
