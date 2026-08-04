#!/usr/bin/env bash
# Cut a Legion release.
#
# Usage: scripts/release.sh <version>      e.g. scripts/release.sh 0.1.0
#
# Bumps the workspace version, verifies the build, commits, tags, and pushes.
# The release.yml GitHub Actions workflow then builds, publishes, and signs
# everything. Nothing is published from this local script.
#
# Prereqs:
#   - clean working tree (no uncommitted changes)
#   - on the `main` branch
#   - the version must be a clean semver (0.1.0, 0.2.0-rc.1, ...)
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <version>" >&2
  echo "  e.g. $0 0.1.0" >&2
  echo "       $0 0.1.0-rc.1" >&2
  exit 2
fi

VERSION="$1"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Validate semver shape (allow optional -pre).
if ! echo "$VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
  echo "error: '$VERSION' is not a valid semver (e.g. 0.1.0, 0.2.0-rc.1)" >&2
  exit 1
fi

# Guard: clean tree + main branch.
if [ -n "$(git status --porcelain)" ]; then
  echo "error: working tree has uncommitted changes; commit or stash first" >&2
  git status --short >&2
  exit 1
fi
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$BRANCH" != "main" ]; then
  echo "error: must be on 'main' (currently on '$BRANCH')" >&2
  exit 1
fi

# Resolve the push remote: prefer main's configured upstream, fall back to
# the only remote if there is exactly one. Don't hardcode 'origin' — clones
# may name it anything (e.g. 'uselegion').
REMOTE="$(git config "branch.${BRANCH}.remote" 2>/dev/null || true)"
if [ -z "$REMOTE" ]; then
  REMOTES="$(git remote)"
  if [ "$(echo "$REMOTES" | wc -l | tr -d ' ')" = "1" ]; then
    REMOTE="$REMOTES"
  else
    echo "error: no upstream configured for '$BRANCH' and multiple remotes exist:" >&2
    echo "$REMOTES" >&2
    echo "set one with: git branch --set-upstream-to=<remote>/$BRANCH" >&2
    exit 1
  fi
fi

# Bump version in the workspace root (single source of truth).
CURRENT=$(grep -m1 '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
if [ "$CURRENT" = "$VERSION" ]; then
  echo "Cargo.toml is already at $VERSION — skipping bump"
else
  echo "bumping Cargo.toml: $CURRENT -> $VERSION"
  # Use a precise sed to hit only the [workspace.package] version line.
  python3 - "$VERSION" <<'PY'
import sys, re
ver = sys.argv[1]
with open("Cargo.toml") as f:
    s = f.read()
s2 = re.sub(r'^version = "[^"]*"', f'version = "{ver}"', s, count=1, flags=re.M)
assert s != s2, "version line not found"
with open("Cargo.toml", "w") as f:
    f.write(s2)
PY
fi

# Verify it builds before tagging (catch metadata/path issues early).
echo "verifying build..."
cargo build --workspace --all-targets

# Commit + tag + push.
git add Cargo.toml Cargo.lock
git commit -m "release: v${VERSION}"
git tag "v${VERSION}"
echo
echo "created tag v${VERSION}. Push to trigger the release workflow:"
echo "  git push $REMOTE main --tags"
echo
read -r -p "push now? [y/N] " ans
if [[ "$ans" =~ ^[Yy]$ ]]; then
  git push "$REMOTE" main --tags
  echo "pushed. Watch: https://github.com/dawnswwwww/uselegion/actions"
else
  echo "not pushed. Run when ready: git push $REMOTE main --tags"
fi
