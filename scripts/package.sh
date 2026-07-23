#!/usr/bin/env bash
# Package forge-composer for distribution: a release composerd binary + a VSIX
# into dist/. Run from the repo root: `bash scripts/package.sh`.
#
# Reviewed-not-proven: the vsce step needs network to fetch @vscode/vsce, so it
# is NOT run by the hermetic oracle — the Architect runs this script. The cargo
# release build half is deterministic and offline-safe.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

DIST="$REPO/dist"
mkdir -p "$DIST"

echo "== 1. release daemon =="
( cd "$REPO/daemon" && cargo build --release -p composerd )
cp "$REPO/daemon/target/release/composerd" "$DIST/composerd"

echo "== 2. build + package extension =="
( cd "$REPO/extension" && npm run build )
( cd "$REPO/extension" && npx --yes @vscode/vsce package --no-dependencies --out "$DIST/" )

echo "== 3. dist listing =="
ls -la "$DIST"

# Ensure dist/ is gitignored (idempotent).
GITIGNORE="$REPO/.gitignore"
if ! grep -qx 'dist/' "$GITIGNORE" 2>/dev/null; then
  printf '\n# Packaging output\ndist/\n' >> "$GITIGNORE"
  echo "== added dist/ to .gitignore =="
fi
