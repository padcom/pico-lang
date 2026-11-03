//! `ppl symbolize trace.txt` (spec §7.1, PLAN P10.8d): turns a release build's panic trace,
//! whose frames are offsets into the executable (`app+0x1a560`) under a build ID, into ppl
//! functions, files and lines, from the separate debug file the release build wrote next to the
//! executable (`app.dbg`; `app.dSYM` on macOS). Lines that aren't frames pass through; frames
//! outside ppl code (the runtime, libc) are left out, as debug builds' traces leave them out.

use object::{Object, ObjectSegment};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub fn run(trace: &Path, debug: Option<&Path>) -> ExitCode {
    let text = if trace == Path::new("-") {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s);
        s
    } else {
        match std::fs::read_to_string(trace) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: cannot read {}: {e}", trace.display());
                return ExitCode::from(2);
            }
        }
    };
    match symbolize(&text, trace, debug) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A frame line: `  3: app+0x1a560`.
fn frame(line: &str) -> Option<(&str, u64)> {
    let (_, rest) = line.trim_start().split_once(": ")?;
    let (module, off) = rest.trim().rsplit_once("+0x")?;
    Some((module, u64::from_str_radix(off, 16).ok()?))
}

/// The build ID of a release trace's header: `stack trace (build 4c16…; …):`.
fn trace_build_id(text: &str) -> Option<&str> {
    text.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("stack trace (build ")?;
        Some(rest.split([';', ')']).next()?.trim())
    })
}

fn symbolize(text: &str, trace: &Path, debug: Option<&Path>) -> Result<String, String> {
    let module = text.lines().find_map(frame).map(|(m, _)| m.to_string());
    let Some(module) = module else {
        // (Nothing to resolve: a debug build's trace names its frames already.)
        return Ok(text.to_string());
    };
    let file = match debug {
        Some(d) => debug_path(d),
        None => find_debug_file(&module, trace)?,
    };
    let data = std::fs::read(&file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let obj = object::File::parse(&*data)
        .map_err(|e| format!("{} isn't a debug file: {e}", file.display()))?;
    // The trace must come from the build the debug file belongs to.
    if let Some(want) = trace_build_id(text) {
        let have = build_id(&obj);
        if have.as_deref().is_some_and(|h| h != want) {
            return Err(format!(
                "{} belongs to another build ({}, the trace is {want})",
                file.display(),
                have.unwrap_or_default()
            ));
        }
    }
    // Offsets are from where the executable is loaded: its first segment (`__TEXT` on macOS,
    // address 0 for position-independent ELF).
    let base = obj
        .segments()
        .find(|s| s.name().ok().flatten() == Some("__TEXT"))
        .map_or(0, |s| s.address());
    let loader = addr2line::Loader::new(&file)
        .map_err(|e| format!("cannot read the debug info of {}: {e}", file.display()))?;
    let mut out = String::new();
    let mut k = 0;
    for line in text.lines() {
        let Some((m, off)) = frame(line) else {
            // (The header as a debug build's: the build ID was for finding the debug file.)
            if line.trim_start().starts_with("stack trace (build ") {
                out.push_str("stack trace:\n");
            } else {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        };
        if m != module {
            continue;
        }
        // (A return address: the call is the byte before it.)
        let probe = base + off.saturating_sub(1);
        let mut shown = Vec::new();
        if let Ok(mut frames) = loader.find_frames(probe) {
            while let Ok(Some(f)) = frames.next() {
                let name = f
                    .function
                    .as_ref()
                    .and_then(|n| n.raw_name().ok().map(|s| s.to_string()));
                let loc = f.location.as_ref().and_then(|l| {
                    let file = l.file?;
                    Some((file.to_string(), l.line.unwrap_or(0), l.column.unwrap_or(0)))
                });
                // Only ppl code (the runtime's and libc's frames are left out).
                if let (Some(name), Some((file, line, col))) = (name, loc)
                    && file.ends_with(".ppl")
                {
                    shown.push(format!(
                        "{} at {}:{line}:{col}",
                        ppl_name(&name),
                        display_path(&file)
                    ));
                }
            }
        }
        // (Inlined calls are frames too, innermost first, as a debug build shows them.)
        for s in &shown {
            out.push_str(&format!("  {k}: {s}\n"));
            k += 1;
        }
    }
    Ok(out)
}

/// A function's ppl name from its symbol: `ppl.inner` is `inner`, `ppl_entry` the top level.
fn ppl_name(symbol: &str) -> String {
    match symbol {
        "ppl_entry" => "<main>".into(),
        s => s.strip_prefix("ppl.").unwrap_or(s).to_string(),
    }
}

fn build_id(obj: &object::File) -> Option<String> {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    if let Ok(Some(id)) = obj.build_id() {
        return Some(hex(id));
    }
    obj.mach_uuid().ok().flatten().map(|u| hex(&u))
}

/// A debug file, or the DWARF inside a `.dSYM` bundle.
fn debug_path(p: &Path) -> PathBuf {
    if p.extension().is_some_and(|e| e == "dSYM") {
        let name = p.file_stem().unwrap_or_default();
        return p.join("Contents/Resources/DWARF").join(name);
    }
    p.to_path_buf()
}

/// The debug file of `module`, next to the trace or in the current directory: `module.dbg`,
/// `module.dSYM`.
fn find_debug_file(module: &str, trace: &Path) -> Result<PathBuf, String> {
    let mut dirs = vec![PathBuf::from(".")];
    if let Some(d) = trace.parent().filter(|d| !d.as_os_str().is_empty()) {
        dirs.insert(0, d.to_path_buf());
    }
    for d in &dirs {
        for c in [
            d.join(format!("{module}.dbg")),
            debug_path(&d.join(format!("{module}.dSYM"))),
        ] {
            if c.is_file() {
                return Ok(c);
            }
        }
    }
    Err(format!(
        "no debug file for `{module}` (looked for {module}.dbg and {module}.dSYM next to the trace and here; `--debug` names one)"
    ))
}

/// A source path, relative to the current directory when it's inside it.
fn display_path(p: &str) -> String {
    let path = Path::new(p);
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(|r| r.display().to_string()))
        .unwrap_or_else(|| p.to_string())
}
