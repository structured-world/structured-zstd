use super::*;

#[test]
fn roundtrip() {
    use alloc::vec::Vec;
    round_trip(&[1, 1, 1, 1, 2, 3]);
    round_trip(&[1, 1, 1, 1, 2, 3, 5, 45, 12, 90]);

    for size in 2..255 {
        use alloc::vec;
        let data = vec![123; size];
        round_trip(&data);
        let mut data = Vec::new();
        for x in 0..size {
            data.push(x as u8);
        }
        round_trip(&data);
    }

    #[cfg(feature = "std")]
    if std::fs::exists("fuzz/artifacts/huff0").unwrap_or(false) {
        for file in std::fs::read_dir("fuzz/artifacts/huff0").unwrap() {
            if file.as_ref().unwrap().file_type().unwrap().is_file() {
                let data = std::fs::read(file.unwrap().path()).unwrap();
                round_trip(&data);
            }
        }
    }
}
