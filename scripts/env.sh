# Source this file to put the pinned toolchain on PATH: `source scripts/env.sh`
# macOS: Homebrew installs rustup and LLVM keg-only, so neither is on PATH by default.
# Linux: LLVM comes from apt.llvm.org (/usr/lib/llvm-23), rustup from rustup.rs (~/.cargo/bin).
# Values that are already set in the environment are kept.

if command -v brew >/dev/null 2>&1; then
  _ppl_brew="$(brew --prefix)"
  : "${PPL_RUSTUP_BIN:=$_ppl_brew/opt/rustup/bin}"
  : "${LLVM_SYS_231_PREFIX:=$_ppl_brew/opt/llvm@23}"
  unset _ppl_brew
elif [ -d /usr/lib/llvm-23 ]; then
  : "${LLVM_SYS_231_PREFIX:=/usr/lib/llvm-23}"
fi
if [ -z "${PPL_RUSTUP_BIN:-}" ] && [ -d "$HOME/.cargo/bin" ]; then
  PPL_RUSTUP_BIN="$HOME/.cargo/bin"
fi

if [ -n "${PPL_RUSTUP_BIN:-}" ] && [ -d "$PPL_RUSTUP_BIN" ]; then
  case ":$PATH:" in *":$PPL_RUSTUP_BIN:"*) ;; *) PATH="$PPL_RUSTUP_BIN:$PATH" ;; esac
fi
if [ -n "${LLVM_SYS_231_PREFIX:-}" ] && [ -d "$LLVM_SYS_231_PREFIX/bin" ]; then
  case ":$PATH:" in *":$LLVM_SYS_231_PREFIX/bin:"*) ;; *) PATH="$LLVM_SYS_231_PREFIX/bin:$PATH" ;; esac
fi
export PATH LLVM_SYS_231_PREFIX
