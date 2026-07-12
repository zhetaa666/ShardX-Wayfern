#!/usr/bin/env bash
set -euo pipefail

if [ "${EUID:-$(id -u)}" -ne 0 ]; then
  echo "Run as root: sudo bash scripts/install-ubuntu.sh"
  exit 1
fi

apt-get update
apt-get install -y ca-certificates curl gnupg openssl
install -m 0755 -d /etc/apt/keyrings
if [ ! -f /etc/apt/keyrings/docker.gpg ]; then
  curl -fsSL https://download.docker.com/linux/ubuntu/gpg | gpg --dearmor -o /etc/apt/keyrings/docker.gpg || \
  curl -fsSL https://download.docker.com/linux/debian/gpg | gpg --dearmor -o /etc/apt/keyrings/docker.gpg
  chmod a+r /etc/apt/keyrings/docker.gpg
fi

. /etc/os-release
case "${ID}" in
  ubuntu|debian) DOCKER_ID="${ID}" ;;
  *) echo "Unsupported distro: ${ID}. Use Ubuntu/Debian."; exit 1 ;;
esac

echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/${DOCKER_ID} ${VERSION_CODENAME} stable" > /etc/apt/sources.list.d/docker.list
apt-get update
apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
systemctl enable --now docker

cd "$(dirname "$0")/.."
if [ ! -f .env ]; then
  cp .env.example .env
  TOKEN="$(openssl rand -hex 32)"
  MINIO_PASS="$(openssl rand -hex 32)"
  sed -i "s/^SYNC_TOKEN=.*/SYNC_TOKEN=${TOKEN}/" .env
  sed -i "s/^MINIO_SECRET_KEY=.*/MINIO_SECRET_KEY=${MINIO_PASS}/" .env
  echo "Created .env with generated SYNC_TOKEN and MINIO password."
fi

docker compose up -d --build

echo "ShardX sync server running."
echo "API: http://<your-vps-ip>:$(grep '^PORT=' .env | cut -d= -f2)"
echo "Token: $(grep '^SYNC_TOKEN=' .env | cut -d= -f2-)"
echo "MinIO console: http://<your-vps-ip>:$(grep '^MINIO_CONSOLE_PORT=' .env | cut -d= -f2)"
