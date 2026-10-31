# Source this file to put the pinned toolchain on PATH: `source scripts/env.sh`
# Homebrew installs rustup and LLVM keg-only, so neither is on PATH by default.
# Values that are already set in the environment are kept.

if command -v brew >/dev/null 2>&1; then
  _ppl_brew="$(brew --prefix)"
  : "${PPL_RUSTUP_BIN:=$_ppl_brew/opt/rustup/bin}"
  : "${LLVM_SYS_231_PREFIX:=$_ppl_brew/opt/llvm@23}"
  unset _ppl_brew
fi

if [ -n "${PPL_RUSTUP_BIN:-}" ] && [ -d "$PPL_RUSTUP_BIN" ]; then
  case ":$PATH:" in *":$PPL_RUSTUP_BIN:"*) ;; *) PATH="$PPL_RUSTUP_BIN:$PATH" ;; esac
fi
if [ -n "${LLVM_SYS_231_PREFIX:-}" ] && [ -d "$LLVM_SYS_231_PREFIX/bin" ]; then
  case ":$PATH:" in *":$LLVM_SYS_231_PREFIX/bin:"*) ;; *) PATH="$LLVM_SYS_231_PREFIX/bin:$PATH" ;; esac
fi
export PATH LLVM_SYS_231_PREFIX
