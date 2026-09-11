#!/usr/bin/env bash
# 生成本地的 .cargo/config.toml（含本机 NDK 绝对路径，因此不入库）
#
# 用法（在仓库根目录执行）：
#   ./scripts/setup-cargo-config.sh
#   NDK_ROOT=/path/to/ndk ./scripts/setup-cargo-config.sh
#   ./scripts/setup-cargo-config.sh --force
#
# 探测顺序：
#   1. NDK_ROOT 环境变量
#   2. ANDROID_NDK_HOME
#   3. $ANDROID_HOME/ndk/<最新版本> 或 $ANDROID_SDK_ROOT/ndk/<最新版本>
#   4. ~/Android/Sdk/ndk/<最新版本>、/opt/android-ndk*、/usr/local/android-ndk*

set -euo pipefail

FORCE=0
for arg in "$@"; do
  case "$arg" in
    --force|-f) FORCE=1 ;;
    -h|--help)  sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "未知参数: $arg"; exit 1 ;;
  esac
done

pick_latest() {  # 在给定目录里挑名字版本号最大的子目录
  [ -d "$1" ] || return 1
  ls -1 "$1" 2>/dev/null | sort -V | tail -n 1
}

resolve_ndk_root() {
  for v in "${NDK_ROOT:-}" "${ANDROID_NDK_HOME:-}"; do
    [ -n "$v" ] && [ -d "$v" ] && { echo "$v"; return; }
  done
  for sdk in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" "$HOME/Android/Sdk"; do
    [ -n "$sdk" ] || continue
    local n
    n="$(pick_latest "$sdk/ndk" || true)"
    [ -n "$n" ] && { echo "$sdk/ndk/$n"; return; }
  done
  for d in /opt /usr/local; do
    local n
    n="$(ls -1d "$d"/android-ndk* 2>/dev/null | sort -V | tail -n 1 || true)"
    [ -n "$n" ] && { echo "$n"; return; }
  done
  return 1
}

case "$(uname -s)" in
  Darwin) HOST_TAG="darwin-x86_64" ;;
  Linux)  HOST_TAG="linux-x86_64" ;;
  *)      echo "不支持的系统: $(uname -s)（Windows 请用 setup-cargo-config.ps1）"; exit 1 ;;
esac

ROOT="$(resolve_ndk_root || true)"
if [ -z "$ROOT" ]; then
  echo "未找到 Android NDK。请任选其一：" >&2
  echo "  1) 设置 ANDROID_NDK_HOME（或 NDK_ROOT）指向 NDK 根目录" >&2
  echo "  2) NDK_ROOT=/path/to/ndk $0" >&2
  exit 1
fi

BIN_DIR="$ROOT/toolchains/llvm/prebuilt/$HOST_TAG/bin"
SYSROOT="$ROOT/toolchains/llvm/prebuilt/$HOST_TAG/sysroot"
[ -d "$BIN_DIR" ] || { echo "NDK 目录结构异常，未找到: $BIN_DIR" >&2; exit 1; }
[ -d "$SYSROOT" ] || { echo "未找到 sysroot: $SYSROOT" >&2; exit 1; }
[ -x "$BIN_DIR/clang" ]    || { echo "未找到 clang: $BIN_DIR/clang" >&2; exit 1; }
[ -x "$BIN_DIR/llvm-ar" ]  || { echo "未找到 llvm-ar: $BIN_DIR/llvm-ar" >&2; exit 1; }

# 用 realpath 拿到干净的绝对路径
CLANG="$(cd "$BIN_DIR" && pwd)/clang"
AR="$(cd "$BIN_DIR" && pwd)/llvm-ar"
SYSROOT_ABS="$(cd "$SYSROOT" && pwd)"

OUT="$(cd "$(dirname "$0")/.." && pwd)/.cargo/config.toml"
if [ -f "$OUT" ] && [ "$FORCE" -eq 0 ]; then
  echo "已存在 .cargo/config.toml，未覆盖（要重新生成请加 --force）"
  grep -m1 '^# NDK:' "$OUT" || true
  exit 0
fi

mkdir -p "$(dirname "$OUT")"
{
  echo "# 由 scripts/setup-cargo-config.sh 自动生成，请勿手工编辑（本机路径，不入库）"
  echo "# NDK: $ROOT"
  echo "# ABI 最低版本 21 = Android 5.0"
  echo
  for t in "aarch64-linux-android:aarch64-linux-android21" \
           "armv7-linux-androideabi:armv7-linux-androideabi21" \
           "x86_64-linux-android:x86_64-linux-android21"; do
    triple="${t%%:*}"; abi="${t##*:}"
    echo "[target.$triple]"
    echo "ar = \"$AR\""
    echo "linker = \"$CLANG\""
    echo "rustflags = ["
    echo "  \"-C\", \"link-arg=--target=$abi\","
    echo "  \"-C\", \"link-arg=--sysroot=$SYSROOT_ABS\","
    echo "  \"-C\", \"link-arg=-fuse-ld=lld\","
    echo "]"
    echo
  done
} > "$OUT"

echo "已生成 .cargo/config.toml"
echo "  NDK    : $ROOT"
echo "  host   : $HOST_TAG"
echo "  现在可以执行: cargo build --target aarch64-linux-android --release"
