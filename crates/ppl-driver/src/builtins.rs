//! Compiler builtins (what libgcc or compiler-rt give C programs: 128-bit integer division,
//! soft-float `long double` on aarch64, …), compiled from the vendored compiler-rt
//! (`third_party/compiler-rt`) per target and cached, then linked as needed into every
//! program lld links.

use crate::link::{Clang, Job, cached_build};
use crate::target::{Arch, Os, Target};
use std::path::PathBuf;

/// compiler-rt's `GENERIC_SOURCES` (lib/builtins/CMakeLists.txt) without the Apple-only and
/// PowerPC-only files.
#[rustfmt::skip]
const GENERIC: &[&str] = &[
    "absvdi2", "absvsi2", "absvti2", "adddf3", "addsf3", "addvdi3", "addvsi3", "addvti3",
    "ashldi3", "ashlti3", "ashrdi3", "ashrti3", "bswapdi2", "bswapsi2", "clzdi2", "clzsi2",
    "clzti2", "cmpdi2", "cmpti2", "comparedf2", "comparesf2", "ctzdi2", "ctzsi2", "ctzti2",
    "divdc3", "divdf3", "divdi3", "divmoddi4", "divmodsi4", "divmodti4", "divsc3", "divsf3",
    "divsi3", "divti3", "extendsfdf2", "extendhfsf2", "extendhfdf2", "ffsdi2", "ffssi2", "ffsti2",
    "fixdfdi", "fixdfsi", "fixdfti", "fixsfdi", "fixsfsi", "fixsfti", "fixunsdfdi", "fixunsdfsi",
    "fixunsdfti", "fixunssfdi", "fixunssfsi", "fixunssfti", "floatdidf", "floatdisf", "floatsidf",
    "floatsisf", "floattidf", "floattisf", "floatundidf", "floatundisf", "floatunsidf",
    "floatunsisf", "floatuntidf", "floatuntisf", "fp_mode", "int_util", "lshrdi3", "lshrti3",
    "moddi3", "modsi3", "modti3", "muldc3", "muldf3", "muldi3", "mulodi4", "mulosi4", "muloti4",
    "mulsc3", "mulsf3", "multi3", "mulvdi3", "mulvsi3", "mulvti3", "negdf2", "negdi2", "negsf2",
    "negti2", "negvdi2", "negvsi2", "negvti2", "paritydi2", "paritysi2", "parityti2",
    "popcountdi2", "popcountsi2", "popcountti2", "powidf2", "powisf2", "subdf3", "subsf3",
    "subvdi3", "subvsi3", "subvti3", "truncdfhf2", "truncdfsf2", "truncsfhf2", "ucmpdi2",
    "ucmpti2", "udivdi3", "udivmoddi4", "udivmodsi4", "udivmodti4", "udivsi3", "udivti3",
    "umoddi3", "umodsi3", "umodti3",
];

/// `GENERIC_TF_SOURCES`: 128-bit floating point (aarch64's `long double`).
#[rustfmt::skip]
const TF: &[&str] = &[
    "addtf3", "comparetf2", "divtc3", "divtf3", "extenddftf2", "extendhftf2", "extendsftf2",
    "fixtfdi", "fixtfsi", "fixtfti", "fixunstfdi", "fixunstfsi", "fixunstfti", "floatditf",
    "floatsitf", "floattitf", "floatunditf", "floatunsitf", "floatuntitf", "multc3", "multf3",
    "powitf2", "subtf3", "trunctfdf2", "trunctfhf2", "trunctfsf2",
];

/// `x86_80_BIT_SOURCES`: x86's 80-bit `long double`.
#[rustfmt::skip]
const X86_80: &[&str] = &[
    "divxc3", "extendhfxf2", "extendxftf2", "fixxfdi", "fixxfti", "fixunsxfdi", "fixunsxfsi",
    "fixunsxfti", "floatdixf", "floattixf", "floatundixf", "floatuntixf", "mulxc3", "powixf2",
    "trunctfxf2", "truncxfhf2",
];

/// The builtins compiled for `t`, to link as needed.
pub fn objects(t: Target, cc: &Clang) -> Result<Vec<PathBuf>, String> {
    let src = ppl_runtime::compiler_rt_dir().join("lib/builtins");
    let mut common: Vec<String> = [
        format!("--target={}", t.clang_triple()),
        "-nostdinc".into(),
        "-isystem".into(),
        format!("{}/include", cc.resource_dir),
        "-ffreestanding".into(),
        "-fno-builtin".into(),
        "-std=c11".into(),
        "-O2".into(),
        "-fPIC".into(),
        // As compiler-rt builds them: not exported from the programs they're linked into.
        "-fvisibility=hidden".into(),
        "-DVISIBILITY_HIDDEN".into(),
        "-ffunction-sections".into(),
        "-fdata-sections".into(),
        "-w".into(),
    ]
    .into();
    if t.arch == Arch::Aarch64 {
        common.push("-mno-outline-atomics".into());
    }
    if t.os == Os::Windows {
        // (int_util.c includes <stdlib.h> there, even freestanding.)
        common.push("-isystem".into());
        common.push(crate::mingw::headers().display().to_string());
    }
    let mut names: Vec<&str> = GENERIC.iter().chain(TF).copied().collect();
    if t.arch == Arch::X86_64 {
        names.extend(X86_80);
    }
    let mut jobs: Vec<Job> = names
        .iter()
        .map(|n| Job {
            src: src.join(format!("{n}.c")),
            obj: format!("{n}.o"),
            args: common.clone(),
            start: false,
        })
        .collect();
    if t.os == Os::Windows {
        // The stack probe Windows code calls for big frames (`___chkstk_ms`, `__chkstk`).
        let arch = match t.arch {
            Arch::X86_64 => "x86_64",
            _ => "aarch64",
        };
        jobs.push(Job {
            src: src.join(format!("{arch}/chkstk.S")),
            obj: "chkstk.o".into(),
            args: common.clone(),
            start: false,
        });
    }
    // compiler-rt's headers aren't in the key: they're vendored and change only with it.
    let dir = cached_build("builtins", t.llvm_triple(), &jobs, &[], |_| Ok(()))?;
    Ok(jobs.iter().map(|j| dir.join(&j.obj)).collect())
}
