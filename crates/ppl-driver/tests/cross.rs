//! Cross-compilation (spec §15): `linux-x64` from another host gives an x86-64 ELF linked by
//! the built-in lld, which runs under qemu-user when that's set up.

use std::path::Path;

/// The x86-64 glibc the build links against and qemu loads (Debian's `libc6-dev-amd64-cross`).
const SYSROOT: &str = "/usr/x86_64-linux-gnu";

#[test]
fn linux_x64() {
    let target = ppl_driver::Target::parse("linux-x64").unwrap();
    if !ppl_lld::AVAILABLE || target.is_host() || !Path::new(SYSROOT).join("lib/Scrt1.o").is_file()
    {
        eprintln!("skipped: needs lld, a non-x86-64 host and {SYSROOT}");
        return;
    }
    let checked = ppl_driver::check_source(
        "cross.ppl",
        "println(`hello from ${Math.cbrt(27)} ${[1, 2, 3].map((x: int) => x * 2):?}`)\n",
    );
    assert!(!checked.has_errors(), "{}", checked.render_diagnostics());
    let opts = ppl_driver::BuildOptions {
        release: false,
        target: Some(target),
    };
    let exe = ppl_driver::build_cached(&checked, opts).unwrap();
    let bytes = std::fs::read(&exe).unwrap();
    assert_eq!(&bytes[..4], b"\x7fELF");
    assert_eq!(
        u16::from_le_bytes([bytes[18], bytes[19]]),
        62,
        "e_machine: x86-64"
    );
    if !Path::new("/proc/sys/fs/binfmt_misc/qemu-x86_64").exists() {
        eprintln!("not run: no qemu-x86_64 binfmt handler");
        return;
    }
    let out = std::process::Command::new(&exe)
        .env("QEMU_LD_PREFIX", SYSROOT)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hello from 3 [2, 4, 6]\n"
    );
}
