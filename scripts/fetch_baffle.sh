#!/bin/sh
set -eu

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

pin_file=$(dirname "$0")/dependency-pins.json
pin=$(python3 -c 'import json,sys; p=json.load(open(sys.argv[1]))["dependencies"]["baffle"]; a=p["archives"][sys.argv[2]]; print(p["version"],p["repository"],p["sha256sums_sha256"],a["filename"],a["sha256"])' "$pin_file" "$arch")
IFS=' ' read -r BAFFLE_VERSION repository SHA256SUMS_SHA256 archive expected <<EOF
$pin
EOF

if [ -e "$destination" ] || [ -L "$destination" ]; then
  echo "destination already exists: $destination" >&2
  exit 1
fi

parent=$(dirname "$destination")
mkdir -p "$parent"
temp_root=$(mktemp -d "$parent/.fetch-baffle.XXXXXX")
trap 'rm -rf "$temp_root"' EXIT

release_url="https://github.com/${repository}/releases/download/v${BAFFLE_VERSION}"
archive_path="$temp_root/$archive"
curl --fail --location --retry 3 --output "$archive_path" "$release_url/$archive"
curl --fail --location --retry 3 --output "$temp_root/SHA256SUMS" "$release_url/SHA256SUMS"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{ print $1 }'
  else
    echo "No SHA-256 utility is available" >&2
    exit 1
  fi
}

actual_sums=$(sha256 "$temp_root/SHA256SUMS")
if [ "$actual_sums" != "$SHA256SUMS_SHA256" ]; then
  echo "SHA256SUMS checksum mismatch for Baffle v${BAFFLE_VERSION}" >&2
  exit 1
fi
published=$(awk -v archive="$archive" '$2 == archive || $2 == "*" archive { print $1 }' "$temp_root/SHA256SUMS")
if [ "$published" != "$expected" ]; then
  echo "SHA256SUMS does not match the pinned Baffle archive digest for $archive" >&2
  exit 1
fi

actual=$(sha256 "$archive_path")
if [ "$actual" != "$expected" ]; then
  echo "Checksum mismatch for $archive" >&2
  exit 1
fi

mkdir "$temp_root/package"
tar -xzf "$archive_path" -C "$temp_root/package"
test -s "$temp_root/package/baffle"
chmod 0755 "$temp_root/package/baffle"
mv "$temp_root/package" "$destination"
