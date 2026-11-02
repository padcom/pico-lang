//! glibc stub libraries (spec §15.2, decision 245): for a glibc version, shared objects that
//! define exactly the symbols (and symbol versions) that version's libraries export, so lld
//! links against them without a system glibc. They have no code: at run time, the real glibc
//! provides everything. Built the way Zig builds them, from its `abilists`
//! (`third_party/zig-libc`).

use crate::target::{Arch, Target};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// glibc's libraries in link order: name, soname version, and the version that merged it into
/// libc (2.34 for pthread, dl, rt and util).
/// (name, soname version, merged into libc in)
type Lib = (&'static str, u8, Option<(u8, u8)>);

const LIBS: [Lib; 8] = [
    ("m", 6, None),
    ("pthread", 0, Some((2, 34))),
    ("c", 6, None),
    ("dl", 2, Some((2, 34))),
    ("rt", 1, Some((2, 34))),
    ("ld", 2, None),
    ("util", 1, Some((2, 34))),
    ("resolv", 2, None),
];

/// Bump to invalidate stubs made by older compilers.
const STUB_CACHE_FORMAT: u32 = 1;

type Version = (u8, u8, u8);

/// The parsed `abilists`.
struct AbiLists {
    versions: Vec<Version>,
    targets: Vec<String>,
    /// The rest of the file: function inclusions, then object inclusions.
    inclusions: Vec<u8>,
}

fn abilists_path() -> PathBuf {
    ppl_runtime::zig_libc_dir().join("glibc/abilists")
}

impl AbiLists {
    fn load() -> Result<AbiLists, String> {
        let p = abilists_path();
        let bytes = std::fs::read(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        AbiLists::parse(&bytes).ok_or_else(|| format!("{} is malformed", p.display()))
    }

    fn parse(b: &[u8]) -> Option<AbiLists> {
        let mut r = Reader { b, at: 0 };
        let nlibs = r.byte()?;
        for i in 0..nlibs as usize {
            if r.cstr()? != LIBS.get(i)?.0 {
                return None;
            }
        }
        let versions = (0..r.byte()?)
            .map(|_| Some((r.byte()?, r.byte()?, r.byte()?)))
            .collect::<Option<_>>()?;
        let targets = (0..r.byte()?)
            .map(|_| r.cstr().map(str::to_string))
            .collect::<Option<_>>()?;
        Some(AbiLists {
            versions,
            targets,
            inclusions: b[r.at..].to_vec(),
        })
    }

    /// The index of the version to build for: the newest listed one not newer than `want`
    /// (versions without new symbols aren't listed).
    fn version_index(&self, want: (u16, u16)) -> Result<usize, String> {
        let want = (want.0 as u8, want.1 as u8, 0);
        let latest = *self.versions.last().expect("abilists has versions");
        if want > latest {
            return Err(format!(
                "glibc {}.{} is newer than the newest ppl knows, {}",
                want.0,
                want.1,
                show(latest)
            ));
        }
        Ok(self.versions.iter().rposition(|v| *v <= want).unwrap_or(0))
    }
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Option<u8> {
        let v = *self.b.get(self.at)?;
        self.at += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes([self.byte()?, self.byte()?]))
    }
    fn uleb(&mut self) -> Option<u64> {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let b = self.byte()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
        }
    }
    fn cstr(&mut self) -> Option<&str> {
        let end = self.at + self.b[self.at..].iter().position(|&c| c == 0)?;
        let s = std::str::from_utf8(&self.b[self.at..end]).ok()?;
        self.at = end + 1;
        Some(s)
    }
}

fn show(v: Version) -> String {
    if v.2 == 0 {
        format!("{}.{}", v.0, v.1)
    } else {
        format!("{}.{}.{}", v.0, v.1, v.2)
    }
}

/// One stub library to build.
struct Stub {
    /// `libc.so.6`, …
    file: String,
    soname: String,
    asm: String,
}

/// The version script defining every version up to the target's, and each library's assembly.
fn stub_sources(abi: &AbiLists, t: Target) -> Result<(String, Vec<Stub>), String> {
    let name = t.multiarch();
    let ti = abi
        .targets
        .iter()
        .position(|n| *n == name)
        .ok_or_else(|| format!("no glibc ABI list for {name}"))?;
    let vi = abi.version_index(t.glibc.expect("a gnu target"))?;
    let glibc = abi.versions[vi];
    let mut map = String::new();
    for v in &abi.versions[..=vi] {
        writeln!(map, "GLIBC_{} {{ }};", show(*v)).unwrap();
    }
    let mut stubs = Vec::new();
    for (li, &(lib, sover, merged)) in LIBS.iter().enumerate() {
        if merged.is_some_and(|(a, b)| (glibc.0, glibc.1) >= (a, b)) {
            continue;
        }
        let mut asm = String::from(".text\n");
        let mut r = Reader {
            b: &abi.inclusions,
            at: 0,
        };
        emit(&mut r, &mut asm, li, ti, vi, abi, false).ok_or("abilists is malformed")?;
        asm.push_str(".data\n");
        if lib == "c" {
            // The real libc refers to `_IO_stdin_used`, which puts the program's (from our
            // start file) into its dynamic symbol table; without it glibc assumes an ancient
            // `FILE` layout.
            // (In .data: in .rodata, the reference would need a text relocation.)
            asm.push_str(".balign 8\n.globl _IO_stdin_used\n.quad _IO_stdin_used\n");
        }
        emit(&mut r, &mut asm, li, ti, vi, abi, true).ok_or("abilists is malformed")?;
        let file = format!("lib{lib}.so.{sover}");
        let soname = if lib == "ld" {
            match t.arch {
                Arch::X86_64 => "ld-linux-x86-64.so.2".to_string(),
                _ => "ld-linux-aarch64.so.1".to_string(),
            }
        } else {
            file.clone()
        };
        stubs.push(Stub { file, soname, asm });
    }
    Ok((map, stubs))
}

/// Writes the stub definitions of the functions (or objects) in library `li` for target `ti`,
/// up to version `vi`. Each symbol gets one definition per version, the newest as the default
/// (`@@`), as `.symver`ed aliases.
fn emit(
    r: &mut Reader,
    out: &mut String,
    li: usize,
    ti: usize,
    vi: usize,
    abi: &AbiLists,
    objects: bool,
) -> Option<()> {
    let n = r.u16()?;
    let mut name: Option<String> = None;
    let mut found: Vec<(usize, u64)> = Vec::new();
    for _ in 0..n {
        let sym = match &name {
            Some(s) => s.clone(),
            None => {
                found.clear();
                let s = r.cstr()?.to_string();
                name = Some(s.clone());
                s
            }
        };
        let targets = r.uleb()?;
        let size = if objects { r.uleb()? } else { 0 };
        let mut lib = r.byte()?;
        let last_inclusion = lib & 0x80 != 0;
        lib &= 0x7f;
        let applies = lib as usize == li && ti < 64 && targets & (1 << ti) != 0;
        loop {
            let b = r.byte()?;
            let v = (b & 0x7f) as usize;
            if applies && v <= vi && !found.iter().any(|(f, _)| *f == v) {
                found.push((v, size));
            }
            if b & 0x80 != 0 {
                break;
            }
        }
        if !last_inclusion {
            continue;
        }
        name = None;
        let Some(default) = found.iter().map(|(v, _)| *v).max() else {
            continue;
        };
        for &(v, size) in &found {
            let ver = abi.versions[v];
            let alias = format!("{sym}_{}", show(ver).replace('.', "_"));
            let at = if v == default { "@@" } else { "@" };
            writeln!(out, ".balign 8\n.globl {alias}").unwrap();
            if objects {
                writeln!(out, ".type {alias}, %object\n.size {alias}, {size}").unwrap();
            } else {
                writeln!(out, ".type {alias}, %function").unwrap();
            }
            writeln!(out, ".symver {alias}, {sym}{at}GLIBC_{}, remove", show(ver)).unwrap();
            if objects {
                writeln!(out, "{alias}: .fill {size}, 1, 0").unwrap();
            } else {
                writeln!(out, "{alias}: .quad 0").unwrap();
            }
        }
    }
    Some(())
}

/// The stub libraries for `t` in link order, built on first use and cached.
pub fn stubs(t: Target, clang: &Path) -> Result<Vec<PathBuf>, String> {
    let abi = AbiLists::load()?;
    let (map, stubs) = stub_sources(&abi, t)?;
    let dir = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        STUB_CACHE_FORMAT.hash(&mut h);
        map.hash(&mut h);
        for s in &stubs {
            s.asm.hash(&mut h);
        }
        std::env::temp_dir()
            .join("ppl-cache")
            .join("glibc")
            .join(format!("{}-{:016x}", t.multiarch(), h.finish()))
    };
    let files: Vec<PathBuf> = stubs.iter().map(|s| dir.join(&s.file)).collect();
    if dir.is_dir() {
        return Ok(files);
    }
    let tmp = dir.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    let map_path = tmp.join("all.map");
    std::fs::write(&map_path, &map).map_err(|e| e.to_string())?;
    for s in &stubs {
        let asm = tmp.join(format!("{}.s", s.file));
        let obj = tmp.join(format!("{}.o", s.file));
        std::fs::write(&asm, &s.asm).map_err(|e| e.to_string())?;
        let out = std::process::Command::new(clang)
            .arg(format!("--target={}", t.llvm_triple()))
            .arg("-c")
            .arg(&asm)
            .arg("-o")
            .arg(&obj)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", clang.display()))?;
        if !out.status.success() {
            return Err(format!(
                "assembling the {} stub failed:\n{}",
                s.file,
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let args: Vec<String> = vec![
            "-m".into(),
            crate::link::emulation(t.arch).into(),
            "-shared".into(),
            "-soname".into(),
            s.soname.clone(),
            "--version-script".into(),
            map_path.display().to_string(),
            "-o".into(),
            tmp.join(&s.file).display().to_string(),
            obj.display().to_string(),
        ];
        ppl_lld::link_elf(&args)
            .map_err(|e| format!("linking the {} stub failed:\n{e}", s.file))?;
    }
    if std::fs::rename(&tmp, &dir).is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
        if !dir.is_dir() {
            return Err(format!("cannot create {}", dir.display()));
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(target: &str) -> (String, Vec<Stub>) {
        let abi = AbiLists::load().unwrap();
        stub_sources(&abi, Target::parse(target).unwrap()).unwrap()
    }

    fn lib<'a>(stubs: &'a [Stub], file: &str) -> &'a str {
        &stubs.iter().find(|s| s.file == file).unwrap().asm
    }

    #[test]
    fn versions_are_limited_to_the_target() {
        let (map, stubs) = sources("linux-x64");
        assert!(map.starts_with("GLIBC_2.0 { };\n"));
        assert!(map.ends_with("GLIBC_2.30 { };\nGLIBC_2.31 { };\n"), "{map}");
        let c = lib(&stubs, "libc.so.6");
        assert!(c.contains(".symver strtol_2_2_5, strtol@@GLIBC_2.2.5, remove"));
        assert!(
            !c.contains("__isoc23_strtol"),
            "a 2.38 symbol in 2.31 stubs"
        );
        // Before 2.34, threads are libpthread's.
        assert!(lib(&stubs, "libpthread.so.0").contains("pthread_create@@GLIBC_2.2.5"));
        assert!(c.contains("stdout@@GLIBC_2.2.5"));
        assert!(c.contains(".size stdout_2_2_5, 8"));
    }

    #[test]
    fn newer_versions() {
        let (_, stubs) = sources("aarch64-unknown-linux-gnu.2.39");
        assert!(stubs.iter().all(|s| s.file != "libpthread.so.0"));
        let c = lib(&stubs, "libc.so.6");
        assert!(c.contains("__isoc23_strtol@@GLIBC_2.38"));
        assert!(c.contains("pthread_create@@GLIBC_2.34"));
        assert!(
            Target::parse("linux-x64.2.99")
                .map_err(|e| e.to_string())
                .and_then(|t| AbiLists::load()?.version_index(t.glibc.unwrap()))
                .unwrap_err()
                .contains("newer")
        );
    }
}
