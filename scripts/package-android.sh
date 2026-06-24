#!/bin/bash

set -euo pipefail

RED="\033[0;31m"
GREEN="\033[0;32m"
YELLOW="\033[1;33m"
NC="\033[0m"

DEFAULT_NDK_VERSION="27.3.13750724"
DEFAULT_TARGETS="aarch64-linux-android armv7-linux-androideabi"
PACKAGE_DIR="ale-my-eyes-android"

log_info() {
    printf "%b%s%b\n" "$GREEN" "$1" "$NC"
}

log_warn() {
    printf "%b%s%b\n" "$YELLOW" "$1" "$NC"
}

log_error() {
    printf "%b%s%b\n" "$RED" "$1" "$NC" >&2
}

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        log_error "未找到命令: $1"
        exit 1
    fi
}

require_repo_root() {
    if [ ! -f "Cargo.toml" ]; then
        log_error "请在项目根目录运行此脚本"
        exit 1
    fi
}

resolve_sdk_root() {
    local sdk_root="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-/usr/local/lib/android/sdk}}"
    if [ ! -d "$sdk_root" ]; then
        log_error "Android SDK 目录不存在: $sdk_root"
        log_error "请设置 ANDROID_HOME 或 ANDROID_SDK_ROOT"
        exit 1
    fi

    printf "%s\n" "$sdk_root"
}

resolve_ndk_root() {
    local sdk_root="$1"
    local requested_version="${ANDROID_NDK_VERSION:-}"

    if [ -n "${ANDROID_NDK_ROOT:-}" ]; then
        if [ ! -d "$ANDROID_NDK_ROOT" ]; then
            log_error "ANDROID_NDK_ROOT 不存在: $ANDROID_NDK_ROOT"
            exit 1
        fi
        printf "%s\n" "$ANDROID_NDK_ROOT"
        return
    fi

    if [ -n "$requested_version" ]; then
        local requested_path="$sdk_root/ndk/$requested_version"
        if [ ! -d "$requested_path" ]; then
            log_error "未找到 Android NDK 版本: $requested_version"
            log_error "请安装该版本，或显式设置 ANDROID_NDK_ROOT"
            exit 1
        fi
        printf "%s\n" "$requested_path"
        return
    fi

    if [ -d "$sdk_root/ndk/$DEFAULT_NDK_VERSION" ]; then
        printf "%s\n" "$sdk_root/ndk/$DEFAULT_NDK_VERSION"
        return
    fi

    local newest_path=""
    newest_path=$(find "$sdk_root/ndk" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort -V | tail -n 1 || true)
    if [ -n "$newest_path" ]; then
        log_warn "默认 NDK 版本 $DEFAULT_NDK_VERSION 不存在，回退到 $(basename "$newest_path")"
        printf "%s\n" "$newest_path"
        return
    fi

    log_error "未找到任何可用的 Android NDK"
    log_error "请安装 Android NDK，或显式设置 ANDROID_NDK_ROOT"
    exit 1
}

ensure_cargo_apk() {
    if ! cargo apk --version >/dev/null 2>&1; then
        log_info "安装 cargo-apk..."
        cargo install cargo-apk
    fi
}

ensure_keystore() {
    local keystore_path="${CARGO_APK_RELEASE_KEYSTORE:-${RUNNER_TEMP:-/tmp}/ale-my-eyes-android-release.keystore}"
    local keystore_password="${CARGO_APK_RELEASE_KEYSTORE_PASSWORD:-android}"

    if [ ! -f "$keystore_path" ]; then
        log_info "生成临时 Android 签名 keystore..."
        keytool -genkeypair -v \
            -storetype PKCS12 \
            -keystore "$keystore_path" \
            -storepass "$keystore_password" \
            -alias androiddebugkey \
            -keypass "$keystore_password" \
            -keyalg RSA \
            -keysize 2048 \
            -validity 10000 \
            -dname "CN=Android Debug,O=Android,C=US" >/dev/null 2>&1
    fi

    export CARGO_APK_RELEASE_KEYSTORE="$keystore_path"
    export CARGO_APK_RELEASE_KEYSTORE_PASSWORD="$keystore_password"
}

prepare_output_dir() {
    rm -rf "$PACKAGE_DIR"
    mkdir -p "$PACKAGE_DIR"
}

output_name_for_target() {
    case "$1" in
        aarch64-linux-android)
            printf "%s\n" "ale-my-eyes-arm64.apk"
            ;;
        armv7-linux-androideabi)
            printf "%s\n" "ale-my-eyes-armv7.apk"
            ;;
        *)
            log_error "不支持的 Android target: $1"
            exit 1
            ;;
    esac
}

clear_apk_outputs() {
    local target="$1"
    rm -f target/release/apk/*.apk "target/$target/release/apk"/*.apk 2>/dev/null || true
}

locate_apk_for_target() {
    local target="$1"
    local apk_path=""
    local target_dir="target/$target/release/apk"

    if [ -d "$target_dir" ]; then
        apk_path=$(find "$target_dir" -maxdepth 1 -type f -name "*.apk" | sort | tail -n 1 || true)
    fi

    if [ -z "$apk_path" ] && [ -d "target/release/apk" ]; then
        apk_path=$(find "target/release/apk" -maxdepth 1 -type f -name "*.apk" | sort | tail -n 1 || true)
    fi

    if [ -z "$apk_path" ] || [ ! -f "$apk_path" ]; then
        log_error "未找到 $target 构建产物 APK"
        exit 1
    fi

    printf "%s\n" "$apk_path"
}

build_target() {
    local target="$1"
    local output_name="$2"
    local apk_path=""

    clear_apk_outputs "$target"

    log_info "构建 $target..."
    cargo apk build -p ale-gui --target "$target" --lib --release

    apk_path=$(locate_apk_for_target "$target")
    cp "$apk_path" "$PACKAGE_DIR/$output_name"
    log_info "已生成: $PACKAGE_DIR/$output_name"
}

create_archive() {
    if command -v zip >/dev/null 2>&1; then
        log_info "创建 zip 压缩包..."
        rm -f ale-my-eyes-android.zip
        zip -rq ale-my-eyes-android.zip "$PACKAGE_DIR"
    elif command -v tar >/dev/null 2>&1; then
        log_info "创建 tar.gz 压缩包..."
        rm -f ale-my-eyes-android.tar.gz
        tar -czf ale-my-eyes-android.tar.gz "$PACKAGE_DIR"
    else
        log_warn "未找到 zip 或 tar，跳过压缩包创建"
    fi
}

print_summary() {
    log_info "Android 打包完成"
    printf "输出目录: %s\n" "$PACKAGE_DIR"

    if [ -f "$PACKAGE_DIR/ale-my-eyes-arm64.apk" ]; then
        printf "arm64 APK: %s/%s\n" "$PACKAGE_DIR" "ale-my-eyes-arm64.apk"
    fi
    if [ -f "$PACKAGE_DIR/ale-my-eyes-armv7.apk" ]; then
        printf "armv7 APK: %s/%s\n" "$PACKAGE_DIR" "ale-my-eyes-armv7.apk"
    fi
}

main() {
    local sdk_root=""
    local ndk_root=""
    local output_name=""

    require_repo_root
    require_command rustup
    require_command cargo
    require_command keytool
    require_command find

    sdk_root=$(resolve_sdk_root)
    export ANDROID_HOME="$sdk_root"
    export ANDROID_SDK_ROOT="$sdk_root"

    ndk_root=$(resolve_ndk_root "$sdk_root")
    export ANDROID_NDK_ROOT="$ndk_root"
    export ANDROID_NDK_HOME="$ndk_root"
    export ANDROID_NDK="$ndk_root"
    export NDK_HOME="$ndk_root"
    export ANDROID_NDK_VERSION="$(basename "$ndk_root")"

    log_info "使用 Android SDK: $ANDROID_HOME"
    log_info "使用 Android NDK: $ANDROID_NDK_ROOT"

    ensure_cargo_apk

    read -r -a BUILD_TARGETS <<< "${ANDROID_BUILD_TARGETS:-$DEFAULT_TARGETS}"
    log_info "添加 Android Rust targets..."
    rustup target add "${BUILD_TARGETS[@]}"

    ensure_keystore
    prepare_output_dir

    for target in "${BUILD_TARGETS[@]}"; do
        output_name=$(output_name_for_target "$target")
        build_target "$target" "$output_name"
    done

    create_archive
    print_summary
}

main "$@"
