use std::process::Command;

#[test]
fn version_flag_prints_name_and_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_pico"))
        .arg("--version")
        .output()
        .expect("failed to run pico");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.trim(), format!("pico {}", env!("CARGO_PKG_VERSION")));
}
