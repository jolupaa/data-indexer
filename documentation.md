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
- **`./search_index`** is the on-disk Tantivy index directory (configurable
  with `INDEX_DIR`). It is created by `reindex` and served by `serve`.
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

| Command  | What it does                                                                        | Requires `DB_URL` |
| -------- | ----------------------------------------------------------------------------------- | ----------------- |
| `reindex`| Reads Postgres and rebuilds the index from scratch, committing it all at once.      | Yes               |
| `serve`  | Opens the existing index and starts the HTTP server (default `127.0.0.1:5000`).     | No                |
| `help`   | Prints usage.                                                                       | No                |

An unknown command prints usage and exits with status `2`.

```bash
# 1. Build the index from the database (run once, or whenever you need a full rebuild)
DB_URL="postgres://user:password@localhost:5432/mydb" cargo run --release -- reindex

# 2. Start the HTTP search server
cargo run --release -- serve
```

> **Note:** `serve` requires that the index already exists. Always run
> `reindex` at least once before serving.

#### How `reindex` behaves

- The index is rebuilt in place and all changes land in a single commit: until
  that commit, searches keep seeing the previous contents, and if PostgreSQL
  fails half-way the previous index is left untouched. The directory itself is
  never moved or deleted, so `INDEX_DIR` can be a symlink or a mount point
  (e.g. a Docker volume).
- It **refuses to run while `serve` is using the same index** (it takes the
  index write lock). A running server would keep writing into the replaced
  index and corrupt it. Stop `serve`, run `reindex`, start `serve` again.
- It refuses to use a directory that is not empty and does not look like an
  index (no `meta.json`), so a mistyped `INDEX_DIR` can't clobber other data.
- `NULL` columns are indexed as empty strings.

#### Upgrading

The index stores the name of the text analyzer it was built with. When the
analyzer changes between versions (as it did when accent handling and Spanish
plurals were fixed), `serve` refuses to open an index built by the old version
and asks you to run `reindex`. Run it once after upgrading. In that case
`reindex` discards the old index before loading from PostgreSQL (the old one
is unusable by the new version anyway).

#### Shutdown

`serve` stops gracefully on `Ctrl+C` or `SIGTERM`: it stops accepting
connections and lets in-flight requests (and their commits) finish.

### Network binding

By default the server listens on:

```
http://127.0.0.1:5000
```

It binds to `127.0.0.1` (loopback only), so it is **not** reachable from other
machines by default. Run your Node.js backend on the same host, or place a
reverse proxy / SSH tunnel in front of it if you need remote access. To change
the bind address or port, set `BIND_ADDR` (e.g. `BIND_ADDR=0.0.0.0:8080`) —
remember there is no authentication.

---

## 3. Data model

Every document is identified by a composite unique id, `uid`, derived as:

```
uid = "{tipo}:{id}"
```

This means the pair (`tipo`, `id`) must be unique. The same `id` can exist under
different `tipo` values without colliding. `tipo` may not contain `:` (otherwise
two different pairs could produce the same `uid`), neither `tipo` nor `id` may
be empty, and each may be at most 1024 bytes. Leading/trailing spaces in `tipo`
are trimmed.

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
| `info_title` | text            | yes     | yes                    | Used by `info_doc`; omitted when empty.          |

> **Important:** `contenido` is indexed for searching but is **not stored**, so it
> will never appear in `/search` results. Only the stored fields are returned.

Full-text search runs over `titulo`, `subtitulo`, `info_title`, and `contenido`.

### Text analysis

Searchable text goes through the same analyzer at index and at query time:

1. Unicode NFC normalization (so decomposed accents, common in text extracted
   from PDFs, don't split words).
2. Split into words on anything that is not a letter or digit; words longer
   than 40 bytes are dropped.
3. Lowercase and fold accents (`Actualización` → `actualizacion`).
4. Reduce Spanish singular/plural to a shared term (`noticias` → `noticia`,
   `clase`/`clases` → `clas`, `luz`/`luces` → `luc`). The resulting term is not
   always a real word; what matters is that both forms match.

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

`id` and `tipo` are **required strings**. `titulo`, `subtitulo`, `contenido`,
`fecha` and `info_title` are optional strings: missing or `null` means `""`.
(If your source values are numbers or dates, convert them to strings before
sending.) Upserting always replaces the whole document, so send every field you
want to keep — including `info_title` for `info_doc` documents.

The request body may be up to 64 MB.

**Response** — `200 OK`

```json
{ "ok": true }
```

`400` if `tipo`/`id` are empty or longer than 1024 bytes, or `tipo` contains
`:`; `422` if the JSON does not have the expected shape.

### 4.2 `POST /index/upsert/batch`

Same as `/index/upsert` but takes a JSON **array** of documents and applies them
all in a single commit, which is much cheaper than one request per document.
It is all-or-nothing: if any document is invalid, nothing is written. If the
same `tipo`+`id` appears more than once, the last one wins.

**Response** — `200 OK`

```json
{ "ok": true, "indexed": 3 }
```

### 4.3 `DELETE /index/delete`

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

### 4.4 `GET /search`

Runs a full-text query and returns the top matching documents, scored.

**Query parameters**

| Param    | Required | Default | Description                                                                          |
| -------- | -------- | ------- | ------------------------------------------------------------------------------------ |
| `q`      | yes      | —       | The query string (max 1000 characters). Searched against `titulo`, `subtitulo`, `info_title`, `contenido`. |
| `tipo`   | no       | —       | If set (and non-empty), restricts results to that exact `tipo`.                      |
| `limit`  | no       | `10`    | Maximum number of results to return. Values above `1000` are capped; `0` returns `[]`. |
| `offset` | no       | `0`     | Number of top results to skip, for pagination (max `10000`).                          |

Example:

```
GET /search?q=elecciones&tipo=noticia&limit=5&offset=10
```

**Query syntax.** `q` accepts Tantivy's query syntax: several words match
documents containing *any* of them (best matches first), `"exact phrase"`,
`+required`, `-excluded`, `AND` / `OR` / `NOT` (uppercase), grouping with
parentheses and `field:value` (e.g. `titulo:elecciones`). `*` matches every
document, which combined with `tipo` lists a whole category. Case, accents and
singular/plural don't matter.

If `q` is not valid syntax — e.g. `12:30`, a URL, or an unclosed quote — or
nests parentheses more than 8 levels deep, it is searched as plain words
instead of failing.

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
> `doc.titulo`. Also remember `contenido` is not stored and will be absent, and
> `info_title` is only present on documents that have one.

**Error responses**

| Status | When                                                                       |
| ------ | -------------------------------------------------------------------------- |
| `400`  | Missing `q`, `q` longer than 1000 characters, or invalid `limit`/`offset`. |
| `500`  | Internal error (index read or search failure).                             |

The body of an error is a plain-text message, not JSON.

### 4.5 `GET /health`

Returns `200 OK` with the number of documents currently searchable:

```json
{ "ok": true, "docs": 1234 }
```

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
 * @param {{id:string,tipo:string,titulo?:string,subtitulo?:string,contenido?:string,fecha?:string,info_title?:string}} doc
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
      info_title: String(doc.info_title ?? ""),
    }),
  });

  if (!res.ok) {
    throw new Error(`upsert failed: ${res.status} ${await res.text()}`);
  }
  return res.json(); // { ok: true }
}

/**
 * Insert or update many documents with a single commit (all-or-nothing).
 * @param {Array<object>} docs same shape as for upsertDocument
 */
export async function upsertDocuments(docs) {
  const res = await fetch(`${BASE_URL}/index/upsert/batch`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(
      docs.map((doc) => ({
        id: String(doc.id),
        tipo: String(doc.tipo),
        titulo: String(doc.titulo ?? ""),
        subtitulo: String(doc.subtitulo ?? ""),
        contenido: String(doc.contenido ?? ""),
        fecha: String(doc.fecha ?? ""),
        info_title: String(doc.info_title ?? ""),
      })),
    ),
  });

  if (!res.ok) {
    throw new Error(`batch upsert failed: ${res.status} ${await res.text()}`);
  }
  return res.json(); // { ok: true, indexed: n }
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
 * @param {{tipo?:string, limit?:number, offset?:number}} [opts]
 * @returns {Promise<Array<{score:number, doc:object}>>}
 */
export async function search(q, opts = {}) {
  const params = new URLSearchParams({ q });
  if (opts.tipo) params.set("tipo", opts.tipo);
  if (opts.limit) params.set("limit", String(opts.limit));
  if (opts.offset) params.set("offset", String(opts.offset));

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
      offset: req.query.offset ? Number(req.query.offset) : undefined,
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
4. **Periodic / recovery rebuild:** if the index and DB drift, stop `serve`,
   run `reindex`, and start `serve` again. `reindex` refuses to run while
   `serve` holds the index, because the server would keep writing into the
   replaced index. Writes your backend sends while `serve` is down will fail,
   so queue or retry them (or rely on the rebuild picking them up from the DB).

> Because each `upsert`/`delete` commits immediately, prefer
> `upsertDocuments(...)` (`/index/upsert/batch`) for high-volume writes, or run
> them asynchronously (e.g. a queue/worker) rather than one synchronous HTTP
> call per record in a tight loop.

---

## 6. Operational notes & gotchas

- **`serve` must run after `reindex`.** Starting `serve` without an existing
  index fails with a message telling you to run `reindex`.
- **Don't run `reindex` while `serve` is running** on the same index; it
  refuses to. Stop the server first.
- **Single writer.** The server holds one Tantivy `IndexWriter` behind a mutex.
  Concurrent upserts/deletes are serialized; that's fine for moderate volume.
  Use `/index/upsert/batch` or `reindex` for bulk loads.
- **Writes are visible immediately.** A successful upsert/delete is already
  reflected in the next `/search`.
- **`contenido` is searchable but not returned.** Fetch the full body from your
  own database using the `id`/`tipo` from the search result.
- **Field values are arrays in results.** Use the `flattenDoc` helper above.
- **No authentication.** The API is unauthenticated and binds to loopback. Do
  not expose it directly to the public internet — front it with your Node
  backend or a proxy that enforces auth.
- **All fields are strings.** Convert numbers/dates to strings before sending to
  `/index/upsert`.
- **Errors are plain text.** Non-2xx responses return a text message body, so
  read with `await res.text()` when handling failures.
- **Limits.** `limit` ≤ 1000, `offset` ≤ 10000, `q` ≤ 1000 characters, request
  bodies ≤ 64 MB.

---

## 7. Environment variables

| Variable      | Used by            | Description                                                        |
| ------------- | ------------------ | ----------------------------------------------------------------- |
| `DB_URL`      | `reindex`          | PostgreSQL connection string. Required for the initial reindex.   |
| `INDEX_DIR`   | `reindex`, `serve` | Index directory. Default `./search_index`.                        |
| `BIND_ADDR`   | `serve`            | Listen address. Default `127.0.0.1:5000`.                         |
| `INDEXER_URL` | Node app  | (Your convention) base URL of the running indexer, e.g. `http://127.0.0.1:5000`. |

The PostgreSQL schema expected by `reindex`:

- Table `noticias` with columns `id`, `titulo`, `subtitulo`, `extracted_text`,
  `fecha`.
- Table `infoTabs` (unquoted, so PostgreSQL resolves it as `infotabs`) with
  columns `id`, `infotitle`, `title`, `extracted_text`, `subtitle`, `created_at`.

`extracted_text` is indexed as `contenido`. Any column except `id` may be
`NULL`.
