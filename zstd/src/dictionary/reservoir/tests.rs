use super::Reservoir;
use alloc::vec;

#[test]
fn initial_fill() {
    // Create a reservoir 16 bytes in size and read
    // 16 bytes into it
    let r = Reservoir::new(16);
    let test_data = vec![0_u8; 16];
    let output = r.fill(&mut test_data.as_slice());
    assert_eq!(test_data, output);
}

#[test]
fn shrinks_for_small_sample() {
    // Create a reservoir larger than the sample.
    // The output should be smaller.
    let r = Reservoir::new(32);
    let test_data = vec![0_u8; 28];
    let output = r.fill(&mut test_data.as_slice());
    assert!(output.len() == 28);
}

#[test]
fn lake_doesnt_grow() {
    // Create a sample larger than the reservoir
    // The output should be smaller.
    let r = Reservoir::new(32);
    let test_data = vec![0_u8; 16_000_000];
    let output = r.fill(&mut test_data.as_slice());
    assert!(output.len() == 32);
}
