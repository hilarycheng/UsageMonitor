#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")"

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) ;;
  *) printf '%s\n' '請使用 Windows Git Bash 或 MSYS2 執行：bash build.sh' >&2; exit 1 ;;
esac

if ! command -v cargo.exe >/dev/null 2>&1; then
  printf '%s\n' '找不到 cargo.exe；請先安裝 Rust Windows 工具鏈及 Visual Studio C++ Build Tools。' >&2
  exit 1
fi

cargo.exe build --release --locked --config net.offline=false
printf '%s\n' '完成：target/release/usage-monitor.exe' '執行後會在 Windows 系統匣顯示每週剩餘百分比。'
