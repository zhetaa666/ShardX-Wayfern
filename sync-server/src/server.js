import Fastify from 'fastify';
import cors from '@fastify/cors';
import multipart from '@fastify/multipart';
import Database from 'better-sqlite3';
import { Client as MinioClient } from 'minio';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

const env = (name, fallback = '') => process.env[name] || fallback;
const PORT = Number(env('PORT', '8080'));
const HOST = env('HOST', '0.0.0.0');
const DATA_DIR = env('DATA_DIR', './data');
const DB_PATH = env('DB_PATH', path.join(DATA_DIR, 'sync.sqlite'));
const SYNC_TOKEN = env('SYNC_TOKEN');
const WORKSPACE_ID = env('WORKSPACE_ID', 'default');
const MAX_BODY = Number(env('MAX_BODY_BYTES', String(50 * 1024 * 1024)));
const SYNC_PROTOCOL = 2;
const PROFILE_KINDS = new Set(['profile', 'cookies', 'storage_bundle']);

if (!SYNC_TOKEN || SYNC_TOKEN.length < 24) {
  console.error('SYNC_TOKEN must be set and at least 24 chars');
  process.exit(1);
}

fs.mkdirSync(path.dirname(DB_PATH), { recursive: true });
const db = new Database(DB_PATH);
db.pragma('journal_mode = WAL');
db.exec(`
CREATE TABLE IF NOT EXISTS sync_items (
  workspace_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  id TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  deleted_at TEXT,
  device_id TEXT NOT NULL,
  revision INTEGER NOT NULL,
  payload TEXT NOT NULL,
  server_seq INTEGER PRIMARY KEY AUTOINCREMENT,
  checksum TEXT NOT NULL,
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE UNIQUE INDEX IF NOT EXISTS sync_items_latest
ON sync_items(workspace_id, kind, id);
CREATE INDEX IF NOT EXISTS sync_items_seq
ON sync_items(workspace_id, server_seq);
CREATE TABLE IF NOT EXISTS object_refs (
  workspace_id TEXT NOT NULL,
  profile_id TEXT NOT NULL,
  revision INTEGER NOT NULL,
  object_key TEXT NOT NULL,
  checksum TEXT NOT NULL,
  size_bytes INTEGER NOT NULL,
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  PRIMARY KEY (workspace_id, profile_id, revision)
);
CREATE TABLE IF NOT EXISTS leases (
  workspace_id TEXT NOT NULL,
  profile_id TEXT NOT NULL,
  device_id TEXT NOT NULL,
  device_label TEXT,
  expires_at INTEGER NOT NULL,
  PRIMARY KEY (workspace_id, profile_id)
);
UPDATE sync_items SET revision = 1 WHERE revision < 1;
`);

const insertItem = db.prepare(`
INSERT INTO sync_items(
  workspace_id, kind, id, updated_at, deleted_at, device_id,
  revision, payload, checksum
)
VALUES(
  @workspace_id, @kind, @id, @updated_at, @deleted_at, @device_id,
  1, @payload, @checksum
)
`);
const updateItem = db.prepare(`
UPDATE sync_items SET
  updated_at=@updated_at,
  deleted_at=@deleted_at,
  device_id=@device_id,
  revision=revision + 1,
  payload=@payload,
  checksum=@checksum,
  server_seq=(SELECT COALESCE(MAX(server_seq), 0) + 1 FROM sync_items)
WHERE workspace_id=@workspace_id AND kind=@kind AND id=@id AND revision=@revision
`);
const changesStmt = db.prepare(`
SELECT kind,id,updated_at,deleted_at,device_id,revision,payload,server_seq,checksum
FROM sync_items
WHERE workspace_id = ? AND server_seq > ?
ORDER BY server_seq ASC
LIMIT ?
`);
const maxSeqStmt = db.prepare('SELECT COALESCE(MAX(server_seq), 0) AS seq FROM sync_items WHERE workspace_id = ?');
const itemStmt = db.prepare('SELECT * FROM sync_items WHERE workspace_id = ? AND kind = ? AND id = ?');
const objectInsert = db.prepare(`
INSERT OR REPLACE INTO object_refs(workspace_id, profile_id, revision, object_key, checksum, size_bytes)
VALUES(?, ?, ?, ?, ?, ?)
`);
const objectLatest = db.prepare(`
SELECT * FROM object_refs WHERE workspace_id = ? AND profile_id = ? ORDER BY revision DESC LIMIT 1
`);
const leaseGet = db.prepare('SELECT * FROM leases WHERE workspace_id = ? AND profile_id = ?');
const leaseUpsert = db.prepare(`
INSERT INTO leases(workspace_id, profile_id, device_id, device_label, expires_at)
VALUES(@workspace_id, @profile_id, @device_id, @device_label, @expires_at)
ON CONFLICT(workspace_id, profile_id) DO UPDATE SET
  device_id=excluded.device_id,
  device_label=excluded.device_label,
  expires_at=excluded.expires_at
`);
const leaseDelete = db.prepare('DELETE FROM leases WHERE workspace_id = ? AND profile_id = ? AND device_id = ?');

const minio = env('MINIO_ENDPOINT')
  ? new MinioClient({
      endPoint: env('MINIO_ENDPOINT'),
      port: Number(env('MINIO_PORT', '9000')),
      useSSL: env('MINIO_USE_SSL', 'false') === 'true',
      accessKey: env('MINIO_ACCESS_KEY'),
      secretKey: env('MINIO_SECRET_KEY'),
    })
  : null;
const BUCKET = env('MINIO_BUCKET', 'shardx-sync');

async function ensureBucket() {
  if (!minio) return;
  const exists = await minio.bucketExists(BUCKET).catch(() => false);
  if (!exists) await minio.makeBucket(BUCKET);
}

function checksum(value) {
  return crypto.createHash('sha256').update(value).digest('hex');
}

function canonicalize(value) {
  if (Array.isArray(value)) return value.map(canonicalize);
  if (value && typeof value === 'object') {
    return Object.fromEntries(
      Object.keys(value).sort().map((key) => [key, canonicalize(value[key])]),
    );
  }
  return value;
}

function itemChecksum(payload, deleted) {
  return checksum(JSON.stringify({ deleted, payload: canonicalize(payload) }));
}

function cursorToSeq(cursor) {
  if (!cursor) return 0;
  const n = Number(String(cursor).replace(/^seq:/, ''));
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : 0;
}

function auth(req, reply, done) {
  const header = req.headers.authorization || '';
  const token = header.replace(/^bearer\s+/i, '').trim();
  const supplied = Buffer.from(token);
  const expected = Buffer.from(SYNC_TOKEN);
  if (supplied.length !== expected.length || !crypto.timingSafeEqual(supplied, expected)) {
    reply.code(401).send({ error: 'unauthorized' });
    return;
  }
  done();
}

function normalizeItem(raw) {
  if (!raw || typeof raw !== 'object') throw new Error('item must be object');
  for (const key of ['kind', 'id', 'device_id', 'payload']) {
    if (raw[key] == null || raw[key] === '') throw new Error(`missing ${key}`);
  }
  const revision = Number(raw.revision || 0);
  if (!Number.isSafeInteger(revision) || revision < 0) throw new Error('invalid revision');
  const payloadValue = canonicalize(raw.payload);
  const payload = JSON.stringify(payloadValue);
  const deletedAt = raw.deleted_at ? String(raw.deleted_at) : null;
  return {
    workspace_id: WORKSPACE_ID,
    kind: String(raw.kind),
    id: String(raw.id),
    updated_at: `@${Math.floor(Date.now() / 1000)}`,
    deleted_at: deletedAt,
    device_id: String(raw.device_id),
    revision,
    payload,
    checksum: itemChecksum(payloadValue, deletedAt !== null),
  };
}

function publicItem(row) {
  if (!row) return null;
  return {
    kind: row.kind,
    id: row.id,
    updated_at: row.updated_at,
    deleted_at: row.deleted_at,
    device_id: row.device_id,
    revision: row.revision,
    checksum: row.checksum,
    payload: JSON.parse(row.payload),
  };
}

function activeLease(profileId) {
  const lease = leaseGet.get(WORKSPACE_ID, profileId);
  const now = Math.floor(Date.now() / 1000);
  return lease && lease.expires_at > now ? lease : null;
}

function conflict(item, reason, current, lease = null) {
  return {
    kind: item.kind,
    id: item.id,
    reason,
    base_revision: item.revision,
    server_revision: current?.revision || 0,
    holder_device: lease?.device_id || null,
    holder_label: lease?.device_label || null,
    expires_at: lease?.expires_at || null,
    server_item: publicItem(current),
  };
}

const app = Fastify({ logger: true, bodyLimit: MAX_BODY });
await app.register(cors, { origin: false });
await app.register(multipart, { limits: { fileSize: MAX_BODY } });

app.get('/health', async () => ({
  ok: true,
  name: 'shardx-sync-server',
  sync_protocol: SYNC_PROTOCOL,
  workspace_id: WORKSPACE_ID,
  object_storage: Boolean(minio),
  seq: maxSeqStmt.get(WORKSPACE_ID).seq,
}));

app.post('/sync/push', { preHandler: auth }, async (req, reply) => {
  const items = Array.isArray(req.body?.items) ? req.body.items : [];
  if (items.length > 2000) return reply.code(400).send({ error: 'too many items' });

  const tx = db.transaction((rows) => {
    const accepted = [];
    const conflicts = [];
    for (const row of rows) {
      const item = normalizeItem(row);
      let current = itemStmt.get(WORKSPACE_ID, item.kind, item.id);
      const lease = PROFILE_KINDS.has(item.kind) ? activeLease(item.id) : null;
      if (PROFILE_KINDS.has(item.kind) && (!lease || lease.device_id !== item.device_id)) {
        conflicts.push(conflict(item, lease ? 'locked' : 'lease_required', current, lease));
        continue;
      }

      if (current && current.checksum === item.checksum && current.deleted_at === item.deleted_at) {
        accepted.push(publicItem(current));
        continue;
      }
      if (!current) {
        if (item.revision !== 0) {
          conflicts.push(conflict(item, 'missing', null));
          continue;
        }
        insertItem.run(item);
      } else {
        if (item.revision !== current.revision) {
          conflicts.push(conflict(item, 'stale_revision', current));
          continue;
        }
        if (updateItem.run(item).changes !== 1) {
          current = itemStmt.get(WORKSPACE_ID, item.kind, item.id);
          conflicts.push(conflict(item, 'stale_revision', current));
          continue;
        }
      }
      current = itemStmt.get(WORKSPACE_ID, item.kind, item.id);
      accepted.push(publicItem(current));
    }
    return { accepted, conflicts };
  });

  let result;
  try {
    result = tx(items);
  } catch (error) {
    return reply.code(400).send({ error: error.message });
  }
  return {
    ok: result.conflicts.length === 0,
    protocol: SYNC_PROTOCOL,
    accepted: result.accepted,
    conflicts: result.conflicts,
    cursor: `seq:${maxSeqStmt.get(WORKSPACE_ID).seq}`,
  };
});

app.get('/sync/changes', { preHandler: auth }, async (req) => {
  const since = cursorToSeq(req.query?.since);
  const limit = Math.min(Math.max(Number(req.query?.limit || 500), 1), 2000);
  const rows = changesStmt.all(WORKSPACE_ID, since, limit);
  const last = rows.length ? rows[rows.length - 1].server_seq : maxSeqStmt.get(WORKSPACE_ID).seq;
  return { protocol: SYNC_PROTOCOL, items: rows.map(publicItem), cursor: `seq:${last}` };
});

app.get('/sync/item/:kind/:id', { preHandler: auth }, async (req, reply) => {
  const row = itemStmt.get(WORKSPACE_ID, req.params.kind, req.params.id);
  if (!row) return reply.code(404).send({ error: 'not found' });
  return publicItem(row);
});

app.post('/storage/:profileId/:revision', { preHandler: auth }, async (req, reply) => {
  if (!minio) return reply.code(503).send({ error: 'object storage disabled' });
  const deviceId = String(req.headers['x-shardx-device-id'] || req.query?.device_id || '').trim();
  if (!deviceId) return reply.code(400).send({ error: 'missing device id' });
  const lease = activeLease(String(req.params.profileId));
  if (!lease || lease.device_id !== deviceId) {
    return reply.code(409).send({
      error: lease ? 'locked' : 'lease_required',
      holder_device: lease?.device_id || null,
      holder_label: lease?.device_label || null,
      expires_at: lease?.expires_at || null,
    });
  }
  const part = await req.file();
  if (!part) return reply.code(400).send({ error: 'missing file' });
  const chunks = [];
  for await (const chunk of part.file) chunks.push(chunk);
  const body = Buffer.concat(chunks);
  const hash = checksum(body);
  const key = `${WORKSPACE_ID}/profiles/${req.params.profileId}/storage/${req.params.revision}.tar.gz`;
  await minio.putObject(BUCKET, key, body, body.length, { 'Content-Type': 'application/gzip', 'X-Checksum-Sha256': hash });
  objectInsert.run(WORKSPACE_ID, req.params.profileId, Number(req.params.revision), key, hash, body.length);
  return { ok: true, key, checksum: hash, size_bytes: body.length };
});

app.get('/storage/:profileId/latest', { preHandler: auth }, async (req, reply) => {
  if (!minio) return reply.code(503).send({ error: 'object storage disabled' });
  const row = objectLatest.get(WORKSPACE_ID, req.params.profileId);
  if (!row) return reply.code(404).send({ error: 'not found' });
  const url = await minio.presignedGetObject(BUCKET, row.object_key, 60 * 10);
  return { profile_id: req.params.profileId, revision: row.revision, checksum: row.checksum, size_bytes: row.size_bytes, url };
});

app.post('/lease/:profileId', { preHandler: auth }, async (req, reply) => {
  const profileId = String(req.params.profileId);
  const deviceId = String(req.body?.device_id || '').trim();
  if (!deviceId) return reply.code(400).send({ error: 'missing device_id' });
  const deviceLabel = req.body?.device_label ? String(req.body.device_label) : null;
  const ttl = Math.min(Math.max(Number(req.body?.ttl_secs || 180), 30), 3600);
  const now = Math.floor(Date.now() / 1000);
  const existing = leaseGet.get(WORKSPACE_ID, profileId);
  const active = existing && existing.expires_at > now && existing.device_id !== deviceId;
  if (active) {
    return reply.code(409).send({
      ok: false,
      held_by_me: false,
      holder_device: existing.device_id,
      holder_label: existing.device_label,
      expires_at: existing.expires_at,
    });
  }
  const expiresAt = now + ttl;
  leaseUpsert.run({
    workspace_id: WORKSPACE_ID,
    profile_id: profileId,
    device_id: deviceId,
    device_label: deviceLabel,
    expires_at: expiresAt,
  });
  return { ok: true, held_by_me: true, holder_device: deviceId, holder_label: deviceLabel, expires_at: expiresAt };
});

app.delete('/lease/:profileId', { preHandler: auth }, async (req, reply) => {
  const profileId = String(req.params.profileId);
  const deviceId = String(req.body?.device_id || '').trim();
  if (!deviceId) return reply.code(400).send({ error: 'missing device_id' });
  leaseDelete.run(WORKSPACE_ID, profileId, deviceId);
  return { ok: true };
});

app.get('/lease/:profileId', { preHandler: auth }, async (req) => {
  const profileId = String(req.params.profileId);
  const now = Math.floor(Date.now() / 1000);
  const existing = leaseGet.get(WORKSPACE_ID, profileId);
  if (!existing || existing.expires_at <= now) return { free: true };
  return {
    free: false,
    holder_device: existing.device_id,
    holder_label: existing.device_label,
    expires_at: existing.expires_at,
  };
});

await ensureBucket();
await app.listen({ host: HOST, port: PORT });
