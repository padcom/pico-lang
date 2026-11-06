# P10.10b1 Embedded Resources Implementation

This file documents the implementation of the P10.10b1 Embedded resources feature that was already complete in the codebase but needed to be properly surfaced in the website documentation.

## Summary of Implementation

The embedded resources feature was implemented according to decision 297 and includes:

1. **Feature Flag**: The `embed` feature is defined in both `pico-cli/Cargo.toml` and `pico-runtime/Cargo.toml`
2. **Build Process**: The `pico-runtime/build.rs` build script bundles resources when the `embed` feature is enabled
3. **Runtime Logic**: The `pico-runtime/src/lib.rs` handles unpacking bundled resources into the cache
4. **Resources Bundled**: 
   - `runtime/` - C runtime sources
   - `third_party/` - Vendored libraries (mbedtls, compiler-rt, etc.)
   - `stdlib/` - Standard library sources
   - Host runtime libraries (`lib/`) - Static libraries needed for compilation

## Files Modified

1. `website/index.html` - Added the 20th feature to the tour
2. `website/index.html` - Updated title from "nineteen features" to "twenty features"

## Key Features

- **Single Binary Distribution**: Release builds bundle all necessary resources
- **Cache Management**: Resources unpacked into `$TMPDIR/pico-cache/home/` on first use
- **Fallback Mechanism**: Falls back to installed resources if embedded ones aren't available
- **Size Efficient**: Resources are gzipped (~8 MB) during bundling

## Usage

To build a self-contained binary with embedded resources:
```bash
cargo build --features embed --release
```

The resulting binary will include all necessary runtime resources and can run without requiring external pico installation.