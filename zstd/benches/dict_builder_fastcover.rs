use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::io::Cursor;
use structured_zstd::dictionary::{
    FastCoverOptions, create_fastcover_raw_dict_from_source, create_raw_dict_from_source,
};

fn corpus() -> Vec<u8> {
    let mut data = Vec::new();
    for i in 0..2_000u32 {
        data.extend_from_slice(
            format!(
                "tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbcccccdddddeeeeefffff\n"
            )
            .as_bytes(),
        );
    }
    data
}

fn bench_dict_builder(c: &mut Criterion) {
    let data = corpus();
    let dict_size = 8 * 1024;

    c.bench_function("dict_builder/cover_raw", |b| {
        b.iter(|| {
            let mut out = Vec::new();
            create_raw_dict_from_source(
                Cursor::new(data.as_slice()),
                data.len(),
                &mut out,
                black_box(dict_size),
            );
            black_box(out.len());
        })
    });

    c.bench_function("dict_builder/fastcover_raw_opt", |b| {
        b.iter(|| {
            let mut out = Vec::new();
            let tuned = create_fastcover_raw_dict_from_source(
                Cursor::new(data.as_slice()),
                &mut out,
                black_box(dict_size),
                &FastCoverOptions::default(),
            )
            .expect("fastcover training should succeed");
            black_box((out.len(), tuned.score));
        })
    });

    c.bench_function("dict_builder/fastcover_raw_fixed", |b| {
        b.iter(|| {
            let mut out = Vec::new();
            let opts = FastCoverOptions {
                optimize: false,
                accel: 4,
                k: 256,
                d: 8,
                f: 20,
                ..FastCoverOptions::default()
            };
            let tuned = create_fastcover_raw_dict_from_source(
                Cursor::new(data.as_slice()),
                &mut out,
                black_box(dict_size),
                &opts,
            )
            .expect("fastcover training should succeed");
            black_box((out.len(), tuned.score));
        })
    });
}

criterion_group!(benches, bench_dict_builder);
criterion_main!(benches);
