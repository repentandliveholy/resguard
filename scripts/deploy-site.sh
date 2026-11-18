#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST="${DEPLOY_HOST:-ovh}"
TARGET_DIR="${DEPLOY_TARGET_DIR:-/srv/resguard-site}"

cp "$ROOT/install.sh" "$ROOT/site/install.sh"
cp "$ROOT/install.ps1" "$ROOT/site/install.ps1"

echo "==> Deploying resguard site to ${HOST}:${TARGET_DIR}..."
ssh "$HOST" "sudo mkdir -p '${TARGET_DIR}' && sudo chown -R godsheep:caddy '${TARGET_DIR}'"
rsync -avz --delete \
  --exclude=".DS_Store" \
  "$ROOT/site/" "${HOST}:${TARGET_DIR}/"

ssh "$HOST" "chmod -R u=rwX,go=rX '${TARGET_DIR}'"
echo "==> Site deployed to ${HOST}:${TARGET_DIR}"
