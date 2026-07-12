# ShardX Sync Server

Self-hosted sync backend for ShardX Launcher.

## What it syncs

The launcher v0.2.7 client can push/pull:

- profiles
- proxies
- fingerprint library entries
- cookies when enabled in Settings

This server also includes MinIO object-storage endpoints for the next phase: LocalStorage, IndexedDB, Session Storage, and extension storage bundles.

## VPS install: Ubuntu/Debian

```bash
git clone https://github.com/zhetaa666/ShardX-Wayfern.git
cd ShardX-Wayfern/sync-server
sudo bash scripts/install-ubuntu.sh
```

The script installs Docker + Compose, creates `.env`, generates a token, builds the server, and starts:

- API: `http://YOUR_VPS_IP:8080`
- MinIO S3: `http://YOUR_VPS_IP:9000`
- MinIO console: `http://YOUR_VPS_IP:9001`

## Manual run

```bash
cp .env.example .env
# edit SYNC_TOKEN and MINIO_SECRET_KEY
openssl rand -hex 32
docker compose up -d --build
```

## Configure ShardX Launcher

Open Settings → Self-hosted sync:

- Enable sync client
- Server URL: `http://YOUR_VPS_IP:8080` or your HTTPS domain
- Bearer token: value from `.env` `SYNC_TOKEN`
- Enable cookies/session login state if you want same logged-in accounts on another device
- Save settings
- Click Test connection
- Click Sync now

## HTTPS recommendation

For production, put Caddy/Nginx in front and use HTTPS. Bearer token protects access, but without client-side encryption the VPS can read synced cookies.

## API

All sync endpoints require:

```http
Authorization: Bearer <SYNC_TOKEN>
```

Endpoints:

- `GET /health`
- `POST /sync/push`
- `GET /sync/changes?since=seq:<n>`
- `GET /sync/item/:kind/:id`
- `POST /storage/:profileId/:revision`
- `GET /storage/:profileId/latest`

Item envelope:

```json
{
  "kind": "profile|proxies|fingerprint|cookies",
  "id": "string",
  "updated_at": "@unix_seconds",
  "deleted_at": null,
  "device_id": "uuid",
  "revision": 0,
  "payload": {}
}
```

## Important workflow

For reliable premium-style session sync:

1. Stop profile on Device A.
2. Sync now uploads latest cookies/profile config.
3. Device B Sync now pulls latest state.
4. Launch profile on Device B.

Profile must be stopped before cookie/storage import/export. Running Chromium may not flush SQLite/LevelDB yet.

## Data files

- SQLite DB: `sync-server/data/sync.sqlite`
- MinIO data: `sync-server/minio-data/`

Back up both folders.
