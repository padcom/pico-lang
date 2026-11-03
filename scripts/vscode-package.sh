#!/usr/bin/env bash
# Packages editors/vscode as a .vsix (no npm needed while the extension has no code) and,
# with --install, installs it with VS Code's CLI (`code`, or $CODE).
# Usage: scripts/vscode-package.sh [--install]   → target/vscode/ppl-<version>.vsix
set -euo pipefail
cd "$(dirname "$0")/.."
out=target/vscode
mkdir -p "$out"
vsix=$(python3 - "$out" <<'PY'
import json, sys, zipfile, pathlib
src = pathlib.Path("editors/vscode")
pkg = json.loads((src / "package.json").read_text())
name, ver, pub = pkg["name"], pkg["version"], pkg["publisher"]
dest = pathlib.Path(sys.argv[1]) / f"{name}-{ver}.vsix"
manifest = f"""<?xml version="1.0" encoding="utf-8"?>
<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011">
  <Metadata>
    <Identity Language="en-US" Id="{name}" Version="{ver}" Publisher="{pub}" />
    <DisplayName>{pkg.get("displayName", name)}</DisplayName>
    <Description xml:space="preserve">{pkg.get("description", "")}</Description>
    <Categories>Programming Languages</Categories>
    <Properties>
      <Property Id="Microsoft.VisualStudio.Code.Engine" Value="{pkg["engines"]["vscode"]}" />
    </Properties>
  </Metadata>
  <Installation><InstallationTarget Id="Microsoft.VisualStudio.Code" /></Installation>
  <Dependencies />
  <Assets>
    <Asset Type="Microsoft.VisualStudio.Code.Manifest" Path="extension/package.json" Addressable="true" />
  </Assets>
</PackageManifest>
"""
types = """<?xml version="1.0" encoding="utf-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension=".json" ContentType="application/json" />
  <Default Extension=".md" ContentType="text/markdown" />
  <Default Extension=".py" ContentType="text/plain" />
  <Default Extension=".vsixmanifest" ContentType="text/xml" />
</Types>
"""
with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("extension.vsixmanifest", manifest)
    z.writestr("[Content_Types].xml", types)
    for f in sorted(src.rglob("*")):
        if f.is_file() and f.suffix != ".py":
            z.write(f, "extension/" + f.relative_to(src).as_posix())
print(dest)
PY
)
echo "$vsix"
if [ "${1:-}" = "--install" ]; then
  "${CODE:-code}" --install-extension "$vsix" --force
fi
