# data-indexer — Documentation

`data-indexer` is a standalone full-text search service written in Rust. It uses
[Tantivy](https://github.com/quickwit-oss/tantivy) as the search engine and
[Axum](https://github.com/tokio-rs/axum) to expose a small HTTP API. The initial
index is built from a PostgreSQL database; afterwards documents are kept in sync
through the HTTP API.

This document explains how to run the service and how to talk to it from a
**Node.js backend**.

---

## 1. Architecture overview

```
┌────────────────┐        HTTP (JSON)        ┌─────────────────────┐
│  Node.js API   │ ────────────────────────▶ │   data-indexer      │
│  (your app)    │  upsert / delete / search │  (Rust + Axum)      │
└───────┬────────┘                           └──────────┬──────────┘
        │                                               │
        │ writes                                        │ reads on `reindex`
        ▼                                               ▼
┌────────────────┐                           ┌─────────────────────┐
│   PostgreSQL    │ ◀──────────────────────── │  ./search_index     │
│  (source data)  │     bulk reindex          │  (Tantivy on disk)  │
└────────────────┘                           └─────────────────────┘
```

- **PostgreSQL** holds the source-of-truth data (`noticias` and `infoTabs`
  tables). It is read only during a full `reindex`.
- **`./search_index`** is the on-disk Tantivy index directory. It is created by
  `reindex` and served by `serve`.
- **Your Node.js backend** is the only thing that should call the HTTP API. The
  typical pattern: whenever you create/update/delete a record in your own DB,
  you mirror that change to `data-indexer` via `/index/upsert` or
  `/index/delete`, and you query `/search` when a user searches.

---

## 2. Running the service

### Prerequisites

- Rust toolchain (edition 2024).
- A reachable PostgreSQL instance for the initial reindex.

### Build

```bash
cargo build --release
```

### Commands

The binary takes a single subcommand. If none is given, it defaults to `serve`.

| Command  | What it does                                                                 | Requires `DB_URL` |
| -------- | ---------------------------------------------------------------------------- | ----------------- |
| `reindex`| Drops `./search_index`, reads Postgres, and rebuilds the index from scratch. | Yes               |
| `serve`  | Opens the existing `./search_index` and starts the HTTP server on port 5000. | No                |

```bash
# 1. Build the index from the database (run once, or whenever you need a full rebuild)
DB_URL="postgres://user:password@localhost:5432/mydb" cargo run --release -- reindex

# 2. Start the HTTP search server
cargo run --release -- serve
```

> **Note:** `serve` requires that `./search_index` already exists. Always run
> `reindex` at least once before serving.

### Network binding

The server listens on:

```
http://127.0.0.1:5000
```

It binds to `127.0.0.1` (loopback only), so it is **not** reachable from other
machines by default. Run your Node.js backend on the same host, or place a
reverse proxy / SSH tunnel in front of it if you need remote access. To change
the bind address or port, edit `src/server.rs` (the `TcpListener::bind` call).

---

## 3. Data model

Every document is identified by a composite unique id, `uid`, derived as:

```
uid = "{tipo}:{id}"
```

This means the pair (`tipo`, `id`) must be unique. The same `id` can exist under
different `tipo` values without colliding.

### Indexed fields

| Field        | Type in index   | Stored? | Tokenized (full-text)? | Notes                                            |
| ------------ | --------------- | ------- | ---------------------- | ------------------------------------------------ |
| `id`         | string          | yes     | no (exact)             | Your record id.                                  |
| `uid`        | string          | yes     | no (exact)             | `tipo:id`, used internally for upsert/delete.    |
| `tipo`       | string          | yes     | no (exact)             | Document category, e.g. `noticia`, `info_doc`.   |
| `titulo`     | text            | yes     | yes                    | Searchable.                                      |
| `subtitulo`  | text            | yes     | yes                    | Searchable.                                      |
| `contenido`  | text            | **no**  | yes                    | Searchable but **not returned** in results.      |
| `fecha`      | string          | yes     | no (exact)             | Date as a string.                                |
| `info_title` | text            | yes     | yes                    | Only populated by `info_doc` during reindex.     |

> **Important:** `contenido` is indexed for searching but is **not stored**, so it
> will never appear in `/search` results. Only the stored fields are returned.

Full-text search runs over `titulo`, `subtitulo`, and `contenido`.

---

## 4. HTTP API reference

All request and response bodies are JSON. The base URL is
`http://127.0.0.1:5000`.

### 4.1 `POST /index/upsert`

Inserts or replaces a single document. Internally it deletes any existing
document with the same `uid` and adds the new one, then commits — so calling it
repeatedly with the same `tipo`+`id` is safe (idempotent upsert).

**Request body**

```json
{
  "id": "123",
  "tipo": "noticia",
  "titulo": "Título de la noticia",
  "subtitulo": "Un subtítulo",
  "contenido": "El cuerpo completo del documento...",
  "fecha": "2026-06-16"
}
```

All six fields are **required strings**. (If your source values are numbers or
dates, convert them to strings before sending.)

**Response** — `200 OK`

```json
{ "ok": true }
```

### 4.2 `DELETE /index/delete`

Removes a document by its `tipo`+`id`.

**Request body**

```json
{
  "id": "123",
  "tipo": "noticia"
}
```

**Response** — `200 OK`

```json
{ "ok": true }
```

> Deleting a non-existent document is **not** an error — it still returns
> `{ "ok": true }`.

### 4.3 `GET /search`

Runs a full-text query and returns the top matching documents, scored.

**Query parameters**

| Param   | Required | Default | Description                                                         |
| ------- | -------- | ------- | ------------------------------------------------------------------- |
| `q`     | yes      | —       | The query string. Searched against `titulo`, `subtitulo`, `contenido`. |
| `tipo`  | no       | —       | If set (and non-empty), restricts results to that exact `tipo`.     |
| `limit` | no       | `10`    | Maximum number of results to return.                                |

Example:

```
GET /search?q=elecciones&tipo=noticia&limit=5
```

**Response** — `200 OK`

An array of results, ordered by descending relevance `score`:

```json
[
  {
    "score": 2.31,
    "doc": {
      "id": ["123"],
      "uid": ["noticia:123"],
      "tipo": ["noticia"],
      "titulo": ["Título de la noticia"],
      "subtitulo": ["Un subtítulo"],
      "fecha": ["2026-06-16"]
    }
  }
]
```

> **Note on the `doc` shape:** Tantivy returns every stored field as an **array
> of values** (even when there is only one value). So read `doc.titulo[0]`, not
> `doc.titulo`. Also remember `contenido` is not stored and will be absent.

**Error responses**

| Status | When                                                              |
| ------ | ----------------------------------------------------------------- |
| `400`  | The query string `q` could not be parsed.                         |
| `500`  | Internal error (index reader, search, or serialization failure).  |

The body of an error is a plain-text message, not JSON.

---

## 5. Connecting from a Node.js backend

Below is a small, dependency-free client using the built-in `fetch` (Node.js 18+).
Adapt it to your framework (Express, NestJS, Fastify, Next.js route handlers, etc.).

### 5.1 A reusable client module

```js
// searchClient.js
const BASE_URL = process.env.INDEXER_URL || "http://127.0.0.1:5000";

/**
 * Insert or update a document in the search index.
 * @param {{id:string,tipo:string,titulo:string,subtitulo:string,contenido:string,fecha:string}} doc
 */
export async function upsertDocument(doc) {
  const res = await fetch(`${BASE_URL}/index/upsert`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      id: String(doc.id),
      tipo: String(doc.tipo),
      titulo: String(doc.titulo ?? ""),
      subtitulo: String(doc.subtitulo ?? ""),
      contenido: String(doc.contenido ?? ""),
      fecha: String(doc.fecha ?? ""),
    }),
  });

  if (!res.ok) {
    throw new Error(`upsert failed: ${res.status} ${await res.text()}`);
  }
  return res.json(); // { ok: true }
}

/**
 * Remove a document from the search index.
 * @param {string} tipo
 * @param {string} id
 */
export async function deleteDocument(tipo, id) {
  const res = await fetch(`${BASE_URL}/index/delete`, {
    method: "DELETE",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id: String(id), tipo: String(tipo) }),
  });

  if (!res.ok) {
    throw new Error(`delete failed: ${res.status} ${await res.text()}`);
  }
  return res.json(); // { ok: true }
}

/**
 * Search the index.
 * @param {string} q       query string
 * @param {{tipo?:string, limit?:number}} [opts]
 * @returns {Promise<Array<{score:number, doc:object}>>}
 */
export async function search(q, opts = {}) {
  const params = new URLSearchParams({ q });
  if (opts.tipo) params.set("tipo", opts.tipo);
  if (opts.limit) params.set("limit", String(opts.limit));

  const res = await fetch(`${BASE_URL}/search?${params.toString()}`);

  if (!res.ok) {
    throw new Error(`search failed: ${res.status} ${await res.text()}`);
  }
  return res.json();
}

/** Helper: flatten Tantivy's array-valued fields into plain scalars. */
export function flattenDoc(result) {
  const flat = {};
  for (const [key, value] of Object.entries(result.doc)) {
    flat[key] = Array.isArray(value) ? value[0] : value;
  }
  return { score: result.score, ...flat };
}
```

### 5.2 Using it inside an Express route

```js
// app.js
import express from "express";
import { upsertDocument, deleteDocument, search, flattenDoc } from "./searchClient.js";

const app = express();
app.use(express.json());

// Mirror a write in your own DB to the index
app.post("/articles", async (req, res) => {
  const article = await db.createArticle(req.body); // your own persistence

  await upsertDocument({
    id: article.id,
    tipo: "noticia",
    titulo: article.titulo,
    subtitulo: article.subtitulo,
    contenido: article.contenido,
    fecha: article.fecha,
  });

  res.status(201).json(article);
});

// Mirror a delete
app.delete("/articles/:id", async (req, res) => {
  await db.deleteArticle(req.params.id);
  await deleteDocument("noticia", req.params.id);
  res.status(204).end();
});

// Expose search to the frontend
app.get("/search", async (req, res) => {
  try {
    const results = await search(req.query.q, {
      tipo: req.query.tipo,
      limit: req.query.limit ? Number(req.query.limit) : undefined,
    });
    res.json(results.map(flattenDoc));
  } catch (err) {
    res.status(502).json({ error: String(err.message) });
  }
});

app.listen(3000, () => console.log("Node backend on :3000"));
```

### 5.3 Keeping the index in sync — recommended pattern

1. **Initial load:** run `data-indexer reindex` once (reads Postgres directly).
2. **On create/update** in your Node backend: call `upsertDocument(...)` after
   your DB write succeeds.
3. **On delete:** call `deleteDocument(tipo, id)` after your DB delete succeeds.
4. **Periodic / recovery rebuild:** if the index and DB drift, re-run `reindex`
   (it wipes and rebuilds `./search_index`). Restart `serve` afterwards so it
   re-opens the fresh index.

> Because each `upsert`/`delete` commits immediately, prefer to **batch**
> high-volume writes or run them asynchronously (e.g. a queue/worker) rather than
> one synchronous HTTP call per record in a tight loop.

---

## 6. Operational notes & gotchas

- **`serve` must run after `reindex`.** Starting `serve` without an existing
  `./search_index` will fail to open the directory.
- **Single writer.** The server holds one Tantivy `IndexWriter` behind a mutex.
  Concurrent upserts/deletes are serialized; that's fine for moderate volume but
  not for high-throughput bulk writes — use `reindex` for bulk loads.
- **`contenido` is searchable but not returned.** Fetch the full body from your
  own database using the `id`/`tipo` from the search result.
- **Field values are arrays in results.** Use the `flattenDoc` helper above.
- **No authentication.** The API is unauthenticated and binds to loopback. Do
  not expose it directly to the public internet — front it with your Node
  backend or a proxy that enforces auth.
- **All fields are strings.** Convert numbers/dates to strings before sending to
  `/index/upsert`.
- **Errors are plain text.** Non-2xx responses from `/search` return a text
  message body, so read with `await res.text()` when handling failures.

---

## 7. Environment variables

| Variable      | Used by   | Description                                                        |
| ------------- | --------- | ----------------------------------------------------------------- |
| `DB_URL`      | `reindex` | PostgreSQL connection string. Required for the initial reindex.   |
| `INDEXER_URL` | Node app  | (Your convention) base URL of the running indexer, e.g. `http://127.0.0.1:5000`. |

The PostgreSQL schema expected by `reindex`:

- Table `noticias` with columns `id`, `titulo`, `subtitulo`, `contenido`, `fecha`.
- Table `infoTabs` with columns `id`, `infotitle`, `title`, `contenido`,
  `subtitle`, `created_at`.
