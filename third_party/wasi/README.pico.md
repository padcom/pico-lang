# WASI (bundled for pico)

What pico needs to make `wasm32-wasi` programs WASI preview 2 components (decision 257):

- `wasi_snapshot_preview1.command.wasm`: the preview 1 → preview 2 adapter from wasmtime v50.0.1's
  release (its GitHub build attestation checked with
  `gh attestation verify <file> --repo bytecodealliance/wasmtime`). Apache-2.0 WITH
  LLVM-exception, like wasmtime.
- `wit/deps/*`: the WASI 0.2.12 interfaces (WIT), unmodified from wasmtime v50.0.1
  (`crates/wasi/src/p2/wit/deps`). Apache-2.0 WITH LLVM-exception.
- `wit/pico.wit`: pico's own world, the interfaces the runtime imports directly.

The C bindings for that world are in `runtime/wasi`, regenerated with
`cargo run -p pico-wasi-bindgen`.
