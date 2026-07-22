# ShardX Sync Server

Self-hosted sync backend for ShardX Launcher.

## What it syncs

The launcher client can push/pull:

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

The API image build uses the host network. This keeps Debian and npm package downloads working on VPS providers whose Docker bridge rejects outbound repository traffic; runtime containers remain on the Compose network.

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

Protocol 2 uses server-owned revisions and compare-and-swap writes. Clients send their last accepted server revision as `revision`; new items use `0`. The server returns accepted item envelopes with the new revision and returns stale or lease-locked writes in `conflicts` without overwriting current data.

Item envelope:

```json
{
  "kind": "profile|proxy|fingerprint|cookies|storage_bundle",
  "id": "string",
  "updated_at": "@server_unix_seconds",
  "deleted_at": null,
  "device_id": "uuid",
  "revision": 12,
  "checksum": "sha256",
  "payload": {}
}
```

## Important workflow

The launcher acquires a per-profile lease before reconciling and launching. On browser close it waits for Chromium storage to flush, uploads changed state while the lease remains held, and releases the lease afterward. Installer and portable packages use the same dynamic lease behavior.

Profile must be stopped before cookie/storage import/export. Running Chromium may not flush SQLite/LevelDB yet.

## Data files

- SQLite DB: `sync-server/data/sync.sqlite`
- MinIO data: `sync-server/minio-data/`

Back up both folders.
