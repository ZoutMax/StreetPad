#!/bin/bash
# StreetPad checks: unit tests (run on Windows), release build, and with --live a self-test
# inside the real Google Maps page. Run from WSL: tools/check.sh [--live]
set -euo pipefail
cd "$(dirname "$0")/.."
[ -f ~/.config/wsl-env.sh ] && . ~/.config/wsl-env.sh
PS=/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe

echo "== unit tests"
TEST_EXE=$(cargo xwin test --release --no-run 2>&1 | sed -n 's/.*Executable unittests src\/main.rs (\(.*\))/\1/p')
"$TEST_EXE" | tr -d '\r' | grep -E "^test result|FAILED|panicked" 
"$TEST_EXE" >/dev/null 2>&1 || { echo "UNIT TESTS FAILED"; exit 1; }

echo "== release build"
cargo xwin build --release 2>&1 | grep -E "^(error|warning)|Finished"
EXE=target/x86_64-pc-windows-msvc/release/StreetPad.exe

[ "${1:-}" = "--live" ] || { echo "OK (use --live for the Google page self-test)"; exit 0; }

echo "== live self-test in Google Maps"
WIN_TMP=$(cd /mnt/c && $PS -NoProfile -Command '$env:TEMP' | tr -d '\r')
TMP=$(wslpath "$WIN_TMP")
cp tools/selftest.js "$TMP/streetpad-selftest.js"
cp "$EXE" "$TMP/StreetPad-selftest.exe"
rm -f "$TMP/streetpad-debug.txt"
(cd /mnt/c && $PS -NoProfile -Command "
  Get-Process StreetPad -EA SilentlyContinue | % { \$_.CloseMainWindow() | Out-Null }; Start-Sleep 3
  Get-Process StreetPad -EA SilentlyContinue | Stop-Process -Force
  \$p = Start-Process '$WIN_TMP\\StreetPad-selftest.exe' -PassThru -ArgumentList '--url','https://www.google.com/maps/@52.3731,4.8926,16z','--debug-js','$WIN_TMP\\streetpad-selftest.js'
  for (\$i = 0; \$i -lt 30 -and -not (Test-Path '$WIN_TMP\\streetpad-debug.txt'); \$i++) { Start-Sleep 1 }
  Start-Sleep 1; \$p | Stop-Process -Force" | tr -d '\r')
RESULT=$(cat "$TMP/streetpad-debug.txt" 2>/dev/null || echo '{}')
rm -f "$TMP/streetpad-debug.txt" "$TMP/streetpad-selftest.js"
sleep 2; rm -f "$TMP/StreetPad-selftest.exe" || true
python3 -I -c '
import json, sys
r = json.loads(sys.argv[1]).get("result", {}).get("value")
if not r: print("LIVE SELF-TEST: no result (page did not load?)"); sys.exit(1)
for k, v in r["checks"].items(): print(("  ok   " if v else "  FAIL ") + k)
print("LIVE SELF-TEST:", "PASS" if r["pass"] else "FAIL")
sys.exit(0 if r["pass"] else 1)' "$RESULT"
