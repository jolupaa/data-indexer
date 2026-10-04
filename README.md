# data-indexer

A small, fast full-text search service written in Rust
([Tantivy](https://github.com/quickwit-oss/tantivy) +
[Axum](https://github.com/tokio-rs/axum)). It builds an initial search index
from a PostgreSQL database and then exposes a tiny HTTP API to upsert, delete,
and search documents — designed to sit behind a Node.js (or any) backend.

> 📖 Full API reference and Node.js integration guide:
> **[documentation.md](./documentation.md)**

## Features

- Full-text search over `titulo`, `subtitulo`, `info_title`, and `contenido`,
  insensitive to case and accents and to Spanish singular/plural
  (`clase` ≈ `clases`, `actualización` ≈ `actualizaciones`).
- Optional filtering by document type (`tipo`), `limit` and `offset` pagination.
- Idempotent upsert keyed by a composite `uid` (`tipo:id`), single or batched.
- Safe bulk `reindex` straight from PostgreSQL: the rebuild is applied in a
  single commit, so a failure half-way leaves the previous index intact.

## Requirements

- Rust toolchain (edition 2024)
- PostgreSQL (only needed for the initial `reindex`)

## Quick start

```bash
# Build
cargo build --release

# 1. Build the index from your database (run once)
DB_URL="postgres://user:password@localhost:5432/mydb" cargo run --release -- reindex

# 2. Start the HTTP server on http://127.0.0.1:5000
cargo run --release -- serve
```

> **Upgrading from an earlier version?** Schema 3 added the `acl` and `thread`
> fields: stop `serve`, run the new `reindex` once and start `serve` again.
> `serve` refuses to open an index built by an older version and tells you so,
> and an older binary refuses a schema-3 index too, so rolling back means
> emptying `INDEX_DIR` and running the older `reindex`. See
> [documentation.md](./documentation.md#upgrading).

## Commands

| Command   | Description                                                                 |
| --------- | --------------------------------------------------------------------------- |
| `reindex` | Rebuilds the index from PostgreSQL. Needs `DB_URL`. Stop `serve` first.     |
| `serve`   | Serves the existing index over HTTP on `127.0.0.1:5000` (default command).  |

| Variable    | Default           | Description                          |
| ----------- | ----------------- | ------------------------------------ |
| `DB_URL`    | —                 | PostgreSQL URL (only for `reindex`). |
| `INDEX_DIR` | `./search_index`  | Where the index lives.               |
| `BIND_ADDR` | `127.0.0.1:5000`  | Address `serve` listens on.          |

## HTTP API

| Method   | Path                  | Purpose                                         |
| -------- | --------------------- | ----------------------------------------------- |
| `POST`   | `/index/upsert`       | Insert or replace a document.                   |
| `POST`   | `/index/upsert/batch` | Insert or replace many documents in one commit. |
| `DELETE` | `/index/delete`       | Delete a document by `tipo` + `id`.             |
| `GET`    | `/search`             | Full-text search (`?q=&tipo=&limit=&offset=`).  |
| `GET`    | `/health`             | Liveness check: `{ "ok": true, "docs": N }`.    |

### Example

```bash
# Upsert
curl -X POST http://127.0.0.1:5000/index/upsert \
  -H 'Content-Type: application/json' \
  -d '{"id":"123","tipo":"noticia","titulo":"Hola","subtitulo":"sub","contenido":"cuerpo","fecha":"2026-06-16"}'

# Search
curl "http://127.0.0.1:5000/search?q=hola&tipo=noticia&limit=5"

# Delete
curl -X DELETE http://127.0.0.1:5000/index/delete \
  -H 'Content-Type: application/json' \
  -d '{"id":"123","tipo":"noticia"}'
```

## Connecting from Node.js

```js
const BASE_URL = process.env.INDEXER_URL || "http://127.0.0.1:5000";

export async function search(q, { tipo, limit } = {}) {
  const params = new URLSearchParams({ q });
  if (tipo) params.set("tipo", tipo);
  if (limit) params.set("limit", String(limit));
  const res = await fetch(`${BASE_URL}/search?${params}`);
  if (!res.ok) throw new Error(`search failed: ${res.status} ${await res.text()}`);
  return res.json(); // [{ score, doc }]
}
```

See **[documentation.md](./documentation.md)** for the full reusable client,
Express integration, the index-sync pattern, and important gotchas (e.g. result
fields are array-valued and `contenido` is searchable but not returned).

## Notes

- The server binds to `127.0.0.1` by default and has **no authentication** —
  keep it behind your backend or a proxy.
- `serve` requires that `reindex` has been run at least once.
- All document fields are strings.
- `limit` is capped at 1000, `offset` at 10000 and `q` at 1000 characters.

## License

[MIT](./LICENSE) © Jose Luis de Pina Arenas
