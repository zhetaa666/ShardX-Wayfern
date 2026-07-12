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
`);

const upsertItem = db.prepare(`
INSERT INTO sync_items(workspace_id, kind, id, updated_at, deleted_at, device_id, revision, payload, checksum)
VALUES(@workspace_id, @kind, @id, @updated_at, @deleted_at, @device_id, @revision, @payload, @checksum)
ON CONFLICT(workspace_id, kind, id) DO UPDATE SET
  updated_at=excluded.updated_at,
  deleted_at=excluded.deleted_at,
  device_id=excluded.device_id,
  revision=CASE WHEN excluded.revision > sync_items.revision THEN excluded.revision ELSE sync_items.revision + 1 END,
  payload=excluded.payload,
  checksum=excluded.checksum,
  server_seq=(SELECT COALESCE(MAX(server_seq), 0) + 1 FROM sync_items)
WHERE excluded.updated_at >= sync_items.updated_at
`);
const changesStmt = db.prepare(`
SELECT kind,id,updated_at,deleted_at,device_id,revision,payload,server_seq
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

function cursorToSeq(cursor) {
  if (!cursor) return 0;
  const n = Number(String(cursor).replace(/^seq:/, ''));
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : 0;
}

function auth(req, reply, done) {
  const header = req.headers.authorization || '';
  const token = header.replace(/^bearer\s+/i, '').trim();
  if (!crypto.timingSafeEqual(Buffer.from(token.padEnd(SYNC_TOKEN.length)), Buffer.from(SYNC_TOKEN.padEnd(token.length)))) {
    reply.code(401).send({ error: 'unauthorized' });
    return;
  }
  done();
}

function normalizeItem(raw) {
  if (!raw || typeof raw !== 'object') throw new Error('item must be object');
  for (const key of ['kind', 'id', 'updated_at', 'device_id', 'payload']) {
    if (raw[key] == null || raw[key] === '') throw new Error(`missing ${key}`);
  }
  const payload = JSON.stringify(raw.payload);
  return {
    workspace_id: WORKSPACE_ID,
    kind: String(raw.kind),
    id: String(raw.id),
    updated_at: String(raw.updated_at),
    deleted_at: raw.deleted_at ? String(raw.deleted_at) : null,
    device_id: String(raw.device_id),
    revision: Number(raw.revision || 0),
    payload,
    checksum: checksum(payload),
  };
}

const app = Fastify({ logger: true, bodyLimit: MAX_BODY });
await app.register(cors, { origin: false });
await app.register(multipart, { limits: { fileSize: MAX_BODY } });

app.get('/health', async () => ({
  ok: true,
  name: 'shardx-sync-server',
  workspace_id: WORKSPACE_ID,
  object_storage: Boolean(minio),
  seq: maxSeqStmt.get(WORKSPACE_ID).seq,
}));

app.post('/sync/push', { preHandler: auth }, async (req) => {
  const items = Array.isArray(req.body?.items) ? req.body.items : [];
  const tx = db.transaction((rows) => {
    let accepted = 0;
    for (const row of rows) {
      const item = normalizeItem(row);
      upsertItem.run(item);
      accepted += 1;
    }
    return accepted;
  });
  const accepted = tx(items);
  return { ok: true, accepted, cursor: `seq:${maxSeqStmt.get(WORKSPACE_ID).seq}` };
});

app.get('/sync/changes', { preHandler: auth }, async (req) => {
  const since = cursorToSeq(req.query?.since);
  const limit = Math.min(Number(req.query?.limit || 500), 2000);
  const rows = changesStmt.all(WORKSPACE_ID, since, limit);
  const items = rows.map((r) => ({
    kind: r.kind,
    id: r.id,
    updated_at: r.updated_at,
    deleted_at: r.deleted_at,
    device_id: r.device_id,
    revision: r.revision,
    payload: JSON.parse(r.payload),
  }));
  const last = rows.length ? rows[rows.length - 1].server_seq : maxSeqStmt.get(WORKSPACE_ID).seq;
  return { items, cursor: `seq:${last}` };
});

app.get('/sync/item/:kind/:id', { preHandler: auth }, async (req, reply) => {
  const row = itemStmt.get(WORKSPACE_ID, req.params.kind, req.params.id);
  if (!row) return reply.code(404).send({ error: 'not found' });
  return {
    kind: row.kind,
    id: row.id,
    updated_at: row.updated_at,
    deleted_at: row.deleted_at,
    device_id: row.device_id,
    revision: row.revision,
    payload: JSON.parse(row.payload),
  };
});

app.post('/storage/:profileId/:revision', { preHandler: auth }, async (req, reply) => {
  if (!minio) return reply.code(503).send({ error: 'object storage disabled' });
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

await ensureBucket();
await app.listen({ host: HOST, port: PORT });
