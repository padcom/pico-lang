//! lld, linked into the compiler (spec §15.2: no system linker). Only the ELF flavor so far.

use std::sync::Mutex;

// Used only for its link order (see Cargo.toml).
use inkwell as _;

/// Whether this compiler was built with lld.
pub const AVAILABLE: bool = cfg!(ppl_lld);

#[cfg(ppl_lld)]
unsafe extern "C" {
    fn ppl_lld_link(
        argv: *const *const std::ffi::c_char,
        argc: i32,
        out: *mut *mut std::ffi::c_char,
        can_run_again: *mut i32,
    ) -> i32;
    fn ppl_lld_free(p: *mut std::ffi::c_char);
}

/// lld keeps its state in globals: one link at a time, and none after a crash it can't
/// recover from.
static LOCK: Mutex<bool> = Mutex::new(true);

/// Runs `ld.lld` with these arguments (without the program name). On failure, the error is
/// lld's output.
pub fn link_elf(args: &[String]) -> Result<(), String> {
    let mut usable = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !*usable {
        return Err("lld crashed earlier in this process".into());
    }
    run(args, &mut usable)
}

#[cfg(ppl_lld)]
fn run(args: &[String], usable: &mut bool) -> Result<(), String> {
    use std::ffi::{CStr, CString};
    let cs: Vec<CString> = std::iter::once("ld.lld")
        .chain(args.iter().map(String::as_str))
        .map(|a| CString::new(a).map_err(|_| format!("NUL in linker argument `{a}`")))
        .collect::<Result<_, _>>()?;
    let ptrs: Vec<*const std::ffi::c_char> = cs.iter().map(|c| c.as_ptr()).collect();
    let mut out = std::ptr::null_mut();
    let mut again = 1;
    // SAFETY: `ptrs` points at NUL-terminated strings that outlive the call; the shim sets
    // `out` to a string we free with its own deallocator.
    let code = unsafe { ppl_lld_link(ptrs.as_ptr(), ptrs.len() as i32, &mut out, &mut again) };
    // SAFETY: `out` is the shim's NUL-terminated allocation, freed exactly once.
    let text = unsafe {
        let t = CStr::from_ptr(out).to_string_lossy().into_owned();
        ppl_lld_free(out);
        t
    };
    *usable = again != 0;
    if code == 0 { Ok(()) } else { Err(text) }
}

#[cfg(not(ppl_lld))]
fn run(_: &[String], _: &mut bool) -> Result<(), String> {
    Err("this ppl was built without lld (set PPL_LLD_PREFIX to an lld installation)".into())
}
