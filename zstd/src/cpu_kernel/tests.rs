use super::*;

/// A ceiling admits its own rung and those below it on its own ladder, and
/// nothing on the other ladder; scalar code is always admitted, and a scalar
/// ceiling admits nothing else. No ceiling admits everything.
#[test]
fn a_ceiling_admits_its_rung_and_below_on_its_ladder() {
    use CpuLevel::*;
    assert!(allowed_under(None, Avx512));
    for ceiling in CpuLevel::ALL {
        assert!(allowed_under(Some(ceiling), Scalar), "{ceiling}");
    }
    let x86 = [Sse2, Sse42, Bmi2, Avx2, Avx512];
    for (index, ceiling) in x86.iter().enumerate() {
        for (needed_index, needed) in x86.iter().enumerate() {
            assert_eq!(
                allowed_under(Some(*ceiling), *needed),
                needed_index <= index,
                "ceiling {ceiling}, needed {needed}"
            );
        }
        assert!(!allowed_under(Some(*ceiling), Neon));
        assert!(!allowed_under(Some(*ceiling), Sve));
    }
    assert!(allowed_under(Some(Sve), Neon));
    assert!(!allowed_under(Some(Neon), Sve));
    assert!(!allowed_under(Some(Neon), Sse2));
    for needed in CpuLevel::ALL.into_iter().filter(|level| *level != Scalar) {
        assert!(!allowed_under(Some(Scalar), needed), "{needed}");
    }
}

/// Every level parses back from its own name, in any case, plus the other
/// common spellings; anything else is refused and the error lists the names.
#[test]
fn levels_parse_from_their_names() {
    for level in CpuLevel::ALL {
        assert_eq!(level.name().parse::<CpuLevel>(), Ok(level));
        assert_eq!(
            level.name().to_ascii_uppercase().parse::<CpuLevel>(),
            Ok(level)
        );
    }
    assert_eq!("sse42".parse::<CpuLevel>(), Ok(CpuLevel::Sse42));
    assert_eq!("AVX-512".parse::<CpuLevel>(), Ok(CpuLevel::Avx512));
    assert_eq!("avx512f".parse::<CpuLevel>(), Ok(CpuLevel::Avx512));
    let err = "avx3".parse::<CpuLevel>().unwrap_err();
    let message = std::string::ToString::to_string(&err);
    assert!(
        message.contains("sse4.2") && message.contains("avx512"),
        "{message}"
    );
}

/// Run `body` as test `name` in a process of its own: the ceiling is
/// process-wide and frozen at first use, so a test that sets or freezes it
/// must not share a process with any other test, whatever runner is used.
/// The parent re-runs this test binary for exactly `name` and checks it
/// passed; the child, marked by the environment, runs `body`.
fn in_own_process(name: &str, body: fn()) {
    const CHILD: &str = "STRUCTURED_ZSTD_CEILING_TEST";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        body();
        return;
    }
    let test = std::format!("cpu_kernel::tests::{name}");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test.as_str(), "--exact", "--test-threads=1"])
        .env(CHILD, name)
        .status()
        .unwrap();
    assert!(status.success(), "{name} failed in its own process");
}

/// Set before any kernel is chosen, the ceiling holds for every tier the
/// process then picks; once it is in force, it cannot be changed.
#[test]
fn a_ceiling_set_first_governs_every_tier_and_then_freezes() {
    in_own_process(
        "a_ceiling_set_first_governs_every_tier_and_then_freezes",
        ceiling_set_first_governs_every_tier_and_then_freezes,
    );
}

fn ceiling_set_first_governs_every_tier_and_then_freezes() {
    assert_eq!(set_cpu_ceiling(CpuLevel::Scalar), Ok(()));
    assert_eq!(cpu_ceiling(), Some(CpuLevel::Scalar));
    assert_eq!(active_cpu_kernel_name(), "scalar");
    assert_eq!(crate::encoding::active_match_kernel_name(), "scalar");
    assert_eq!(
        crate::decoding::simd_copy::ExactCopyTier::resolve(),
        crate::decoding::simd_copy::ExactCopyTier::Scalar
    );
    assert_eq!(
        set_cpu_ceiling(CpuLevel::Scalar),
        Err(CpuCeilingError::AlreadyResolved)
    );
}

/// Choosing a kernel freezes the ceiling as none: setting one afterwards would
/// leave tiers already in use above it.
#[test]
fn a_ceiling_cannot_follow_the_first_kernel_choice() {
    in_own_process(
        "a_ceiling_cannot_follow_the_first_kernel_choice",
        ceiling_cannot_follow_the_first_kernel_choice,
    );
}

fn ceiling_cannot_follow_the_first_kernel_choice() {
    let _ = active_cpu_kernel_name();
    assert_eq!(cpu_ceiling(), None);
    assert_eq!(
        set_cpu_ceiling(CpuLevel::Scalar),
        Err(CpuCeilingError::AlreadyResolved)
    );
}

/// A level of the other architecture is refused rather than read as scalar.
#[test]
fn a_level_of_another_architecture_is_refused() {
    in_own_process(
        "a_level_of_another_architecture_is_refused",
        level_of_another_architecture_is_refused,
    );
}

fn level_of_another_architecture_is_refused() {
    let foreign = if cfg!(target_arch = "aarch64") {
        CpuLevel::Avx2
    } else {
        CpuLevel::Neon
    };
    assert_eq!(
        set_cpu_ceiling(foreign),
        Err(CpuCeilingError::OtherArchitecture(foreign))
    );
    assert_eq!(
        cpu_ceiling(),
        None,
        "a refused ceiling freezes nothing but the default"
    );
}

#[test]
fn scalar_mask_lower_bits_zero_n_returns_zero() {
    assert_eq!(ScalarKernel::mask_lower_bits(0xDEADBEEF, 0), 0);
}

#[test]
fn scalar_mask_lower_bits_full_64_returns_full_value() {
    assert_eq!(
        ScalarKernel::mask_lower_bits(0xFFFF_FFFF_FFFF_FFFF, 64),
        0xFFFF_FFFF_FFFF_FFFF
    );
}

#[test]
fn scalar_mask_lower_bits_mid_keeps_low_n_bits() {
    // n=8: keep low 8 bits, zero the rest
    assert_eq!(ScalarKernel::mask_lower_bits(0xDEAD_BEEF, 8), 0xEF);
    assert_eq!(
        ScalarKernel::mask_lower_bits(0x0102_0304_0506_0708, 16),
        0x0708
    );
}

// Gated on `std` AND `kernel-avx2`: the `is_x86_feature_detected!`
// guard below is a no-op under `--no-default-features` (no std,
// no runtime feature detection), so the test body would call
// `Avx2Kernel::mask_lower_bits` unconditionally and SIGILL on any
// non-BMI2 CPU — hence `feature = "std"`. `Avx2Kernel` itself is
// `#[cfg(feature = "kernel-avx2")]`, so the test must also require
// that feature or a `std`-only trimmed build (`kernel-avx2` off)
// fails to compile against the undefined type.
#[cfg(all(target_arch = "x86_64", feature = "std", feature = "kernel-avx2"))]
#[test]
fn avx2_mask_lower_bits_matches_scalar_on_bmi2_hw() {
    // Only run when BMI2 actually available — otherwise constructing
    // Avx2Kernel via dispatch wouldn't happen.
    if !std::arch::is_x86_feature_detected!("bmi2") {
        return;
    }
    for n in 0..=64u8 {
        let v = 0x1234_5678_9ABC_DEF0u64;
        assert_eq!(
            Avx2Kernel::mask_lower_bits(v, n),
            ScalarKernel::mask_lower_bits(v, n),
            "mismatch at n={}",
            n
        );
    }
}

/// Regression: a CPU advertising AVX-512 VBMI2 but NOT AVX2 (the
/// AMD64 baseline allows this combination at the spec level) was
/// previously selected as `Vbmi2`, which would SIGILL on the
/// first AVX2-mixed VBMI2 kernel invocation. The selection must
/// fall through to Scalar (or a non-AVX tier) in that case.
#[cfg(all(target_arch = "x86_64", feature = "kernel-vbmi2"))]
#[test]
fn select_x86_kernel_vbmi2_without_avx2_does_not_pick_vbmi2() {
    let tag = select_x86_kernel(
        /* avx512vbmi2 */ true, /* avx512f */ true, /* avx512vl */ true,
        /* avx512bw */ true, /* bmi2 */ true, /* avx2 */ false,
        /* sse2 */ true,
    );
    assert_ne!(
        tag,
        CpuKernelTag::Vbmi2,
        "selecting Vbmi2 without AVX2 would call AVX2 instructions and SIGILL"
    );
}

/// Sanity: when every flag is present the selector returns Vbmi2.
#[cfg(all(target_arch = "x86_64", feature = "kernel-vbmi2"))]
#[test]
fn select_x86_kernel_full_x86_v4_picks_vbmi2() {
    let tag = select_x86_kernel(true, true, true, true, true, true, true);
    assert_eq!(tag, CpuKernelTag::Vbmi2);
}

/// Sanity: AVX2 + BMI2 without AVX-512 → Avx2.
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
#[test]
fn select_x86_kernel_avx2_baseline_picks_avx2() {
    let tag = select_x86_kernel(false, false, false, false, true, true, true);
    assert_eq!(tag, CpuKernelTag::Avx2);
}

/// SSE2-only (no BMI2/AVX2) → Sse2, the x86_64 floor above Scalar.
#[cfg(all(target_arch = "x86_64", feature = "kernel-sse"))]
#[test]
fn select_x86_kernel_sse2_only_picks_sse2() {
    let tag = select_x86_kernel(false, false, false, false, false, false, true);
    assert_eq!(tag, CpuKernelTag::Sse2);
}

/// No SIMD flags at all → Scalar (off-x86_64 / pre-SSE2 x86).
#[cfg(target_arch = "x86_64")]
#[test]
fn select_x86_kernel_no_features_picks_scalar() {
    let tag = select_x86_kernel(false, false, false, false, false, false, false);
    assert_eq!(tag, CpuKernelTag::Scalar);
}

#[test]
fn detect_returns_consistent_tag() {
    let first = detect_cpu_kernel();
    let second = detect_cpu_kernel();
    assert_eq!(
        first, second,
        "cached detect must return same tag on repeated calls"
    );
}

#[test]
fn active_kernel_name_is_known_lowercase_tier() {
    // The diagnostic name must be one of the stable lowercase tier
    // strings the dashboard parses, and must match whatever tier
    // detection resolves to on this host (no `unknown` / empty leak).
    const KNOWN: &[&str] = &["scalar", "sse2", "bmi2", "avx2", "vbmi2", "neon", "sve"];
    let name = active_cpu_kernel_name();
    assert!(
        KNOWN.contains(&name),
        "active kernel name {name:?} is not a recognised tier"
    );
    assert_eq!(
        name,
        name.to_ascii_lowercase(),
        "tier name must be lowercase for stable dashboard parsing"
    );
}

#[test]
fn every_kernel_tag_maps_to_its_lowercase_name() {
    // `active_cpu_kernel_name` only exercises whichever arm the running
    // CPU resolves to, so map each constructible tag directly to cover
    // every branch on this build's feature set.
    assert_eq!(CpuKernelTag::Scalar.name(), "scalar");
    #[cfg(all(target_arch = "x86_64", feature = "kernel-sse"))]
    assert_eq!(CpuKernelTag::Sse2.name(), "sse2");
    #[cfg(all(target_arch = "x86_64", feature = "kernel-bmi2"))]
    assert_eq!(CpuKernelTag::Bmi2.name(), "bmi2");
    #[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
    assert_eq!(CpuKernelTag::Avx2.name(), "avx2");
    #[cfg(all(target_arch = "x86_64", feature = "kernel-vbmi2"))]
    assert_eq!(CpuKernelTag::Vbmi2.name(), "vbmi2");
    #[cfg(all(target_arch = "aarch64", feature = "kernel-neon"))]
    assert_eq!(CpuKernelTag::Neon.name(), "neon");
    #[cfg(all(
        target_arch = "aarch64",
        feature = "kernel-sve",
        any(feature = "std", target_feature = "sve"),
    ))]
    assert_eq!(CpuKernelTag::Sve.name(), "sve");
}

#[test]
fn active_kernel_name_is_stable_across_calls() {
    // Backed by the cached `detect_cpu_kernel`, so repeated calls must
    // return the identical static string.
    assert_eq!(active_cpu_kernel_name(), active_cpu_kernel_name());
}
