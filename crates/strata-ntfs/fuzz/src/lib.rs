//! Link-time support shared by the fuzz targets. Holds no code, only data,
//! so the coverage pass never instruments this crate.

// COMPAT: on windows-msvc, LLVM puts inline 8-bit counters in the grouped
// COFF section `.SCOV$CM` and the PC table in `.SCOVP$M`, and the bounds
// symbols that ELF linkers synthesize must be defined by hand. compiler-rt
// does this in sanitizer_coverage_win_sections.cpp, which libfuzzer-sys does
// not build. `$A` sorts before `$M` and `$Z` after it, so these symbols
// bracket every counter and PC entry in the image. Each start sentinel adds
// exactly one counter and one PC-table entry, keeping the two tables the
// same length as libFuzzer requires. They must live outside the target
// crates: when a module both defines these names and is instrumented, LLVM
// renames its own references (`__start___sancov_cntrs.85`) and linking fails.
#[cfg(all(windows, fuzzing))]
#[allow(non_upper_case_globals)]
mod sancov_bounds {
    #[unsafe(link_section = ".SCOV$CA")]
    #[unsafe(no_mangle)]
    #[used]
    static mut __start___sancov_cntrs: u8 = 0;

    #[unsafe(link_section = ".SCOV$CZ")]
    #[unsafe(no_mangle)]
    #[used]
    static mut __stop___sancov_cntrs: u8 = 0;

    #[unsafe(link_section = ".SCOVP$A")]
    #[unsafe(no_mangle)]
    #[used]
    static __start___sancov_pcs: [u64; 2] = [0; 2];

    #[unsafe(link_section = ".SCOVP$Z")]
    #[unsafe(no_mangle)]
    #[used]
    static __stop___sancov_pcs: [u64; 2] = [0; 2];
}
