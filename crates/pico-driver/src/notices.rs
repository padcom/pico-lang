//! The license notices of what a build links in (#106): `pico build` writes
//! `<output>.notices.txt` next to the executable, with the license texts of pico's runtime and
//! of every third-party component in it. The linker writes a map of the objects it took; a
//! component is in when one of its objects is in the map (Mbed TLS only when the program uses
//! TLS or `std/crypto`, musl only on musl targets and for Windows' math, …).

use std::path::{Path, PathBuf};

/// A component a program can contain, in the order the notices list them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Part {
    /// The runtime and the standard library: always.
    Pico,
    MbedTls,
    /// DEFLATE (WebSocket compression).
    Miniz,
    Musl,
    Mingw,
    WasiLibc,
    /// The WASI preview 1 → preview 2 adapter in `wasm32-wasi` components.
    WasiAdapter,
    CompilerRt,
}

const PICO: &str = include_str!("../../../LICENSE");
const MBEDTLS: &str = include_str!("../../../third_party/mbedtls/LICENSE");
const MINIZ: &str = include_str!("../../../third_party/miniz/LICENSE");
const MUSL: &str = include_str!("../../../third_party/musl/COPYRIGHT");
const MINGW: &str = include_str!("../../../third_party/zig-libc/mingw/COPYING");
const WASI_LIBC: &str = include_str!("../../../third_party/zig-libc/wasi/LICENSE");
const WASI_MIT: &str = include_str!("../../../third_party/zig-libc/wasi/LICENSE-MIT");
const CLOUDLIBC: &str =
    include_str!("../../../third_party/zig-libc/wasi/libc-bottom-half/cloudlibc/LICENSE");
const APACHE_LLVM: &str = include_str!("../../../third_party/zig-libc/wasi/LICENSE-APACHE-LLVM");
const COMPILER_RT: &str = include_str!("../../../third_party/compiler-rt/LICENSE.TXT");

/// winpthreads' license, from its source files' headers (it has no file of its own).
const WINPTHREADS: &str = "Copyright (c) 2011-2016  mingw-w64 project

Permission is hereby granted, free of charge, to any person obtaining a
copy of this software and associated documentation files (the \"Software\"),
to deal in the Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, sublicense,
and/or sell copies of the Software, and to permit persons to whom the
Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
";

/// emmalloc's notice (wasi-libc's malloc, from Emscripten; `emmalloc/emmalloc.c`).
const EMMALLOC: &str = "Copyright 2018 The Emscripten Authors.  All rights reserved.
Emscripten is available under two separate licenses, the MIT license and the
University of Illinois/NCSA Open Source License.
";

impl Part {
    fn title(self) -> &'static str {
        match self {
            Part::Pico => "Pico (the runtime and the standard library), MIT",
            Part::MbedTls => "Mbed TLS 3.6, Apache-2.0 (chosen of Apache-2.0 OR GPL-2.0-or-later)",
            Part::Miniz => "miniz 3.1.2, MIT",
            Part::Musl => "musl libc, MIT",
            Part::Mingw => "mingw-w64 (the CRT, ZPL-2.1, and winpthreads, MIT)",
            Part::WasiLibc => {
                "wasi-libc, MIT (chosen of Apache-2.0 WITH LLVM-exception, Apache-2.0 and MIT)"
            }
            Part::WasiAdapter => {
                "wasmtime's WASI preview 1 adapter, Apache-2.0 WITH LLVM-exception"
            }
            Part::CompilerRt => "LLVM compiler-rt (builtins), Apache-2.0 WITH LLVM-exception",
        }
    }

    /// The texts to reproduce: (what it is, text).
    fn texts(self) -> Vec<(&'static str, &'static str)> {
        match self {
            Part::Pico => vec![("LICENSE", PICO)],
            Part::MbedTls => vec![("LICENSE", MBEDTLS)],
            Part::Miniz => vec![("LICENSE", MINIZ)],
            Part::Musl => vec![("COPYRIGHT", MUSL)],
            Part::Mingw => vec![("COPYING", MINGW), ("winpthreads", WINPTHREADS)],
            Part::WasiLibc => vec![
                ("LICENSE", WASI_LIBC),
                ("LICENSE-MIT", WASI_MIT),
                ("musl's COPYRIGHT (libc-top-half/musl)", MUSL),
                (
                    "cloudlibc's LICENSE (libc-bottom-half/cloudlibc)",
                    CLOUDLIBC,
                ),
                ("emmalloc", EMMALLOC),
            ],
            Part::WasiAdapter => vec![("LICENSE", APACHE_LLVM)],
            Part::CompilerRt => vec![("LICENSE.TXT", COMPILER_RT)],
        }
    }
}

/// The notices file of the executable `exe`: `app.notices.txt` (`app.exe.notices.txt`).
pub fn notices_file(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".notices.txt");
    exe.with_file_name(name)
}

/// The linker map a link writes for the notices file `notices` (removed once it's read).
pub(crate) fn map_file(notices: &Path) -> PathBuf {
    notices.with_extension("map")
}

/// Writes `notices` from the linker's `map`: [`Part::Pico`], the parts in `always`, and each
/// part of `offered` one of whose objects (or archives) the map names.
pub(crate) fn write(
    notices: &Path,
    map: &Path,
    offered: &[(Part, &[PathBuf])],
    always: &[Part],
) -> Result<(), String> {
    let bytes = std::fs::read(map).map_err(|e| format!("cannot read {}: {e}", map.display()))?;
    let _ = std::fs::remove_file(map);
    let text = String::from_utf8_lossy(&bytes);
    let mut parts = vec![Part::Pico];
    parts.extend_from_slice(always);
    for (part, objs) in offered {
        if objs.iter().any(|o| mentions(&text, o)) {
            parts.push(*part);
        }
    }
    parts.sort();
    parts.dedup();
    std::fs::write(notices, render(&parts))
        .map_err(|e| format!("cannot write {}: {e}", notices.display()))
}

/// Whether the map names `path` as a whole: followed by what ends a path there (`:(.text)`,
/// `(member.o)`, a line end), so `a/mbedtls_ssl.o` isn't found in `a/mbedtls_ssl_tls.o`.
fn mentions(map: &str, path: &Path) -> bool {
    let p = path.display().to_string();
    map.match_indices(&p).any(|(i, _)| {
        map[i + p.len()..]
            .chars()
            .next()
            .is_none_or(|c| matches!(c, ':' | '(' | ')' | '\n' | '\r' | ' ' | '\t'))
    })
}

/// The file's text: what's in the program, then each license.
pub(crate) fn render(parts: &[Part]) -> String {
    let rule = "=".repeat(78);
    let mut s = String::from(
        "Third-party notices: the program contains the following components, under these licenses.\n\n",
    );
    for p in parts {
        s.push_str(&format!("- {}\n", p.title()));
    }
    for p in parts {
        for (what, text) in p.texts() {
            s.push_str(&format!("\n{rule}\n{}: {what}\n{rule}\n\n", p.title()));
            s.push_str(text.trim_end());
            s.push('\n');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_whole_paths_only() {
        let map = "  /c/mbedtls_ssl_tls.o:(.text)\n  /c/libm.a(sin.o)\n";
        assert!(mentions(map, Path::new("/c/mbedtls_ssl_tls.o")));
        assert!(!mentions(map, Path::new("/c/mbedtls_ssl.o")));
        assert!(mentions(map, Path::new("/c/libm.a")));
        assert!(!mentions(map, Path::new("/c/lib")));
    }

    #[test]
    fn lists_then_reproduces_each_license() {
        let s = render(&[Part::Pico, Part::Musl]);
        assert!(
            s.contains("- Pico (the runtime and the standard library), MIT\n- musl libc, MIT\n")
        );
        assert!(s.contains("Permission is hereby granted"));
        assert!(s.contains("musl as a whole is licensed under the following standard MIT license"));
        assert!(!s.contains("Apache"));
    }
}
