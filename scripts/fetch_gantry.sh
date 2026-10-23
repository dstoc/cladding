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
pin=$(python3 -c 'import json,sys; p=json.load(open(sys.argv[1]))["dependencies"]["gantry"]; a=p["archives"][sys.argv[2]]; print(p["version"],p["repository"],p["sha256sums_sha256"],a["filename"],a["sha256"],a["package_root"])' "$pin_file" "$arch")
IFS=' ' read -r GANTRY_VERSION repository SHA256SUMS_SHA256 archive expected_archive_sha256 package_root <<EOF
$pin
EOF

if [ -e "$destination" ] || [ -L "$destination" ]; then
  echo "destination already exists: $destination" >&2
  exit 1
fi

parent=$(dirname "$destination")
mkdir -p "$parent"
temp_root=$(mktemp -d "$parent/.fetch-gantry.XXXXXX")
trap 'rm -rf "$temp_root"' EXIT

release_url="https://github.com/${repository}/releases/download/v${GANTRY_VERSION}"
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

actual_sums_sha256=$(sha256 "$temp_root/SHA256SUMS")
if [ "$actual_sums_sha256" != "$SHA256SUMS_SHA256" ]; then
  echo "SHA256SUMS checksum mismatch for Gantry v${GANTRY_VERSION}" >&2
  exit 1
fi
published_archive_sha256=$(awk -v archive="$archive" '$2 == archive { print $1 }' "$temp_root/SHA256SUMS")
if [ "$published_archive_sha256" != "$expected_archive_sha256" ]; then
  echo "SHA256SUMS does not match the pinned Gantry archive digest for $archive" >&2
  exit 1
fi
actual_archive_sha256=$(sha256 "$archive_path")
if [ "$actual_archive_sha256" != "$expected_archive_sha256" ]; then
  echo "archive checksum mismatch for $archive" >&2
  exit 1
fi

mkdir "$temp_root/unpacked"
tar -xzf "$archive_path" -C "$temp_root/unpacked"
package="$temp_root/unpacked/$package_root"
for path in gantry run-remote LICENSE README.md docs/reference.md; do
  if [ ! -s "$package/$path" ]; then
    echo "Gantry archive is missing $path" >&2
    exit 1
  fi
done
for binary in gantry run-remote; do
  chmod 0755 "$package/$binary"
  if [ ! -x "$package/$binary" ]; then
    echo "Gantry archive contains a non-executable $binary" >&2
    exit 1
  fi
done

printf '%s\n' "$GANTRY_VERSION" > "$package/VERSION"
mv "$package" "$destination"
