#!/usr/bin/env bash
#
# Cross-compiles the ERL Raster engine for Android and stages the shared object where this package
# looks for it, at `android/src/main/jniLibs/<abi>/liberl_raster.so`.
#
#   ./scripts/build-android.sh                 # arm64-v8a, the ABI every phone was measured on
#   ./scripts/build-android.sh arm64-v8a armeabi-v7a x86_64
#
# Required once, on the machine that builds:
#
#   rustup target add aarch64-linux-android
#   cargo install cargo-ndk
#   export ANDROID_NDK_HOME=...    # or ANDROID_NDK_ROOT, or an Android SDK that has one
#
# The crate directory is found by walking up from this package to the repository root. Set
# `ERL_RASTER_DIR` to point somewhere else, which is what a consumer with the engine checked out
# separately does.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
package="$(cd "$here/.." && pwd)"
crate_root="${ERL_RASTER_DIR:-$(cd "$package/../../.." && pwd)}"
staging="$package/android/src/main/jniLibs"

abis=("$@")
if [ ${#abis[@]} -eq 0 ]; then
  abis=(arm64-v8a)
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "erl-raster: no cargo on the path. Install Rust, then run this again." >&2
  exit 1
fi

if ! cargo ndk --version >/dev/null 2>&1; then
  echo "erl-raster: the cargo-ndk subcommand is missing. Run: cargo install cargo-ndk" >&2
  exit 1
fi

if [ ! -f "$crate_root/crates/erl-raster-ffi/Cargo.toml" ]; then
  echo "erl-raster: no engine at $crate_root. Point ERL_RASTER_DIR at the checkout." >&2
  exit 1
fi

if [ -z "${ANDROID_NDK_HOME:-}" ] && [ -z "${ANDROID_NDK_ROOT:-}" ]; then
  echo "erl-raster: neither ANDROID_NDK_HOME nor ANDROID_NDK_ROOT is set." >&2
  echo "erl-raster: set one to the NDK directory, for example" >&2
  echo "            export ANDROID_NDK_HOME=\$HOME/Library/Android/sdk/ndk/27.1.12297006" >&2
  exit 1
fi

mkdir -p "$staging"

for abi in "${abis[@]}"; do
  case "$abi" in
    arm64-v8a)   rust_target=aarch64-linux-android ;;
    armeabi-v7a) rust_target=armv7-linux-androideabi ;;
    x86_64)      rust_target=x86_64-linux-android ;;
    x86)         rust_target=i686-linux-android ;;
    *)
      echo "erl-raster: unknown ABI $abi" >&2
      exit 1
      ;;
  esac

  if ! rustup target list --installed | grep -qx "$rust_target"; then
    echo "erl-raster: the Rust target $rust_target is not installed." >&2
    echo "erl-raster: run: rustup target add $rust_target" >&2
    exit 1
  fi

  echo "erl-raster: building $abi ($rust_target)"
  (
    cd "$crate_root"
    cargo ndk -t "$abi" -o "$staging" build --release -p erl-raster-ffi
  )
done

echo "erl-raster: staged into $staging"
find "$staging" -name 'liberl_raster.so' -exec ls -l {} \;
