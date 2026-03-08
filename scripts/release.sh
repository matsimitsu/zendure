#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

# 1. Read current version from Cargo.toml
current=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
IFS='.' read -r major minor patch <<< "$current"

# 2. Bump patch version
new_patch=$((patch + 1))
new_version="${major}.${minor}.${new_patch}"

echo "Bumping version: ${current} -> ${new_version}"

# Update Cargo.toml
sed -i.bak "s/^version = \"${current}\"/version = \"${new_version}\"/" Cargo.toml
rm -f Cargo.toml.bak

# Update Cargo.lock
cargo generate-lockfile --quiet 2>/dev/null || true

# 3. Commit and tag
git add Cargo.toml Cargo.lock
git commit -m "Release v${new_version}"
git tag "v${new_version}"

# 4. Push commit and tag
git push
git push origin "v${new_version}"

echo "Released v${new_version}"
