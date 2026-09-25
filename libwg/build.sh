#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
usage: libwg/build.sh [--test]

Build the patched wireguard-go archive without changing the checked-out
submodule. CORPLINK_WG_SOURCE may point at another local Git checkout.
With --test, go test ./libwg ./corplink ./conn runs in the same patched copy.
EOF
}

run_tests=0
for arg in "$@"; do
    case "$arg" in
        --test) run_tests=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage; exit 2 ;;
    esac
done

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repo_root=$(cd -- "$script_dir/.." && pwd -P)
patch_dir="$script_dir/patches"
source_spec="${CORPLINK_WG_SOURCE:-$script_dir/wireguard-go}"
source_is_default=0
if [[ -z "${CORPLINK_WG_SOURCE:-}" ]]; then
    source_is_default=1
fi

for required in git tar go make; do
    command -v "$required" >/dev/null 2>&1 || {
        printf 'corplink libwg build: required command missing: %s\n' "$required" >&2
        exit 127
    }
done

temp_root=$(mktemp -d "${TMPDIR:-/tmp}/corplink-wg-build.XXXXXX")
install_stage=$(mktemp -d "$script_dir/.corplink-wg-install.XXXXXX")
cleanup() {
    rm -rf -- "$temp_root" "$install_stage"
}
trap cleanup EXIT INT TERM

source_root=''
expected_source=$(cd -- "$source_spec" 2>/dev/null && pwd -P || true)
source_root=$(git -C "$source_spec" rev-parse --show-toplevel 2>/dev/null || true)
source_root=$(cd -- "$source_root" 2>/dev/null && pwd -P || true)
if [[ -z "$expected_source" || "$source_root" != "$expected_source" ]]; then
    if ((source_is_default)); then
        git -C "$repo_root" submodule update --init --recursive -- libwg/wireguard-go
        expected_source=$(cd -- "$source_spec" && pwd -P)
        source_root=$(git -C "$source_spec" rev-parse --show-toplevel 2>/dev/null || true)
        source_root=$(cd -- "$source_root" 2>/dev/null && pwd -P || true)
    fi
fi
if [[ -z "$expected_source" || "$source_root" != "$expected_source" ]]; then
    printf 'corplink libwg build: source is not an initialized Git checkout rooted at %s\n' "$source_spec" >&2
    exit 2
fi

if ! git -C "$source_root" diff --quiet || ! git -C "$source_root" diff --cached --quiet; then
    printf 'corplink libwg build: source checkout has tracked changes; refusing to archive it: %s\n' "$source_root" >&2
    exit 2
fi

source_revision=$(git -C "$source_root" rev-parse HEAD)
source_version=$(git -C "$source_root" describe --tags --always "$source_revision" 2>/dev/null || true)
if [[ -z "$source_version" ]]; then
    source_version="source-${source_revision:0:12}"
fi

patched_root="$temp_root/patched-source"
mkdir -p "$patched_root"
archive_path="$temp_root/source.tar"
git -C "$source_root" archive --format=tar --output="$archive_path" "$source_revision"
tar -xf "$archive_path" -C "$patched_root"

version_literal=$(printf '%s' "$source_version" | sed 's/\\/\\\\/g; s/"/\\"/g')
version_text=$(printf 'package main\n\nconst Version = "%s"\n' "$version_literal")
printf '%s\n' "$version_text" > "$patched_root/version.go"
mkdir -p "$patched_root/libwg"
printf '%s\n' "$version_text" > "$patched_root/libwg/version.go"

patch_count=$(find "$patch_dir" -maxdepth 1 -type f -name '*.patch' -print | wc -l | tr -d ' ')
if [[ "$patch_count" -gt 0 ]]; then
    while IFS= read -r patch; do
        git -C "$patched_root" apply --check "$patch"
        git -C "$patched_root" apply "$patch"
    done < <(find "$patch_dir" -maxdepth 1 -type f -name '*.patch' -print | LC_ALL=C sort)
fi

if ((run_tests)); then
    (cd "$patched_root" && go test ./libwg ./corplink ./conn)
    host_os=$(uname -s)
    if [[ "$host_os" == "Darwin" || "$host_os" == "Linux" ]]; then
        command -v python3 >/dev/null 2>&1 || {
            printf 'corplink libwg build: python3 is required for the FFI probe\n' >&2
            exit 127
        }
        case "$host_os" in
            Darwin) probe_library="$temp_root/libwg-probe.dylib" ;;
            Linux) probe_library="$temp_root/libwg-probe.so" ;;
        esac
        (cd "$patched_root" && CGO_ENABLED=1 go build -trimpath -buildmode=c-shared -o "$probe_library" ./libwg)
        python3 "$repo_root/tests/libwg_ffi_probe.py" "$probe_library"
    fi
fi

(cd "$patched_root" && make -B -o generate-version libwg)
[[ -s "$patched_root/libwg.a" ]] || {
    printf 'corplink libwg build: patched build did not produce libwg.a\n' >&2
    exit 1
}
[[ -s "$patched_root/libwg.h" ]] || {
    printf 'corplink libwg build: patched build did not produce libwg.h\n' >&2
    exit 1
}

mkdir -p "$install_stage"
cp -- "$patched_root/libwg.a" "$install_stage/libwg.a"
cp -- "$patched_root/libwg.h" "$install_stage/libwg.h"

old_archive="$install_stage/old-libwg.a"
old_header="$install_stage/old-libwg.h"
archive_backed_up=0
header_backed_up=0
restore_old() {
    if ((archive_backed_up)); then
        rm -f -- "$script_dir/libwg.a"
        mv -- "$old_archive" "$script_dir/libwg.a"
    fi
    if ((header_backed_up)); then
        rm -f -- "$script_dir/libwg.h"
        mv -- "$old_header" "$script_dir/libwg.h"
    fi
}
trap restore_old ERR
if [[ -e "$script_dir/libwg.a" ]]; then
    mv -- "$script_dir/libwg.a" "$old_archive"
    archive_backed_up=1
fi
if [[ -e "$script_dir/libwg.h" ]]; then
    mv -- "$script_dir/libwg.h" "$old_header"
    header_backed_up=1
fi
mv -- "$install_stage/libwg.a" "$script_dir/libwg.a"
mv -- "$install_stage/libwg.h" "$script_dir/libwg.h"
trap - ERR
printf 'corplink libwg build: installed patched source %s\n' "$source_revision"
