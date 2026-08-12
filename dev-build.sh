#!/usr/bin/env bash

set -euo pipefail

repo_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
flatpak_app_dir="$repo_dir/build-dir"
flatpak_repo_dir="$repo_dir/.flatpak-builder/cache"
manifest="$repo_dir/flatpak/io.github.staffanberglund.transkribera.json"
target_dir="$repo_dir/target/flatpak-dev"
app_id="io.github.staffanberglund.transkribera"

run_app=true
if [[ "${1-}" == "--no-run" ]]; then
    run_app=false
elif [[ $# -gt 0 ]]; then
    printf 'Usage: %s [--no-run]\n' "$0" >&2
    exit 2
fi

cd "$repo_dir"

# Prepare a writable /app using the manifest's runtime and SDK. This is only
# needed once, after build-dir has been removed, or after the application ID
# has changed.
if [[ ! -f "$flatpak_app_dir/metadata" ]] \
    || ! grep -Fqx "name=$app_id" "$flatpak_app_dir/metadata"; then
    flatpak-builder \
        --user \
        --force-clean \
        --stop-at=transkribera \
        "$flatpak_app_dir" \
        "$manifest"
fi

# Keep Cargo's debug artifacts in the repository so dependencies survive each
# flatpak build invocation. The source tree is mounted at the same absolute path
# inside the build sandbox.
flatpak build \
    --filesystem="$repo_dir" \
    --build-dir="$repo_dir" \
    "$flatpak_app_dir" sh -c '
set -eu
repo_dir="$1"
target_dir="$2"
export PATH=/usr/lib/sdk/rust-stable/bin:$PATH
export CARGO_TARGET_DIR="$target_dir"
cd "$repo_dir"
cargo build --locked --offline
install -Dm755 "$target_dir/debug/transkribera" /app/bin/transkribera
strip /app/bin/transkribera
install -Dm644 data/io.github.staffanberglund.transkribera.desktop /app/share/applications/io.github.staffanberglund.transkribera.desktop
install -Dm644 data/io.github.staffanberglund.transkribera.metainfo.xml /app/share/metainfo/io.github.staffanberglund.transkribera.metainfo.xml
install -Dm644 data/icons/io.github.staffanberglund.transkribera.svg /app/share/icons/hicolor/scalable/apps/io.github.staffanberglund.transkribera.svg
' sh "$repo_dir" "$target_dir"

if ! grep -Fqx 'command=transkribera' "$flatpak_app_dir/metadata"; then
    flatpak-builder --finish-only "$flatpak_app_dir" "$manifest"
fi
flatpak build-export "$flatpak_repo_dir" "$flatpak_app_dir"
if flatpak info --user "$app_id" >/dev/null 2>&1; then
    flatpak update --user --assumeyes "$app_id"
else
    flatpak install --user --assumeyes "$flatpak_repo_dir" "$app_id"
fi

if [[ "$run_app" == true ]]; then
    flatpak run --user "$app_id"
fi
