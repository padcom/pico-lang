#!/usr/bin/env bash
# Packages editors/vscode as a .vsix (its npm dependencies installed first, from the lockfile)
# and, with --install, installs it with VS Code's CLI (`code`, or $CODE).
# Usage: scripts/vscode-package.sh [--install]   → target/vscode/pico-<version>.vsix
set -euo pipefail
cd "$(dirname "$0")/.."
out=target/vscode
mkdir -p "$out"
(cd editors/vscode && npm ci --omit=dev --no-audit --no-fund --silent)
vsix=$(python3 - "$out" <<'PY'
import json, sys, zipfile, pathlib
src = pathlib.Path("editors/vscode")
pkg = json.loads((src / "package.json").read_text())
name, ver, pub = pkg["name"], pkg["version"], pkg["publisher"]
dest = pathlib.Path(sys.argv[1]) / f"{name}-{ver}.vsix"
icon = pkg.get("icon")
icon_meta = f"\n    <Icon>extension/{icon}</Icon>" if icon else ""
icon_asset = (f'\n    <Asset Type="Microsoft.VisualStudio.Services.Icons.Default" Path="extension/{icon}" Addressable="true" />'
              if icon else "")
manifest = f"""<?xml version="1.0" encoding="utf-8"?>
<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011">
  <Metadata>
    <Identity Language="en-US" Id="{name}" Version="{ver}" Publisher="{pub}" />
    <DisplayName>{pkg.get("displayName", name)}</DisplayName>
    <Description xml:space="preserve">{pkg.get("description", "")}</Description>
    <Categories>Programming Languages</Categories>{icon_meta}
    <Properties>
      <Property Id="Microsoft.VisualStudio.Code.Engine" Value="{pkg["engines"]["vscode"]}" />
    </Properties>
  </Metadata>
  <Installation><InstallationTarget Id="Microsoft.VisualStudio.Code" /></Installation>
  <Dependencies />
  <Assets>
    <Asset Type="Microsoft.VisualStudio.Code.Manifest" Path="extension/package.json" Addressable="true" />{icon_asset}
  </Assets>
</PackageManifest>
"""
files = [f for f in sorted(src.rglob("*")) if f.is_file() and f.suffix != ".py"]
known = {".json": "application/json", ".md": "text/markdown", ".js": "application/javascript",
         ".vsixmanifest": "text/xml", ".png": "image/png", ".svg": "image/svg+xml"}
exts = sorted({f.suffix.lower() for f in files if f.suffix} | {".vsixmanifest"})
types = '<?xml version="1.0" encoding="utf-8"?>\n<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">\n'
for e in exts:
    types += f'  <Default Extension="{e}" ContentType="{known.get(e, "application/octet-stream")}" />\n'
types += "</Types>\n"
with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("extension.vsixmanifest", manifest)
    z.writestr("[Content_Types].xml", types)
    for f in files:
        z.write(f, "extension/" + f.relative_to(src).as_posix())
print(dest)
PY
)
echo "$vsix"
if [ "${1:-}" = "--install" ]; then
  "${CODE:-code}" --install-extension "$vsix" --force
fi
