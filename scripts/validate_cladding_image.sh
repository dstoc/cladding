#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
containerfile="$repo_dir/Containerfile.cladding"
build_dir=$(mktemp -d)
image=localhost/cladding-tools:ci

cleanup() {
    podman image rm --force "$image" >/dev/null 2>&1 || true
    rm -rf "$build_dir"
}
trap cleanup EXIT INT TERM

cp "$containerfile" "$build_dir/Containerfile"

tools_stage=$(grep -n '^FROM base AS npm-tools$' "$containerfile" | cut -d: -f1)
final_stage=$(grep -n '^FROM base$' "$containerfile" | tail -n 1 | cut -d: -f1)
if [ -z "$tools_stage" ] || [ -z "$final_stage" ] || [ "$tools_stage" -ge "$final_stage" ]; then
    echo "Containerfile must copy npm tools from an intermediate stage" >&2
    exit 1
fi

for package in '@openai/codex' '@earendil-works/pi-coding-agent' '@anthropic-ai/claude-code'; do
    metadata_line=$(grep -n -F "ADD https://registry.npmjs.org/$package/latest " "$containerfile" | cut -d: -f1)
    if [ -z "$metadata_line" ] || [ "$metadata_line" -le "$tools_stage" ] || [ "$metadata_line" -ge "$final_stage" ]; then
        echo "Containerfile must fetch latest-version metadata in the npm tools stage for $package" >&2
        exit 1
    fi
done

podman build --tag "$image" --file "$build_dir/Containerfile" "$build_dir"
podman run --rm --entrypoint /bin/sh "$image" -ec '
    for tool in codex pi claude; do
        command -v "$tool" >/dev/null || {
            echo "Expected npm tool is missing: $tool" >&2
            exit 1
        }
        [ -L "$(command -v "$tool")" ] || {
            echo "Expected npm tool command to resolve through its package link: $tool" >&2
            exit 1
        }
    done

    codex --help >/dev/null

    for tool in codex pi claude; do
        "$tool" --version
    done

    for metadata in codex pi claude; do
        if [ -e "/tmp/npm-$metadata.json" ]; then
            echo "Build-only npm metadata remains in the image: /tmp/npm-$metadata.json" >&2
            exit 1
        fi
    done
'
