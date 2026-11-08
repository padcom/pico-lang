#!/usr/bin/env bash
# Homebrew's llvm@23 and its runtime dependencies on Intel Macs, cached as built kegs (#127):
# Homebrew has no bottles for this platform, so each of them would be compiled from source.
#
#   llvm.sh plan    the cache key and paths, from the formulae Homebrew would install
#   llvm.sh link    after a restore: brew link each keg, then verify; nothing is built
#   llvm.sh build   brew install llvm@23 (from source), with a heartbeat every 5 minutes
#   llvm.sh built   after a build: the key and paths from the kegs actually installed
#
# The key covers llvm@23 and every runtime dependency, by version and revision, so any bump is a
# miss (a rebuild) rather than a restored mix. Outputs go to $GITHUB_OUTPUT.
set -euo pipefail
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1

out() { echo "$1=$2" >> "${GITHUB_OUTPUT:-/dev/null}"; echo "$1=$2"; }

# llvm@23's runtime dependencies (recursive; not build or test ones), dependencies before what
# needs them, then llvm@23: one per line.
formulae() {
  brew deps --topological llvm@23
  echo llvm@23
}

# The "name version" list plan made, for link (the runner's temporary directory).
KEGS="${RUNNER_TEMP:-/tmp}/llvm-kegs.txt"

# "name version[_revision]" per formula, from the index (plan) or from the kegs installed (built).
versions_from_index() {
  # shellcheck disable=SC2046
  brew info --json=v2 $(formulae) |
    jq -r '.formulae[] | "\(.name) \(.versions.stable)\(if .revision > 0 then "_\(.revision)" else "" end)"'
}
versions_installed() {
  local f keg
  for f in $(formulae); do
    keg=$(cd "$(brew --prefix "$f")" 2> /dev/null && pwd -P) || { echo "no installed keg for $f" >&2; return 1; }
    echo "$f $(basename "$keg")"
  done
}

# The key and the keg paths for a "name version" list on stdin.
emit() {
  local list cellar macos hash llvm paths=()
  list=$(sort)
  cellar=$(brew --cellar)
  macos=$(sw_vers -productVersion | cut -d. -f1)
  hash=$(printf '%s\n' "$list" | shasum -a 256 | cut -c1-12)
  llvm=$(awk '$1 == "llvm@23" { print $2 }' <<< "$list")
  while read -r name version; do paths+=("$cellar/$name/$version"); done <<< "$list"
  echo "$list" | sed 's/^/  /'
  printf '%s\n' "$list" > "$KEGS"
  out key "llvm@23-macos$macos-x64-$llvm-$hash"
  { echo "paths<<EOF"; printf '%s\n' "${paths[@]}"; echo "EOF"; } >> "${GITHUB_OUTPUT:-/dev/null}"
}

# Whether the restored kegs make a working LLVM: llvm-config answers, and libLLVM's and a tool's
# Homebrew libraries all resolve.
# (Each check returns explicitly: called as `verify || …`, set -e doesn't apply inside it.)
verify() {
  local prefix libdir lib missing=0 path
  prefix=$(brew --prefix llvm@23) || return 1
  "$prefix/bin/llvm-config" --version || return 1
  "$prefix/bin/llvm-config" --system-libs || return 1
  libdir=$("$prefix/bin/llvm-config" --libdir) || return 1
  lib=$(ls "$libdir"/libLLVM*.dylib 2> /dev/null | head -1 || true)
  [ -n "$lib" ] || { echo "no libLLVM in $libdir" >&2; missing=1; }
  for bin in ${lib:+"$lib"} "$prefix/bin/clang"; do
    [ -e "$bin" ] || { echo "missing $bin" >&2; missing=1; continue; }
    while read -r path; do
      [ -e "$path" ] || { echo "$bin: missing $path" >&2; missing=1; }
    done < <(otool -L "$bin" | awk 'NR > 1 { print $1 }' | grep "^$(brew --prefix)/" || true)
  done
  [ -z "$(brew missing llvm@23 2>&1)" ] || { brew missing llvm@23 >&2; missing=1; }
  return "$missing"
}

case "${1:-}" in
  plan)
    # (An image's Homebrew index can predate llvm@23, as the Intel one's did.)
    brew info llvm@23 > /dev/null 2>&1 || HOMEBREW_NO_AUTO_UPDATE= brew update
    versions_from_index | emit
    ;;
  link)
    # Dependencies first, so llvm@23's links find theirs; each at the version the key was made
    # from. Keg-only ones get only their opt link (brew link refuses them without --force, and
    # they mustn't shadow the system's).
    for f in $(formulae); do
      version=$(awk -v f="$f" '$1 == f { print $2 }' "$KEGS")
      keg="$(brew --cellar)/$f/$version"
      [ -n "$version" ] && [ -f "$keg/INSTALL_RECEIPT.json" ] \
        || { echo "::warning::the cache has no keg for $f $version: building instead"; exit 1; }
      if [ "$(brew info --json=v2 "$f" | jq -r '.formulae[0].keg_only')" = true ]; then
        brew link "$f" 2>&1 | sed 's/^/  /' || true
        # (Where brew link makes no opt link for a keg-only formula, the one it would make.)
        [ -e "$(brew --prefix)/opt/$f" ] || ln -sfn "$keg" "$(brew --prefix)/opt/$f"
      else
        brew link --overwrite "$f"
      fi
      [ "$(cd "$(brew --prefix)/opt/$f" && pwd -P)" = "$keg" ] \
        || { echo "::warning::$f's opt link isn't the cached $version: building instead"; exit 1; }
    done
    verify || { echo "::warning::the restored llvm@23 doesn't work: building instead"; exit 1; }
    ;;
  build)
    # A heartbeat: a silent step of hours looks stalled (and got runs cancelled).
    ( start=$SECONDS; while sleep 300; do echo "still building llvm@23 and its dependencies ($(( (SECONDS - start) / 60 )) min)"; done ) &
    beat=$!
    trap 'kill "$beat" 2> /dev/null' EXIT
    brew install llvm@23 || { HOMEBREW_NO_AUTO_UPDATE= brew update && brew install llvm@23; }
    ;;
  built)
    verify
    versions_installed | emit
    du -sh $(versions_installed | while read -r n v; do echo "$(brew --cellar)/$n/$v"; done) | sort -h | tail -5
    du -csh $(versions_installed | while read -r n v; do echo "$(brew --cellar)/$n/$v"; done) | tail -1
    ;;
  *)
    echo "usage: $0 plan|link|build|built" >&2
    exit 2
    ;;
esac
