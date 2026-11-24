#!/bin/sh
# Kipo (`kp`) one-line POSIX installer
# Repository: https://github.com/junjamrk/kipo
#
# Environment overrides:
#   KP_REPO              GitHub <owner>/<repo> (default: junjamrk/kipo)
#   KP_VERSION           Release tag `vX.Y.Z` or `latest` (default: latest)
#   KP_INSTALL_DIR       Target directory for binaries (default: $HOME/.local/bin)
#   KP_RELEASE_BASE_URL  Release base URL (default: https://github.com/$KP_REPO/releases)
#   KP_INSTALL_ARCHIVE   Local `.tar.gz` archive path for air-gapped/offline installation
#   KP_INSTALL_SHA256SUMS Local `SHA256SUMS` file path paired with `KP_INSTALL_ARCHIVE`

set -eu

err() {
    printf 'kipo-install: error: %s\n' "$*" >&2
    exit 1
}

info() {
    printf '==> %s\n' "$*"
}

KP_REPO="${KP_REPO:-junjamrk/kipo}"
KP_VERSION="${KP_VERSION:-latest}"
if [ -z "${KP_INSTALL_DIR:-}" ]; then
    if [ -z "${HOME:-}" ]; then
        err "HOME is not set; pass KP_INSTALL_DIR explicitly"
    fi
    KP_INSTALL_DIR="$HOME/.local/bin"
fi
KP_RELEASE_BASE_URL="${KP_RELEASE_BASE_URL:-https://github.com/${KP_REPO}/releases}"

detect_target() {
    os_name="$(uname -s 2>/dev/null || printf 'unknown')"
    arch_name="$(uname -m 2>/dev/null || printf 'unknown')"

    case "$os_name" in
        Darwin) target_os="apple-darwin" ;;
        Linux)
            if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
                err "musl libc detected; prebuilt Linux archives require glibc >= 2.28. Build from source via: cargo install --git https://github.com/${KP_REPO} kipo --no-default-features --features connectors"
            fi
            target_os="unknown-linux-gnu"
            ;;
        *) err "unsupported operating system: $os_name (supported: Darwin, Linux; on Windows use scripts/install.ps1)" ;;
    esac

    case "$arch_name" in
        x86_64 | amd64) target_arch="x86_64" ;;
        aarch64 | arm64) target_arch="aarch64" ;;
        *) err "unsupported CPU architecture: $arch_name (supported: x86_64, aarch64)" ;;
    esac

    printf '%s-%s' "$target_arch" "$target_os"
}

compute_sha256() {
    file_path="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file_path" | awk '{print tolower($1)}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file_path" | awk '{print tolower($1)}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$file_path" | awk '{print tolower($NF)}'
    else
        err "missing SHA-256 tool (install sha256sum, shasum, or openssl)"
    fi
}

download_to_file() {
    url="$1"
    dest="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 2 -o "$dest" "$url" || err "failed to download $url"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$dest" "$url" || err "failed to download $url"
    else
        err "neither curl nor wget is available to download $url"
    fi
}

lookup_expected_sha256() {
    sums_file="$1"
    asset_basename="$2"
    awk -v target="$asset_basename" '
        /^[[:space:]]*#/ || NF < 2 { next }
        {
            hash = tolower($1)
            name = $2
            sub(/^\*/, "", name)
            sub(/^\.\//, "", name)
            n = split(name, parts, "/")
            base = parts[n]
            if (base == target && hash ~ /^[0-9a-f]{64}$/) {
                print hash
                exit 0
            }
        }
    ' "$sums_file"
}

TARGET_TRIPLE="$(detect_target)"
ASSET_NAME="kipo-${TARGET_TRIPLE}.tar.gz"

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/kipo-install.XXXXXX")"
cleanup() {
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT INT TERM

ARCHIVE_PATH="$TMP_DIR/$ASSET_NAME"
SUMS_PATH="$TMP_DIR/SHA256SUMS"

if [ -n "${KP_INSTALL_ARCHIVE:-}" ] || [ -n "${KP_INSTALL_SHA256SUMS:-}" ]; then
    if [ -z "${KP_INSTALL_ARCHIVE:-}" ] || [ -z "${KP_INSTALL_SHA256SUMS:-}" ]; then
        err "offline install requires both KP_INSTALL_ARCHIVE and KP_INSTALL_SHA256SUMS"
    fi
    [ -f "$KP_INSTALL_ARCHIVE" ] || err "archive file not found: $KP_INSTALL_ARCHIVE"
    [ -f "$KP_INSTALL_SHA256SUMS" ] || err "checksum file not found: $KP_INSTALL_SHA256SUMS"
    ARCHIVE_BASENAME="$(basename "$KP_INSTALL_ARCHIVE")"
    cp "$KP_INSTALL_ARCHIVE" "$TMP_DIR/$ARCHIVE_BASENAME"
    ARCHIVE_PATH="$TMP_DIR/$ARCHIVE_BASENAME"
    cp "$KP_INSTALL_SHA256SUMS" "$SUMS_PATH"
else
    ARCHIVE_BASENAME="$ASSET_NAME"
    base_trimmed="${KP_RELEASE_BASE_URL%/}"
    if [ "$KP_VERSION" = "latest" ]; then
        DOWNLOAD_PREFIX="${base_trimmed}/latest/download"
    else
        DOWNLOAD_PREFIX="${base_trimmed}/download/${KP_VERSION}"
    fi
    info "Downloading ${ARCHIVE_BASENAME} from ${DOWNLOAD_PREFIX}..."
    download_to_file "${DOWNLOAD_PREFIX}/${ARCHIVE_BASENAME}" "$ARCHIVE_PATH"
    download_to_file "${DOWNLOAD_PREFIX}/SHA256SUMS" "$SUMS_PATH"
fi

EXPECTED_SHA="$(lookup_expected_sha256 "$SUMS_PATH" "$ARCHIVE_BASENAME")"
if [ -z "$EXPECTED_SHA" ]; then
    err "no valid SHA-256 entry for ${ARCHIVE_BASENAME} in SHA256SUMS"
fi

ACTUAL_SHA="$(compute_sha256 "$ARCHIVE_PATH")"
if [ "$ACTUAL_SHA" != "$EXPECTED_SHA" ]; then
    err "SHA-256 checksum mismatch for ${ARCHIVE_BASENAME}: expected ${EXPECTED_SHA}, got ${ACTUAL_SHA}"
fi
info "Verified SHA-256 (${ACTUAL_SHA}) for ${ARCHIVE_BASENAME}"

EXTRACT_DIR="$TMP_DIR/extract"
mkdir -p "$EXTRACT_DIR"
tar -xzf "$ARCHIVE_PATH" -C "$EXTRACT_DIR" || err "failed to extract ${ARCHIVE_BASENAME}"

find_extracted_bin() {
    bin_name="$1"
    if [ -f "$EXTRACT_DIR/$bin_name" ]; then
        printf '%s' "$EXTRACT_DIR/$bin_name"
        return 0
    fi
    found="$(find "$EXTRACT_DIR" -maxdepth 3 -type f -name "$bin_name" | head -n 1 || true)"
    if [ -n "$found" ]; then
        printf '%s' "$found"
        return 0
    fi
    return 1
}

KP_EXTRACTED="$(find_extracted_bin kp)" || err "archive ${ARCHIVE_BASENAME} does not contain a 'kp' binary"
chmod 0755 "$KP_EXTRACTED"
VERSION_OUT="$("$KP_EXTRACTED" --version 2>&1)" || err "extracted kp binary is incompatible with this host (--version failed): ${VERSION_OUT}"

CONN_EXTRACTED=""
if CONN_EXTRACTED="$(find_extracted_bin kp-connector)"; then
    chmod 0755 "$CONN_EXTRACTED"
    "$CONN_EXTRACTED" --version >/dev/null 2>&1 || err "extracted kp-connector binary is incompatible with this host (--version failed)"
fi

# Create install directory and stage atomically ONLY after SHA-256, archive extraction, and host binary self-checks succeeded.
mkdir -p "$KP_INSTALL_DIR"

install_binary_atomic() {
    src_bin="$1"
    dst_name="$2"
    stage_bin="${KP_INSTALL_DIR}/.${dst_name}.tmp.$$"
    cp "$src_bin" "$stage_bin"
    chmod 0755 "$stage_bin"
    mv -f "$stage_bin" "${KP_INSTALL_DIR}/${dst_name}"
}

install_binary_atomic "$KP_EXTRACTED" "kp"

if [ -n "$CONN_EXTRACTED" ]; then
    install_binary_atomic "$CONN_EXTRACTED" "kp-connector"
    info "Installed kp and kp-connector to ${KP_INSTALL_DIR}"
else
    info "Installed kp to ${KP_INSTALL_DIR}"
fi

info "Self-check passed: ${VERSION_OUT}"

case ":${PATH:-}:" in
    *":${KP_INSTALL_DIR}:"*) ;;
    *)
        printf '\nNote: add %s to your PATH:\n  export PATH="%s:$PATH"\n' "$KP_INSTALL_DIR" "$KP_INSTALL_DIR"
        ;;
esac

printf '\nQuickstart:\n'
printf '  1) kp demo --dir ./kipo-demo                                        # 5-minute interactive offline tour\n'
printf '  2) kp init --path ~/.kp/personal --owner alice                      # initialize your personal library\n'
printf '  3) kp connect claude-code --project . --path ~/.kp/personal --as alice\n'
printf '  4) kp inspect --path ~/.kp/personal --as alice                      # launch 127.0.0.1 read-only Web Inspector\n'
