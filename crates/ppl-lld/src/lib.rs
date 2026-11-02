//! lld, linked into the compiler (spec §15.2: no system linker): the ELF and COFF linkers,
//! and LLVM's import-library writer for Windows DLLs.

use std::ffi::{CStr, CString, c_char};
use std::path::Path;
use std::sync::Mutex;

// Used only for its link order (see Cargo.toml).
use inkwell as _;

/// Whether this compiler was built with lld.
pub const AVAILABLE: bool = cfg!(ppl_lld);

/// COFF machine numbers for [`write_import_lib`].
pub const COFF_AMD64: u32 = 0x8664;
pub const COFF_ARM64: u32 = 0xaa64;

#[cfg(ppl_lld)]
unsafe extern "C" {
    fn ppl_lld_link(
        argv: *const *const c_char,
        argc: i32,
        out: *mut *mut c_char,
        can_run_again: *mut i32,
    ) -> i32;
    fn ppl_write_import_lib(
        def: *const c_char,
        machine: u32,
        lib: *const c_char,
        out: *mut *mut c_char,
    ) -> i32;
    fn ppl_lld_free(p: *mut c_char);
}

/// lld keeps its state in globals: one link at a time, and none after a crash it can't
/// recover from.
static LOCK: Mutex<bool> = Mutex::new(true);

/// Runs `ld.lld` with these arguments (without the program name). On failure, the error is
/// lld's output.
pub fn link_elf(args: &[String]) -> Result<(), String> {
    link("ld.lld", args)
}

/// Runs `lld-link` (the COFF linker, MSVC-style arguments).
pub fn link_coff(args: &[String]) -> Result<(), String> {
    link("lld-link", args)
}

fn link(flavor: &str, args: &[String]) -> Result<(), String> {
    let mut usable = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !*usable {
        return Err("lld crashed earlier in this process".into());
    }
    let cs: Vec<CString> = std::iter::once(flavor)
        .chain(args.iter().map(String::as_str))
        .map(cstring)
        .collect::<Result<_, _>>()?;
    run(&cs, &mut usable)
}

/// Writes the import library `lib` for a preprocessed MinGW `.def` file, like
/// `llvm-dlltool --kill-at`.
pub fn write_import_lib(def: &Path, machine: u32, lib: &Path) -> Result<(), String> {
    let def = cstring(&def.display().to_string())?;
    let lib = cstring(&lib.display().to_string())?;
    write_lib(&def, machine, &lib)
}

fn cstring(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| format!("NUL in linker argument `{s}`"))
}

/// Takes ownership of a message string the shim allocated.
#[cfg(ppl_lld)]
fn take(out: *mut c_char) -> String {
    // SAFETY: `out` is the shim's NUL-terminated allocation, freed exactly once.
    unsafe {
        let t = CStr::from_ptr(out).to_string_lossy().into_owned();
        ppl_lld_free(out);
        t
    }
}

#[cfg(ppl_lld)]
fn run(cs: &[CString], usable: &mut bool) -> Result<(), String> {
    let ptrs: Vec<*const c_char> = cs.iter().map(|c| c.as_ptr()).collect();
    let mut out = std::ptr::null_mut();
    let mut again = 1;
    // SAFETY: `ptrs` points at NUL-terminated strings that outlive the call; the shim sets
    // `out` to a string we free with its own deallocator.
    let code = unsafe { ppl_lld_link(ptrs.as_ptr(), ptrs.len() as i32, &mut out, &mut again) };
    let text = take(out);
    *usable = again != 0;
    if code == 0 { Ok(()) } else { Err(text) }
}

#[cfg(ppl_lld)]
fn write_lib(def: &CStr, machine: u32, lib: &CStr) -> Result<(), String> {
    let mut out = std::ptr::null_mut();
    // SAFETY: both paths are NUL-terminated and outlive the call; `out` is set by the shim.
    let code = unsafe { ppl_write_import_lib(def.as_ptr(), machine, lib.as_ptr(), &mut out) };
    let text = take(out);
    if code == 0 { Ok(()) } else { Err(text) }
}

#[cfg(not(ppl_lld))]
const MISSING: &str = "this ppl was built without lld (set PPL_LLD_PREFIX to an lld installation)";

#[cfg(not(ppl_lld))]
fn run(_: &[CString], _: &mut bool) -> Result<(), String> {
    Err(MISSING.into())
}

#[cfg(not(ppl_lld))]
fn write_lib(_: &CStr, _: u32, _: &CStr) -> Result<(), String> {
    Err(MISSING.into())
}

#[cfg(all(test, ppl_lld))]
mod tests {
    #[test]
    fn import_library_from_def() {
        let dir = std::env::temp_dir().join(format!("ppl-lld-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let def = dir.join("k.def");
        std::fs::write(
            &def,
            "LIBRARY kernel32.dll\nEXPORTS\nExitProcess\nWriteFile@20\n",
        )
        .unwrap();
        for machine in [super::COFF_AMD64, super::COFF_ARM64] {
            let lib = dir.join(format!("k{machine:x}.lib"));
            super::write_import_lib(&def, machine, &lib).unwrap();
            let bytes = std::fs::read(&lib).unwrap();
            assert!(bytes.starts_with(b"!<arch>\n"));
            let text = String::from_utf8_lossy(&bytes);
            assert!(text.contains("__imp_ExitProcess") && text.contains("kernel32.dll"));
            // --kill-at: the decoration is gone.
            assert!(text.contains("__imp_WriteFile") && !text.contains("WriteFile@20"));
        }
        assert!(
            super::write_import_lib(
                &dir.join("missing.def"),
                super::COFF_AMD64,
                &dir.join("x.lib")
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
