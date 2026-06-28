#!/usr/bin/env sh
set -e

# pacto-bot-api pre-commit hook
# Install: cp scripts/pre-commit.sh .git/hooks/pre-commit && chmod +x .git/hooks/pre-commit

# Run Beads pre-commit hook if available
if command -v bd >/dev/null 2>&1; then
  echo "Running Beads pre-commit hook..."
  bd hooks run pre-commit "$@" || exit $?
fi

# Run project validation gate (fmt-check, clippy, tests)
echo "Running make validate..."
make validate
