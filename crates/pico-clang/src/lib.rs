//! clang, linked into the compiler (decision 298): `pico clang …` runs clang's driver in the
//! pico process (cc1 and the assembler included), so building the runtime, the C libraries and
//! `c:` imports needs no LLVM installation; and LLVM's archive writer for static libraries.

#[cfg(pico_clang)]
use std::ffi::{CStr, CString, c_char};
#[cfg(pico_clang)]
use std::path::Path;

// Used only for its link order (see Cargo.toml).
use inkwell as _;

/// Whether this compiler was built with clang.
pub const AVAILABLE: bool = cfg!(pico_clang);

/// The resource directory of the clang pico was built with (`lib/clang/<major>`, with the
/// builtin headers in `include/`).
pub fn build_resource_dir() -> Option<&'static str> {
    option_env!("PICO_CLANG_RESOURCE_DIR")
}

#[cfg(pico_clang)]
unsafe extern "C" {
    fn pico_clang_main(argc: i32, argv: *mut *mut c_char, exe: *const c_char) -> i32;
    fn pico_write_archive(
        out: *const c_char,
        members: *const *const c_char,
        n: i32,
        err: *mut *mut c_char,
    ) -> i32;
    fn pico_clang_free(p: *mut c_char);
}

/// Runs clang's driver with `args` (without the program name) and returns its exit status.
/// It sets up LLVM's global state, so the process should exit afterwards.
#[cfg(pico_clang)]
pub fn main(args: &[String]) -> i32 {
    let exe = std::env::current_exe().unwrap_or_else(|_| "pico".into());
    let exe = CString::new(exe.to_string_lossy().as_bytes()).unwrap_or_default();
    let mut owned: Vec<CString> = vec![CString::new("clang").unwrap()];
    owned.extend(
        args.iter()
            .map(|a| CString::new(a.as_bytes()).unwrap_or_default()),
    );
    let mut argv: Vec<*mut c_char> = owned.iter().map(|a| a.as_ptr() as *mut c_char).collect();
    argv.push(std::ptr::null_mut());
    // SAFETY: argv holds argc NUL-terminated strings (and a null), alive for the call.
    unsafe { pico_clang_main(owned.len() as i32, argv.as_mut_ptr(), exe.as_ptr()) }
}

#[cfg(not(pico_clang))]
pub fn main(_args: &[String]) -> i32 {
    eprintln!("error: this pico was built without clang");
    1
}

/// Writes the static library `out` (GNU format, deterministic, with a symbol table) of
/// `members`, replacing it.
#[cfg(pico_clang)]
pub fn write_archive(out: &Path, members: &[&Path]) -> Result<(), String> {
    let c = |p: &Path| CString::new(p.to_string_lossy().as_bytes()).map_err(|e| e.to_string());
    let out = c(out)?;
    let ms = members
        .iter()
        .map(|m| c(m))
        .collect::<Result<Vec<_>, _>>()?;
    let ptrs: Vec<*const c_char> = ms.iter().map(|m| m.as_ptr()).collect();
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: the pointers are NUL-terminated strings alive for the call; `err` is set on
    // failure and freed here.
    unsafe {
        if pico_write_archive(out.as_ptr(), ptrs.as_ptr(), ptrs.len() as i32, &mut err) == 0 {
            return Ok(());
        }
        let msg = CStr::from_ptr(err).to_string_lossy().into_owned();
        pico_clang_free(err);
        Err(msg)
    }
}

#[cfg(not(pico_clang))]
pub fn write_archive(_out: &std::path::Path, _members: &[&std::path::Path]) -> Result<(), String> {
    Err("this pico was built without clang".into())
}
