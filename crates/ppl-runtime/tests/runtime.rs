//! Links small C programs against the runtime and checks their behavior, including that the
//! C number formatting matches the interpreter's `format_float` exactly.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Compiles `src` (a C file defining `ppl_entry`) with the runtime and runs it.
fn run_c(name: &str, src: &str, stdin: &[u8]) -> Output {
    let dir = scratch(name);
    let c = dir.join("main.c");
    let exe = dir.join("main");
    std::fs::write(&c, src).unwrap();
    let status = Command::new("cc")
        .arg(&c)
        .arg(ppl_runtime::static_lib())
        .arg("-I")
        .arg(ppl_runtime::include_dir())
        .args(ppl_runtime::host_link_flags())
        .arg("-o")
        .arg(&exe)
        .status()
        .expect("run cc");
    assert!(status.success(), "compiling {name} failed");
    let mut child = Command::new(&exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn printing_and_panic_format() {
    let out = run_c(
        "panic",
        r#"
#include "ppl_rt.h"
void ppl_entry(void) {
    ppl_print_i64(-42); ppl_print_newline();
    ppl_print_u64(18446744073709551615ull); ppl_print_newline();
    ppl_print_bool(true); ppl_print_newline();
    ppl_print_str("zażółć", 10); ppl_print_char(0x1F642); ppl_print_newline();
    ppl_print_f64(0.1 + 0.2); ppl_print_newline();
    ppl_print_f32(0.1f); ppl_print_newline();
    ppl_panic("integer overflow", 16, "src/main.ppl", 12, 3, 9);
}
"#,
        b"",
    );
    assert_eq!(out.status.code(), Some(101));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "-42\n18446744073709551615\ntrue\nzażółć🙂\n0.30000000000000004\n0.1\n"
    );
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "panic: integer overflow\n  at src/main.ppl:3:9\n"
    );
}

/// xorshift64*, so the test needs no dependencies and is reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

#[test]
fn float_formatting_matches_interpreter() {
    let mut values: Vec<f64> = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.1,
        0.5,
        1.5,
        100.0,
        1e21,
        1e20,
        123456789012345680000.0,
        1e-6,
        1e-7,
        1.5e-7,
        5e-324,
        f64::MAX,
        f64::MIN_POSITIVE,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.1 + 0.2,
        2.0f64.powi(53),
        9007199254740993.0,
        4.35,
        0.000123,
        1234.5678,
    ];
    let mut rng = Rng(0x9E3779B97F4A7C15);
    for _ in 0..3000 {
        let bits = rng.next();
        let v = f64::from_bits(bits);
        if v.is_finite() {
            values.push(v);
        }
        // "Nice" decimals with few digits, the common case in programs.
        values.push((rng.next() % 2_000_000) as f64 / 1000.0 - 1000.0);
    }
    let input: String = values
        .iter()
        .map(|v| format!("{:016x}\n", v.to_bits()))
        .collect();
    let out = run_c(
        "floats",
        r#"
#include "ppl_rt.h"
#include <stdio.h>
#include <string.h>
#include <inttypes.h>
void ppl_entry(void) {
    uint64_t bits;
    char buf[40];
    while (scanf("%" SCNx64, &bits) == 1) {
        double d;
        memcpy(&d, &bits, 8);
        ppl_format_f64(d, buf);
        printf("%s ", buf);
        ppl_format_f32((float)d, buf);
        printf("%s\n", buf);
    }
}
"#,
        input.as_bytes(),
    );
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let mut mismatches = Vec::new();
    for (v, line) in values.iter().zip(text.lines()) {
        let (c64, c32) = line.split_once(' ').unwrap();
        let r64 = ppl_interp::format_float(*v, false);
        let r32 = ppl_interp::format_float(*v as f32 as f64, true);
        if c64 != r64 || c32 != r32 {
            mismatches.push(format!("{v:e}: C {c64} / {c32}, Rust {r64} / {r32}"));
        }
    }
    assert_eq!(text.lines().count(), values.len());
    assert!(
        mismatches.is_empty(),
        "{} mismatches:\n{}",
        mismatches.len(),
        mismatches[..mismatches.len().min(20)].join("\n")
    );
}
