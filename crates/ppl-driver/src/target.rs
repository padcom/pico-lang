//! Compilation targets (spec §15.1): LLVM triples, their short aliases and the glibc version
//! suffix (`x86_64-unknown-linux-gnu.2.35`).

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Arch {
    X86_64,
    Aarch64,
    Wasm32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Os {
    Linux,
    Macos,
    Windows,
    Wasi,
    Browser,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Env {
    None,
    Gnu,
    Musl,
}

/// A target ppl compiles for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub arch: Arch,
    pub os: Os,
    pub env: Env,
    /// The glibc version linked against (major, minor); only for `linux-gnu`.
    pub glibc: Option<(u16, u16)>,
}

/// Spec §15.2: binaries link against glibc 2.31 unless the triple names a version.
pub const DEFAULT_GLIBC: (u16, u16) = (2, 31);

/// The triples of spec §15.1 that exist so far, with their aliases.
const TARGETS: &[(&str, &str)] = &[
    ("x86_64-unknown-linux-gnu", "linux-x64"),
    ("aarch64-unknown-linux-gnu", "linux-arm64"),
    ("x86_64-unknown-linux-musl", "linux-x64-musl"),
    ("aarch64-unknown-linux-musl", "linux-arm64-musl"),
    ("x86_64-apple-darwin", "macos-x64"),
    ("aarch64-apple-darwin", "macos-arm64"),
    ("x86_64-w64-windows-gnu", "windows-x64"),
    ("aarch64-w64-windows-gnu", "windows-arm64"),
    ("wasm32-unknown-unknown", "wasm32-browser"),
    // A preview 2 component, built from preview 1 objects (`clang_triple`; decision 257).
    ("wasm32-wasip2", "wasm32-wasi"),
];

impl Target {
    /// The machine the compiler runs on.
    pub fn host() -> Target {
        let arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else {
            "aarch64"
        };
        let rest = if cfg!(target_os = "macos") {
            "apple-darwin"
        } else if cfg!(target_os = "windows") {
            "w64-windows-gnu"
        } else if cfg!(target_env = "musl") {
            "unknown-linux-musl"
        } else {
            "unknown-linux-gnu"
        };
        Target::parse(&format!("{arch}-{rest}")).expect("the host is a known target")
    }

    /// Parses a triple or an alias, with an optional glibc suffix on `linux-gnu` ones
    /// (`linux-x64.2.35` works too).
    pub fn parse(s: &str) -> Result<Target, String> {
        let (name, glibc) = match s.split_once('.') {
            Some((n, v)) => (n, Some(v)),
            None => (s, None),
        };
        let Some(&(triple, _)) = TARGETS.iter().find(|(t, a)| *t == name || *a == name) else {
            let known: Vec<&str> = TARGETS.iter().map(|(_, a)| *a).collect();
            return Err(format!(
                "unknown target `{s}` (expected an LLVM triple or one of: {})",
                known.join(", ")
            ));
        };
        let arch = match triple.split('-').next() {
            Some("x86_64") => Arch::X86_64,
            Some("aarch64") => Arch::Aarch64,
            _ => Arch::Wasm32,
        };
        let (os, env) = if triple.ends_with("linux-gnu") {
            (Os::Linux, Env::Gnu)
        } else if triple.ends_with("linux-musl") {
            (Os::Linux, Env::Musl)
        } else if triple.ends_with("darwin") {
            (Os::Macos, Env::None)
        } else if triple.ends_with("windows-gnu") {
            (Os::Windows, Env::Gnu)
        } else if triple.ends_with("wasip1") || triple.ends_with("wasip2") {
            (Os::Wasi, Env::None)
        } else {
            (Os::Browser, Env::None)
        };
        let glibc = match (os, env, glibc) {
            (Os::Linux, Env::Gnu, None) => Some(DEFAULT_GLIBC),
            (Os::Linux, Env::Gnu, Some(v)) => {
                let bad =
                    || format!("bad glibc version `{v}` in target `{s}` (expected e.g. 2.35)");
                let (maj, min) = v.split_once('.').ok_or_else(bad)?;
                let ver = (
                    maj.parse::<u16>().map_err(|_| bad())?,
                    min.parse::<u16>().map_err(|_| bad())?,
                );
                if ver < DEFAULT_GLIBC {
                    return Err(format!(
                        "glibc {v} is older than the oldest supported, {}.{} (spec §15.3)",
                        DEFAULT_GLIBC.0, DEFAULT_GLIBC.1
                    ));
                }
                Some(ver)
            }
            (_, _, Some(_)) => {
                return Err(format!(
                    "target `{s}`: only linux-gnu targets take a glibc version"
                ));
            }
            (_, _, None) => None,
        };
        Ok(Target {
            arch,
            os,
            env,
            glibc,
        })
    }

    /// The LLVM triple, without the glibc suffix.
    /// The triple objects are compiled for: [`Target::llvm_triple`], except that WASI programs
    /// are preview 1 code (wasi-libc's) made into a preview 2 component when linked.
    pub fn clang_triple(self) -> &'static str {
        if matches!(self.os, Os::Wasi | Os::Browser) {
            // (The browser's runtime runs on wasi-libc too; its few WASI calls are the glue's.)
            "wasm32-wasip1"
        } else {
            self.llvm_triple()
        }
    }

    pub fn llvm_triple(self) -> &'static str {
        TARGETS
            .iter()
            .map(|(t, _)| *t)
            .find(|t| {
                let probe = Target::parse(t).expect("table entry");
                (probe.arch, probe.os, probe.env) == (self.arch, self.os, self.env)
            })
            .expect("every target has a triple")
    }

    /// The Debian multiarch name (`x86_64-linux-gnu`), used to find a system glibc.
    pub fn multiarch(self) -> String {
        let arch = match self.arch {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
            Arch::Wasm32 => "wasm32",
        };
        let env = match self.env {
            Env::Musl => "musl",
            _ => "gnu",
        };
        format!("{arch}-linux-{env}")
    }

    pub fn is_host(self) -> bool {
        let h = Target::host();
        (self.arch, self.os, self.env) == (h.arch, h.os, h.env)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.llvm_triple())?;
        match self.glibc {
            Some(v) if v != DEFAULT_GLIBC => write!(f, ".{}.{}", v.0, v.1),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_and_triples() {
        let a = Target::parse("linux-x64").unwrap();
        assert_eq!(a, Target::parse("x86_64-unknown-linux-gnu").unwrap());
        assert_eq!(a.llvm_triple(), "x86_64-unknown-linux-gnu");
        assert_eq!(a.glibc, Some((2, 31)));
        assert_eq!(a.multiarch(), "x86_64-linux-gnu");
        let w = Target::parse("windows-x64").unwrap();
        assert_eq!((w.os, w.env, w.glibc), (Os::Windows, Env::Gnu, None));
        assert_eq!(w.llvm_triple(), "x86_64-w64-windows-gnu");
        assert_eq!(Target::parse("wasm32-wasi").unwrap().os, Os::Wasi);
        assert_eq!(
            Target::parse("linux-arm64-musl").unwrap().to_string(),
            "aarch64-unknown-linux-musl"
        );
        assert!(Target::host().is_host());
    }

    #[test]
    fn glibc_suffix() {
        let t = Target::parse("x86_64-unknown-linux-gnu.2.35").unwrap();
        assert_eq!(t.glibc, Some((2, 35)));
        assert_eq!(t.to_string(), "x86_64-unknown-linux-gnu.2.35");
        assert_eq!(
            Target::parse("linux-arm64.2.39").unwrap().glibc,
            Some((2, 39))
        );
        assert!(
            Target::parse("linux-x64.2.17")
                .unwrap_err()
                .contains("older")
        );
        assert!(
            Target::parse("linux-x64.2")
                .unwrap_err()
                .contains("bad glibc")
        );
        assert!(
            Target::parse("linux-x64-musl.2.35")
                .unwrap_err()
                .contains("only linux-gnu")
        );
        assert!(
            Target::parse("sparc-sun")
                .unwrap_err()
                .contains("unknown target")
        );
    }
}
