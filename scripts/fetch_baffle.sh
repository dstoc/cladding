#!/bin/sh
set -eu

BAFFLE_VERSION=2.2.0

usage() {
  echo "usage: $0 <x86_64|aarch64> <new-destination-directory>" >&2
  exit 2
}

[ "$#" -eq 2 ] || usage
arch=$1
destination=$2
case "$arch" in
  x86_64|aarch64) ;;
  *) usage ;;
esac

if [ -e "$destination" ] || [ -L "$destination" ]; then
  echo "destination already exists: $destination" >&2
  exit 1
fi

parent=$(dirname "$destination")
mkdir -p "$parent"
temp_root=$(mktemp -d "$parent/.fetch-baffle.XXXXXX")
trap 'rm -rf "$temp_root"' EXIT

archive="baffle-proxy-v${BAFFLE_VERSION}-${arch}-unknown-linux-gnu.tar.gz"
release_url="https://github.com/dstoc/baffle/releases/download/v${BAFFLE_VERSION}"
archive_path="$temp_root/$archive"
curl --fail --location --retry 3 --output "$archive_path" "$release_url/$archive"
curl --fail --location --retry 3 --output "$temp_root/SHA256SUMS" "$release_url/SHA256SUMS"

expected=$(awk -v archive="$archive" '$2 == archive || $2 == "*" archive { print $1 }' "$temp_root/SHA256SUMS")
case "$expected" in
  ''|*[!0-9a-fA-F]*)
    echo "Expected one SHA256SUMS entry for $archive" >&2
    exit 1
    ;;
esac
if [ "${#expected}" -ne 64 ]; then
  echo "Expected one SHA256SUMS entry for $archive" >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$archive_path" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$archive_path" | awk '{ print $1 }')
else
  echo "No SHA-256 utility is available" >&2
  exit 1
fi
if [ "$actual" != "$expected" ]; then
  echo "Checksum mismatch for $archive" >&2
  exit 1
fi

mkdir "$temp_root/package"
tar -xzf "$archive_path" -C "$temp_root/package"
test -s "$temp_root/package/baffle"
chmod 0755 "$temp_root/package/baffle"
mv "$temp_root/package" "$destination"
