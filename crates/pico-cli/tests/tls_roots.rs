//! Where TLS clients get their trusted roots (#137), as `PICO_TLS_DEBUG=1` reports it: on
//! macOS the bundle and the Keychain (its anchors and the admin and user trust settings), so a
//! root that is only in the Keychain is trusted too. No network: the peer is a local listener
//! that hangs up, so the handshake fails after the roots are loaded.

use std::process::Command;

const PROGRAM: &str = r#"import { listen } from "std/net"
const l = await listen(0)
l.accept().then((c: Conn?) => { if (c != null) c.close() })
try {
  await fetch(`https://127.0.0.1:${l.addr.port}/`)
  println("connected?")
} catch (e) {
  println("the handshake failed, as expected")
}
l.close()
"#;

fn roots_line(interp: bool) -> String {
    let dir = std::env::temp_dir().join(format!("pico-tls-roots-{}-{interp}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("roots.pi");
    std::fs::write(&file, PROGRAM).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pico"));
    cmd.arg("run");
    if interp {
        cmd.arg("--interp");
    }
    let out = cmd
        .arg(&file)
        .current_dir(&dir)
        .env("PICO_TLS_DEBUG", "1")
        .env_remove("SSL_CERT_FILE")
        .env_remove("SSL_CERT_DIR")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("the handshake failed, as expected"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    stderr
        .lines()
        .find(|l| l.starts_with("[tls] roots:"))
        .unwrap_or_else(|| panic!("no `[tls] roots:` line in:\n{stderr}"))
        .to_string()
}

#[test]
fn reports_where_the_roots_come_from() {
    for interp in [true, false] {
        let line = roots_line(interp);
        if cfg!(target_os = "macos") {
            // `[tls] roots: /etc/ssl/cert.pem, 159 from the Keychain`
            let n: usize = line
                .rsplit(", ")
                .next()
                .and_then(|t| t.split(' ').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("unexpected line: {line}"));
            assert!(n > 0, "no roots from the Keychain: {line}");
        }
    }
}
