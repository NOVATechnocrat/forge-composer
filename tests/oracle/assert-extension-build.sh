#!/usr/bin/env bash
# Extension build oracle — the M0 extension must typecheck and bundle.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO/extension"

npm ci --no-fund --no-audit >/dev/null 2>&1 || { echo "ASSERT FAIL: npm ci" >&2; exit 1; }
npm run typecheck >/dev/null || { echo "ASSERT FAIL: typecheck" >&2; exit 1; }
npm run build >/dev/null || { echo "ASSERT FAIL: esbuild" >&2; exit 1; }
[ -s dist/extension.js ] || { echo "ASSERT FAIL: dist/extension.js missing/empty" >&2; exit 1; }

# The M0 webview must actually wire the daemon client (not the scaffold toast).
grep -q "registerWebviewViewProvider" dist/extension.js || {
  echo "ASSERT FAIL: webview provider not registered in bundle" >&2; exit 1; }

echo "EXTENSION-BUILD-OK"
