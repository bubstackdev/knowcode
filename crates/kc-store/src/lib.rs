//! The per-repo index: one SQLite file with FTS5, holding extracted symbols, references and relations.
//!
//! References are stored by name and resolved at query time (see [`query`]), so re-extracting one file never
//! invalidates rows from other files.

pub mod answer;
pub mod ansible;
pub mod bus;
pub mod ci;
mod cfml;
mod csharp;
pub mod content;
pub mod db;
pub mod di;
pub mod identity;
pub mod limits;
pub mod locate;
pub mod manifests;
pub mod ngrx;
pub mod map;
pub mod query;
pub mod rank;
pub mod scip;
mod scope;
pub mod skeleton;
pub mod sql;
pub mod terraform;
pub mod trace;
pub mod unsupported;
mod py;
mod ts;

pub use ts::is_ts_path;

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use kc_extract::{FileFacts, Lang};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// Bumped when query-time resolution changes, so cached derived results (file edges, hotspots, maps) are recomputed.
pub const RESOLVER_VERSION: u32 = 20;

/// Bumped when the schema changes. An index with an older version is rebuilt from scratch; one with a newer version
/// (written by a newer build sharing the cache) is refused, not dropped: see [`Store::open`].
pub const SCHEMA_VERSION: u32 = 21;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    id                INTEGER PRIMARY KEY,
    path              TEXT NOT NULL UNIQUE,
    lang              TEXT NOT NULL,
    mtime_ns          INTEGER NOT NULL,
    size              INTEGER NOT NULL,
    hash              TEXT NOT NULL,
    extractor_version INTEGER NOT NULL,
    parse_errors      INTEGER NOT NULL,
    script            INTEGER NOT NULL DEFAULT 0,
    lines             INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS symbols (
    id          INTEGER PRIMARY KEY,
    file_id     INTEGER NOT NULL,
    parent_id   INTEGER,
    name        TEXT NOT NULL COLLATE NOCASE,
    kind        TEXT NOT NULL,
    container   TEXT,
    signature   TEXT NOT NULL,
    doc         TEXT,
    attrs       TEXT NOT NULL,
    tags        TEXT NOT NULL,
    start_line  INTEGER NOT NULL,
    end_line    INTEGER NOT NULL,
    start_byte  INTEGER NOT NULL,
    end_byte    INTEGER NOT NULL,
    complexity  INTEGER NOT NULL,
    type_name   TEXT,
    nesting     INTEGER NOT NULL DEFAULT 0,
    -- Arguments a call passes (C#): fewest and most, NULL most for `params`; NULL fewest when unknown.
    min_args    INTEGER,
    max_args    INTEGER,
    -- Per parameter, the input types of the delegate it takes, as JSON (C#).
    lambda_types TEXT,
    -- Per parameter, its declared type, encoded, or null for a type parameter, as JSON (C#).
    param_types TEXT,
    -- Health biomarkers of a function (C#, TypeScript): its bumps as `first-last` line ranges, comma-separated (NULL
    -- for none), and the most &&/|| operators in one branch condition with that condition's line.
    bumps       TEXT,
    cond_ops    INTEGER NOT NULL DEFAULT 0,
    cond_line   INTEGER
);
CREATE INDEX IF NOT EXISTS symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS symbols_file ON symbols(file_id);
CREATE INDEX IF NOT EXISTS symbols_container ON symbols(container);

-- Name tokens (camelCase split) for fuzzy lookup; rowid = symbols.id.
CREATE VIRTUAL TABLE IF NOT EXISTS symbols_fts USING fts5(tokens, tokenize = 'unicode61');

-- Concept search (see `locate`): one row per file, rowid = files.id, holding the stemmed words of its path, its
-- symbols' names and doc comments, and the names their bodies reference; `len` is the word count. Contentless: only
-- the inverted index is stored. `files_fts_terms` reads term frequencies back per file and column.
CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(path, names, docs, refs, len UNINDEXED, tokenize = 'unicode61',
    content = '', contentless_delete = 1, contentless_unindexed = 1);
CREATE VIRTUAL TABLE IF NOT EXISTS files_fts_terms USING fts5vocab(files_fts, instance);

CREATE TABLE IF NOT EXISTS refs (
    file_id  INTEGER NOT NULL,
    from_id  INTEGER,
    name     TEXT NOT NULL,
    receiver TEXT,
    receiver_type TEXT,
    kind     TEXT NOT NULL,
    line     INTEGER NOT NULL,
    -- 1-based byte column of the name on its line, 0 when not known: each occurrence is its own row.
    col      INTEGER NOT NULL DEFAULT 0,
    -- Arguments a call passes (C#).
    args     INTEGER,
    -- Per argument, its type as the call site tells it, encoded, or null, as JSON (C#).
    arg_types TEXT,
    -- 1 for a member reference that assigns the member (C#: `x.P = …`, `new T { P = … }`).
    assigns  INTEGER NOT NULL DEFAULT 0,
    -- How a type reference or creation uses the type when that isn't a plain use (C#): `type_argument`, `typeof`,
    -- `nameof`, `constraint`, `target_typed`.
    usage    TEXT
);
CREATE INDEX IF NOT EXISTS refs_name ON refs(name);
CREATE INDEX IF NOT EXISTS refs_from ON refs(from_id);
CREATE INDEX IF NOT EXISTS refs_file ON refs(file_id);

CREATE TABLE IF NOT EXISTS relations (
    file_id     INTEGER NOT NULL,
    from_id     INTEGER NOT NULL,
    target      TEXT NOT NULL,
    target_name TEXT NOT NULL,
    kind        TEXT NOT NULL,
    line        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS relations_target ON relations(target_name);
CREATE INDEX IF NOT EXISTS relations_from ON relations(from_id);
CREATE INDEX IF NOT EXISTS relations_file ON relations(file_id);

CREATE TABLE IF NOT EXISTS imports (
    file_id INTEGER NOT NULL,
    module  TEXT NOT NULL,
    line    INTEGER NOT NULL
);

-- C# `global using` directives: imported by every file of the declaring file's project.
CREATE TABLE IF NOT EXISTS global_usings (
    file_id   INTEGER NOT NULL,
    namespace TEXT NOT NULL,
    alias     TEXT,
    is_static INTEGER NOT NULL,
    line      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS global_usings_file ON global_usings(file_id);
CREATE INDEX IF NOT EXISTS imports_module ON imports(module);
CREATE INDEX IF NOT EXISTS imports_file ON imports(file_id);

-- Names a TypeScript file takes from modules (`import { name as local } from 'module'`; `name` is `*` for a namespace
-- import, `default` for a default one), and with `export` set, names it passes on (`export { name } from`, and
-- `export * from` as local and name `*`).
CREATE TABLE IF NOT EXISTS import_bindings (
    file_id INTEGER NOT NULL,
    local   TEXT NOT NULL,
    name    TEXT NOT NULL,
    module  TEXT NOT NULL,
    export  INTEGER NOT NULL,
    line    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS import_bindings_file ON import_bindings(file_id);

-- A TypeScript project configuration's module resolution settings (`kc_extract::TsConfig`, as JSON).
CREATE TABLE IF NOT EXISTS ts_configs (
    file_id INTEGER NOT NULL,
    config  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS ts_configs_file ON ts_configs(file_id);

-- HTTP routes the code serves (`server`: controller actions, minimal API endpoints) or sends requests to (`client`:
-- request contracts, service gateway DTOs and attribute-declared request classes, and methods making requests: gateway
-- calls by service and path, HTTP client calls with a URL), and service names it hosts (`service`). The full path is
-- `path` joined with the value of the constant `path_ref`, when there is one (a request built in code, and a minimal
-- API endpoint, encode constants in `path` instead: `{=Name}`, and an endpoint on a helper's parameter starts with
-- `{@Helper#i}`); the service is `service`, or the value of the constant `service_ref`. `group` rows are route groups
-- passed to helpers (`service` names the helper and parameter, `path` the prefix; `verb` is `?` when the prefix can't
-- be known and `path` says why); registrations whose routes can't be placed are kept in `unsupported` (area `http`);
-- `prefix` rows are classes' `[Route]` templates and `conventional` rows conventional route
-- templates, placing actions whose path starts `{^Action}`; `operation` rows are calls to generated API clients
-- (`path` the operation, `service` the tag). symbol_id is NULL in top-level statements. See `kc_extract::HttpRoute`.
CREATE TABLE IF NOT EXISTS http_routes (
    file_id     INTEGER NOT NULL,
    symbol_id   INTEGER,
    side        TEXT NOT NULL,
    verb        TEXT NOT NULL,
    path        TEXT NOT NULL,
    path_ref    TEXT,
    service     TEXT,
    service_ref TEXT,
    line        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS http_routes_file ON http_routes(file_id);
CREATE INDEX IF NOT EXISTS http_routes_symbol ON http_routes(symbol_id);

-- Angular facts: `selector`, `template` (a component's templateUrl as written), `route` (path within its file; extra
-- is the component, or `loads:<Name>` for a lazy module, `loads:<routes>@<module>` for a routes constant), `renders` (a custom element used in a template) and `attr`
-- (an attribute that may select a directive). For those two, symbol_id is the component for an inline template, NULL
-- in an .html file, and extra is the element's tag and attribute names, space-separated.
CREATE TABLE IF NOT EXISTS ng_facts (
    file_id   INTEGER NOT NULL,
    symbol_id INTEGER,
    kind      TEXT NOT NULL,
    value     TEXT NOT NULL,
    extra     TEXT,
    line      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ng_facts_file ON ng_facts(file_id);
CREATE INDEX IF NOT EXISTS ng_facts_kind_value ON ng_facts(kind, value);
CREATE INDEX IF NOT EXISTS ng_facts_symbol ON ng_facts(symbol_id);

-- Entity Framework facts: `dbset` (symbol_id is the DbSet property; name its name), `table` (entity -> table name,
-- `schema.table` when given; symbol_id is the [Table] class or the method calling ToTable), `set` (a `Set<T>()` access
-- from symbol_id), `procedure` (a stored procedure called from symbol_id; entity empty), `query` (symbol_id queries
-- the member `name`: `from o in ctx.Orders`), `owned_table` (a `ToTable` on an owned type's builder: entity is
-- `Owner#Navigation`, resolved at query time). See `db`.
CREATE TABLE IF NOT EXISTS db_facts (
    file_id   INTEGER NOT NULL,
    symbol_id INTEGER,
    kind      TEXT NOT NULL,
    entity    TEXT NOT NULL,
    name      TEXT NOT NULL,
    line      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS db_facts_file ON db_facts(file_id);

-- Hosts the repo serves (`serves`: launch profiles, Kestrel, compose services built here, Aspire projects, ingress and
-- proxy host rules, `target` the backend service a proxy sends to) or its clients send to (`configures`: URLs in
-- config, `target` the key; typed HTTP clients' base addresses, `target` the client class). Only hosts are stored.
-- See `kc_extract::HostFact`.
CREATE TABLE IF NOT EXISTS host_facts (
    file_id INTEGER NOT NULL,
    role    TEXT NOT NULL,
    kind    TEXT NOT NULL,
    host    TEXT NOT NULL,
    target  TEXT,
    line    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS host_facts_file ON host_facts(file_id);

-- Named string constants, for resolving `http_routes.path_ref`.
CREATE TABLE IF NOT EXISTS constants (
    file_id   INTEGER NOT NULL,
    qualified TEXT NOT NULL,
    value     TEXT NOT NULL,
    line      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS constants_file ON constants(file_id);
CREATE INDEX IF NOT EXISTS constants_qualified ON constants(qualified);

-- Dependency-injection registrations. `service` is as written (`IValidator<Order>`), `service_name` its bare name;
-- both are NULL for `AsImplementedInterfaces` (every interface the implementation declares). `implementation` is the
-- concrete type's name when known, or an assembly scan's name filter (`*Service`). `kind` is `type`, `factory`,
-- `instance` or `scan`; `lifetime` is NULL for the container's default. `made` is what a factory returns when only the
-- repo's declarations tell (an encoded type expression), resolved into `implementation` (and, with `service_made`,
-- into the service too) after each sync; see `di::resolve_made`. `mode` is NULL for a plain registration, `try` for
-- one made only when the service has none yet (`TryAdd*`), `enumerable` for one added unless the same implementation
-- is (`TryAddEnumerable`) and `replace` for one replacing the service's first (`Replace`).
CREATE TABLE IF NOT EXISTS di_bindings (
    file_id        INTEGER NOT NULL,
    service        TEXT,
    service_name   TEXT,
    implementation TEXT,
    lifetime       TEXT,
    kind           TEXT NOT NULL,
    line           INTEGER NOT NULL,
    made           TEXT,
    service_made   INTEGER NOT NULL DEFAULT 0,
    mode           TEXT
);
CREATE INDEX IF NOT EXISTS di_bindings_file ON di_bindings(file_id);
CREATE INDEX IF NOT EXISTS di_bindings_service ON di_bindings(service_name);
CREATE INDEX IF NOT EXISTS di_bindings_implementation ON di_bindings(implementation);

-- Message-bus facts (C#): `message` (a message type; `message` is its dotted name, `scopes` its namespace),
-- `publish` (symbol_id is the method publishing; `message` the type as written, NULL when unknown), `consume`
-- (symbol_id is the consumer class). `scopes` lists, `|`-separated and nearest first, the namespaces a name written
-- there may live in. `queue` is a virtual host or queue; `via` the call or consumer base.
CREATE TABLE IF NOT EXISTS bus_facts (
    file_id      INTEGER NOT NULL,
    symbol_id    INTEGER NOT NULL,
    kind         TEXT NOT NULL,
    message      TEXT,
    message_name TEXT,
    scopes       TEXT NOT NULL,
    queue        TEXT,
    via          TEXT,
    line         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS bus_facts_file ON bus_facts(file_id);
CREATE INDEX IF NOT EXISTS bus_facts_symbol ON bus_facts(symbol_id);
CREATE INDEX IF NOT EXISTS bus_facts_name ON bus_facts(message_name);

-- Ansible facts (see `kc_extract::ansible`): `requirement` (name: a role's name, value: its src), `role_name` (a
-- role's meta name), `vars_file` (a vars file a play or task loads, as written), `deploys`, `undeploys`, `restarts`
-- or `stops` (what a task does, by its module's state or its name's verb; name: what it acts on: service, image,
-- repo, vhost, queue, exchange, database, url, path, site; value as written). symbol_id
-- is the task, play or role, when there is one. GitLab CI files keep theirs here too (`ci_*`, see
-- `kc_extract::gitlabci`), with the job as symbol_id; their order (rowid) within a line is the script's order.
CREATE TABLE IF NOT EXISTS ansible_facts (
    file_id   INTEGER NOT NULL,
    symbol_id INTEGER,
    kind      TEXT NOT NULL,
    name      TEXT NOT NULL,
    value     TEXT NOT NULL,
    line      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ansible_facts_file ON ansible_facts(file_id);
CREATE INDEX IF NOT EXISTS ansible_facts_kind ON ansible_facts(kind, name);

-- Constructs an extractor recognised but didn't read (`kc_extract::Unsupported`, see `unsupported`): the area they
-- belong to (`di`, `bus`, `sql`, `ansible`, `terraform`, `ci`, `angular`, `imports`, `http`), the construct as
-- written, and its line. Every language's; read per repo, never from the workspace summary. `http` rows are endpoint
-- registrations whose routes can't be placed (`kc_extract::HttpRoute`'s `unsupported` side): construct `{verb}:
-- {why}`, symbol_id the handler or the method registering it (NULL in top-level statements; NULL for other areas).
CREATE TABLE IF NOT EXISTS unsupported (
    file_id   INTEGER NOT NULL,
    area      TEXT NOT NULL,
    construct TEXT NOT NULL,
    line      INTEGER NOT NULL,
    symbol_id INTEGER
);
CREATE INDEX IF NOT EXISTS unsupported_file ON unsupported(file_id);
CREATE INDEX IF NOT EXISTS unsupported_area ON unsupported(area);

-- Manifests: packages (SwiftPM package, XcodeGen project, Cargo package or workspace), their targets and products,
-- and their dependencies. `resolved_repo` is the absolute root of the repo a path dependency lands in.
CREATE TABLE IF NOT EXISTS packages (
    id      INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    name    TEXT NOT NULL COLLATE NOCASE,
    line    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS packages_file ON packages(file_id);
CREATE INDEX IF NOT EXISTS packages_name ON packages(name);

CREATE TABLE IF NOT EXISTS package_targets (
    id         INTEGER PRIMARY KEY,
    file_id    INTEGER NOT NULL,
    package_id INTEGER NOT NULL,
    name       TEXT NOT NULL COLLATE NOCASE,
    kind       TEXT NOT NULL,
    line       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS package_targets_file ON package_targets(file_id);
CREATE INDEX IF NOT EXISTS package_targets_name ON package_targets(name);

CREATE TABLE IF NOT EXISTS target_deps (
    file_id   INTEGER NOT NULL,
    target_id INTEGER NOT NULL,
    name      TEXT NOT NULL,
    product   TEXT,
    package   TEXT,
    kind      TEXT NOT NULL,
    line      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS target_deps_file ON target_deps(file_id);

CREATE TABLE IF NOT EXISTS package_deps (
    file_id       INTEGER NOT NULL,
    package_id    INTEGER NOT NULL,
    name          TEXT NOT NULL,
    source        TEXT NOT NULL,
    location      TEXT NOT NULL,
    requirement   TEXT,
    resolved_repo TEXT,
    line          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS package_deps_file ON package_deps(file_id);

-- Per file, the fingerprint of the facts the workspace summary reads (see `summary_key`).
CREATE TABLE IF NOT EXISTS file_keys (
    file_id INTEGER PRIMARY KEY,
    summary TEXT NOT NULL
);

-- Results derived from the whole index (hotspots, key types), cached until the index changes.
CREATE TABLE IF NOT EXISTS derived (
    key  TEXT PRIMARY KEY,
    gen  TEXT NOT NULL,
    data TEXT NOT NULL
) WITHOUT ROWID;

-- Directories seen at the last full discovery, for the cheap freshness check: an unchanged directory listing means
-- no file was added or removed there. `names` is a hash of the sorted entry names.
CREATE TABLE IF NOT EXISTS dirs (
    path     TEXT PRIMARY KEY,
    mtime_ns INTEGER NOT NULL,
    names    TEXT NOT NULL
) WITHOUT ROWID;

-- The content index (see `content`): every searchable file, and trigram -> file ids.
CREATE TABLE IF NOT EXISTS cfiles (
    id       INTEGER PRIMARY KEY,
    path     TEXT NOT NULL UNIQUE,
    mtime_ns INTEGER NOT NULL DEFAULT 0,
    size     INTEGER NOT NULL DEFAULT 0,
    hash     TEXT NOT NULL DEFAULT '',
    state    INTEGER NOT NULL DEFAULT 0,
    skip     TEXT
);

-- A SCIP index's facts (see `scip`), by location: the files it covered with their hash then (a row about a file
-- applies only while it still has that hash), reference sites with the definition each reaches (def_* NULL: defined
-- outside the repo), and `is_implementation` relationships. Lines are 1-based; `name` is the identifier there.
CREATE TABLE IF NOT EXISTS scip_files (
    path TEXT PRIMARY KEY,
    hash TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS scip_refs (
    path     TEXT NOT NULL,
    line     INTEGER NOT NULL,
    name     TEXT NOT NULL,
    def_path TEXT,
    def_line INTEGER,
    def_name TEXT
);
CREATE INDEX IF NOT EXISTS scip_refs_site ON scip_refs(path, line);

CREATE TABLE IF NOT EXISTS scip_impls (
    path      TEXT NOT NULL,
    line      INTEGER NOT NULL,
    name      TEXT NOT NULL,
    base_path TEXT NOT NULL,
    base_line INTEGER NOT NULL,
    base_name TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS scip_impls_impl ON scip_impls(path, name);
CREATE INDEX IF NOT EXISTS scip_impls_base ON scip_impls(base_path, base_name);

CREATE TABLE IF NOT EXISTS grams (
    gram  INTEGER PRIMARY KEY,
    posts BLOB NOT NULL
);
"#;

/// What the index knows about a file, for the freshness check.
#[derive(Debug, Clone)]
pub struct FileState {
    pub id: i64,
    pub mtime_ns: i64,
    pub size: i64,
    pub hash: String,
    pub extractor_version: u32,
    /// The language it was indexed as.
    pub lang: String,
}

/// A parsed file ready to be written.
pub struct FileRecord<'a> {
    pub path: &'a str,
    pub lang: Lang,
    pub mtime_ns: i64,
    pub size: i64,
    pub hash: &'a str,
    pub lines: u32,
    pub facts: &'a FileFacts,
}

/// A repo's index. Reads made through [`Store::conn`] while a snapshot is held (see [`Store::begin_snapshot`]) all see
/// one committed state of the index, whatever another process writes meanwhile.
pub struct Store {
    conn: Connection,
    /// A second connection, outside the snapshot, for caches that carry their own validity key (see [`Store::side`]).
    side: std::cell::OnceCell<Connection>,
    // Dropped after both SQLite connections, so cache eviction cannot unlink a live index.
    _lease: Option<std::fs::File>,
}

/// A file's content hash as the index records it (`files.hash`): blake3 of its bytes, in hex.
pub fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Whether a path is test code by convention: a test directory (`tests/`, `Api.UnitTests/`, `__tests__/`) or a test
/// file name (`FooTests.cs`, `test_api.py`, `foo_test.go`, `foo.spec.ts`). Names merely containing the word (`Latest`,
/// `Testimonial`, `MonitorTestFailed`, `DeleteState`) are not.
pub fn is_test_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.split('.').next().unwrap_or(name);
    path.split('/').any(|c| matches!(c, "Tests" | "tests" | "test" | "Test" | "__tests__" | "testing" | "UITests") || c.ends_with("Tests"))
        || stem.ends_with("Tests")
        || stem.ends_with("Test")
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || stem == "conftest"
        // `foo.spec.ts`, `foo.test.js`.
        || name.contains(".spec.")
        || name.contains(".test.")
}

/// Whether `path` is under a directory holding copies rather than the live code: a backup, an old version, an
/// archive, a copy or a site template variant (`2020_backup/`, `old/`, `_templates/`, `Copy of x/`). When several
/// definitions share a name, these rank after the others.
pub fn is_copy_path(path: &str) -> bool {
    let dirs = path.rsplit_once('/').map_or("", |(d, _)| d);
    dirs.split('/').any(|d| {
        let d = d.to_ascii_lowercase();
        d.contains("backup")
            || matches!(d.as_str(), "bak" | "old" | "archive" | "archived" | "deprecated" | "obsolete" | "unused" | "copy")
            || d.starts_with("old_")
            || d.starts_with("old-")
            || d.ends_with("_old")
            || d.ends_with("-old")
            || d.ends_with(".old")
            || d.ends_with(".bak")
            || d.ends_with("_bak")
            || d.starts_with("copy of")
            || d.ends_with(" copy")
            || d.ends_with(" - copy")
            || d.ends_with("_copy")
            || d.ends_with("-copy")
            || matches!(d.as_str(), "_templates" | "_template" | "templates_old")
    })
}

/// How far down a definition at `path` ranks among same-named ones: live code 0, test code 1, copies 2.
pub fn path_rank(path: &str) -> u8 {
    if is_copy_path(path) {
        2
    } else if is_test_path(path) {
        1
    } else {
        0
    }
}

/// Puts the index in WAL mode, once. Switching a new file needs it alone, and SQLite answers busy at once rather than
/// waiting when another connection is opening it too, so the switch is retried for a while.
fn set_wal(conn: &Connection) -> Result<()> {
    let mut tries = 0;
    loop {
        let mode: rusqlite::Result<String> = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0));
        if mode.as_deref().is_ok_and(|m| m.eq_ignore_ascii_case("wal")) {
            return Ok(());
        }
        match conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0)) {
            Ok(_) => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::DatabaseBusy && tries < 500 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Opens a read transaction on `conn` and takes its snapshot (a deferred transaction takes it at its first read).
fn begin_snapshot(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN DEFERRED")?;
    conn.query_row("SELECT count(*) FROM meta", [], |_| Ok(()))?;
    Ok(())
}

/// An index file's schema against this build's.
#[derive(Debug, PartialEq)]
enum Schema {
    /// This build's.
    Current,
    /// No schema yet (a new file).
    Empty,
    /// An older build's (or unreadable): rebuilt.
    Older,
    /// A newer build's: refused.
    Newer(u32),
}

/// The schema of the index `conn` holds.
fn schema_state(conn: &Connection) -> Result<Schema> {
    let has_meta: bool =
        conn.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'", [], |_| Ok(true)).optional()?.unwrap_or(false);
    if !has_meta {
        return Ok(Schema::Empty);
    }
    let version: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).optional()?;
    Ok(match version.and_then(|v| v.parse::<u32>().ok()) {
        Some(v) if v == SCHEMA_VERSION => Schema::Current,
        Some(v) if v > SCHEMA_VERSION => Schema::Newer(v),
        _ => Schema::Older,
    })
}

/// Drops every table (and with them their indexes and triggers): virtual tables first, which drop their own shadow
/// tables, then the rest, including tables created on demand (history caches) that the schema doesn't list.
fn drop_all(tx: &Connection) -> Result<()> {
    let tables = |sql: &str| -> Result<Vec<String>> {
        Ok(tx.prepare(sql)?.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?)
    };
    for name in tables("SELECT name FROM sqlite_master WHERE type = 'table' AND sql LIKE 'CREATE VIRTUAL TABLE%'")? {
        tx.execute_batch(&format!("DROP TABLE IF EXISTS \"{}\";", name.replace('"', "\"\"")))?;
    }
    for name in tables("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")? {
        tx.execute_batch(&format!("DROP TABLE IF EXISTS \"{}\";", name.replace('"', "\"\"")))?;
    }
    Ok(())
}

impl Store {
    /// Opens an index while retaining its caller's already-locked lifetime lease until all connections close.
    pub fn open_leased(path: &Path, lease: std::fs::File) -> Result<Store> {
        let mut store = Self::open(path)?;
        store._lease = Some(lease);
        Ok(store)
    }

    /// Whether opening this index would create or migrate its schema. Does not write to the file.
    pub fn needs_migration(path: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(true);
        }
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(schema_state(&conn)? != Schema::Current)
    }

    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        // Another process (the MCP server, a CLI query) may hold the index for a moment: opening while it checkpoints
        // or closes its WAL is briefly busy, so wait rather than fail.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // A 2 MB page cache per connection: queries touch little, and the MCP server may hold several connections.
        conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA temp_store = MEMORY; PRAGMA cache_size = -2048;")?;
        set_wal(&conn)?;
        let mut store = Store { conn, side: std::cell::OnceCell::new(), _lease: None };
        store.migrate()?;
        Ok(store)
    }

    /// Pins every read through [`Store::conn`] to the index as committed now, until [`Store::end_snapshot`] or the
    /// next write batch: a query resolving in many steps then never mixes two generations (symbol ids another
    /// process's sync replaced, a generation read before a reparse and facts read after). WAL mode (see [`set_wal`])
    /// gives readers snapshot isolation without blocking writers: another process's sync proceeds, and this
    /// connection keeps reading the state it started from. A no-op when one is held already.
    pub fn begin_snapshot(&self) -> Result<()> {
        if self.conn.is_autocommit() {
            begin_snapshot(&self.conn)?;
        }
        Ok(())
    }

    /// Releases the snapshot [`Store::begin_snapshot`] took; returns whether one was held.
    pub fn end_snapshot(&self) -> Result<bool> {
        if self.conn.is_autocommit() {
            return Ok(false);
        }
        // Read-only: nothing to commit but the release of the snapshot.
        self.conn.execute_batch("COMMIT")?;
        Ok(true)
    }

    /// Whether reads are pinned to a snapshot (see [`Store::begin_snapshot`]).
    pub fn in_snapshot(&self) -> bool {
        !self.conn.is_autocommit()
    }

    /// The connection for caches whose rows carry what they are valid for (a generation, a git HEAD) rather than
    /// depending on the snapshot: the `derived` cache, `meta` writes and the history caches. Outside a snapshot it is
    /// [`Store::conn`]; inside one, a second connection, so cache writes commit at once instead of upgrading the
    /// snapshot to a write transaction (which fails once another process has committed since it began, and would
    /// hold the write lock for the rest of the query), and the snapshot's reads stay as they were.
    pub fn side(&self) -> Result<&Connection> {
        if self.conn.is_autocommit() {
            return Ok(&self.conn);
        }
        if let Some(c) = self.side.get() {
            return Ok(c);
        }
        let path = self.conn.path().filter(|p| !p.is_empty()).map(std::path::PathBuf::from).context("an in-memory index has no side connection")?;
        let c = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        c.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA temp_store = MEMORY; PRAGMA cache_size = -2048;")?;
        Ok(self.side.get_or_init(|| c))
    }

    /// A `meta` value as committed now, outside any snapshot: for the values the side caches keep (see
    /// [`Store::side`]).
    pub fn side_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.side()?.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    /// Brings the file to this build's schema: creates it, or rebuilds an older one from scratch. A newer one is an
    /// error: dropping it would destroy another build's index, which then rebuilds it, and the two builds take turns
    /// rebuilding each other's. Both can run against separate caches (`KNOWCODE_CACHE_DIR`).
    fn migrate(&mut self) -> Result<()> {
        if schema_state(&self.conn)? == Schema::Current {
            return Ok(()); // the common case: no writes on open
        }
        // Another connection may be creating or migrating this index right now. Holding the write lock and checking
        // again keeps it from seeing a half-made schema as an old one and dropping it.
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        match schema_state(&tx)? {
            Schema::Current => return Ok(tx.commit()?),
            Schema::Newer(v) => {
                let path = tx.path().map(|p| p.to_string()).unwrap_or_default();
                anyhow::bail!(
                    "index {path} was written by a newer knowcode (index schema {v}; this build reads schema {SCHEMA_VERSION}): upgrade knowcode, or point this build at a separate cache with KNOWCODE_CACHE_DIR"
                );
            }
            Schema::Older => drop_all(&tx)?,
            Schema::Empty => {}
        }
        tx.execute_batch(SCHEMA)?;
        tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?1)", [SCHEMA_VERSION.to_string()])?;
        tx.commit()?;
        Ok(())
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    /// Sets a `meta` value, committed at once. Inside a snapshot it is written through [`Store::side`]: the snapshot
    /// does not see it, [`Store::side_meta`] does.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.side()?.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn file_states(&self) -> Result<HashMap<String, FileState>> {
        let mut stmt = self.conn.prepare("SELECT id, path, mtime_ns, size, hash, extractor_version, lang FROM files")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                FileState {
                    id: r.get(0)?,
                    mtime_ns: r.get(2)?,
                    size: r.get(3)?,
                    hash: r.get(4)?,
                    extractor_version: r.get(5)?,
                    lang: r.get(6)?,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The index generation: changes whenever a sync reparses or removes a file, or the resolver changes.
    pub fn generation(&self) -> Result<String> {
        Ok(format!("{}|r{RESOLVER_VERSION}", self.meta("tree_gen")?.unwrap_or_default()))
    }

    /// A value computed from the whole index, cached under `key` until `generation` changes.
    pub fn derived<T: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        key: &str,
        generation: &str,
        compute: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let cached: Option<(String, String)> = self
            .side()?
            .query_row("SELECT gen, data FROM derived WHERE key = ?1", [key], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((g, data)) = cached
            && g == generation
            && let Ok(v) = serde_json::from_str(&data)
        {
            return Ok(v);
        }
        let cuts = limits::cuts_so_far();
        let v = compute()?;
        // A value computed while a child process was cut short may lack what it would have given: used, not kept.
        if limits::cuts_so_far() != cuts {
            return Ok(v);
        }
        self.side()?.execute(
            "INSERT INTO derived(key, gen, data) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET gen = excluded.gen, data = excluded.data",
            params![key, generation, serde_json::to_string(&v)?],
        )?;
        Ok(v)
    }

    /// The value cached under `key` at whatever generation it was computed, with that generation.
    pub fn derived_latest<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<(String, T)>> {
        let cached: Option<(String, String)> = self
            .side()?
            .query_row("SELECT gen, data FROM derived WHERE key = ?1", [key], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        Ok(cached.and_then(|(g, data)| Some((g, serde_json::from_str(&data).ok()?))))
    }

    /// Resolved file-to-file dependencies, cached per index generation. Shares its resolution pass with
    /// [`Store::ref_graph`].
    pub fn file_edges(&self) -> Result<Vec<query::FileEdge>> {
        self.derived("file_edges", &self.generation()?, || Ok(self.ref_graph()?.file_edges()))
    }

    /// The file reference graph with per-name reference counts, cached per index generation. Built once per process:
    /// while another thread builds it (a server's background, after indexing), this one waits for that.
    pub fn ref_graph(&self) -> Result<rank::RefGraph> {
        let generation = self.generation()?;
        let _turn = rank::Turn::take(self.conn.path().unwrap_or_default(), &generation);
        self.derived(&format!("ref_graph:{}", rank::RANK_VERSION), &generation, || rank::ref_graph(&self.conn))
    }

    /// File ranks for ordering a long answer: PageRank over the reference graph, personalized toward `seeds` (the
    /// global rank, cached per generation, without seeds). The weighted graph is cached per generation, rebuilt
    /// from the full graph when that is current (`overview` and `hotspots` compute it). Computing the full graph costs
    /// a pass resolving every reference, so a graph from an earlier generation is reused rather than paying it on
    /// every edit (a few edits move ranks little, and only order depends on them); with none cached it is computed
    /// when `compute` is set, else the ranks are empty and ties keep their path order.
    pub fn file_rank(&self, seeds: &[&str], flow: rank::Flow, compute: bool) -> Result<rank::FileRank> {
        Ok(self.file_rank_at(seeds, flow, compute)?.0)
    }

    /// [`Store::file_rank`], and the generation of the graph it ranked when that is an earlier one than the index's:
    /// an answer ordered by it says so. With none cached while the graph is being built in the background, a query
    /// with a freshness deadline waits for it until then; past it, nothing is ranked and the generation given is
    /// [`rank::PENDING`].
    pub fn file_rank_at(&self, seeds: &[&str], flow: rank::Flow, compute: bool) -> Result<(rank::FileRank, Option<String>)> {
        let generation = self.generation()?;
        let key = format!("rank_graph:{}", rank::RANK_VERSION);
        let ref_key = format!("ref_graph:{}", rank::RANK_VERSION);
        let (graph_gen, graph) = match self.derived_latest::<rank::RankGraph>(&key)? {
            Some((g, graph)) if g == generation => (g, graph),
            // The full graph is current (an overview or hotspots computed it): the weights follow cheaply.
            _ if self.derived_gen(&ref_key)?.as_deref() == Some(generation.as_str()) => {
                (generation.clone(), self.derived(&key, &generation, || Ok(self.ref_graph()?.weighted()))?)
            }
            Some(stale) => stale,
            None if compute => {
                if let Some(deadline) = limits::current().freshness_deadline()
                    && !rank::wait_built(self.conn.path().unwrap_or_default(), &generation, deadline)
                {
                    return Ok((rank::FileRank::default(), Some(rank::PENDING.to_string())));
                }
                (generation.clone(), self.derived(&key, &generation, || Ok(self.ref_graph()?.weighted()))?)
            }
            None => return Ok((rank::FileRank::default(), None)),
        };
        let stale = (graph_gen != generation).then(|| graph_gen.clone());
        if seeds.is_empty() && flow == rank::Flow::Uses {
            let scores = self.derived(&format!("pagerank:{}", rank::RANK_VERSION), &graph_gen, || Ok(rank::pagerank(&graph, &[], flow)))?;
            return Ok((rank::FileRank(scores), stale));
        }
        Ok((rank::FileRank(rank::pagerank(&graph, seeds, flow)), stale))
    }

    /// The generation the value cached under `key` was computed at.
    pub fn derived_gen(&self, key: &str) -> Result<Option<String>> {
        Ok(self.side()?.query_row("SELECT gen FROM derived WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    /// The Entity Framework model (tables, their accessors, stored procedures), cached per index generation.
    pub fn db_model(&self) -> Result<db::DbModel> {
        self.derived(&format!("db_model:{}", db::MODEL_VERSION), &self.generation()?, || db::model(&self.conn))
    }

    /// Directories recorded at the last full discovery: (path, mtime_ns, names hash).
    pub fn dirs(&self) -> Result<Vec<(String, i64, String)>> {
        let mut stmt = self.conn.prepare_cached("SELECT path, mtime_ns, names FROM dirs")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Starts a write batch. Everything in one batch commits atomically. A snapshot held is released first, and taken
    /// again at the new state when the batch commits. The batch takes the write lock at once, waiting up to the busy
    /// timeout for another writer: a deferred one that read first could not write once another process committed.
    pub fn batch(&mut self) -> Result<Batch<'_>> {
        let resnap = self.end_snapshot()?;
        let tx = Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        // An index made before the table was: created in place, its files' keys missing (each reads as changed).
        tx.execute_batch("CREATE TABLE IF NOT EXISTS file_keys (file_id INTEGER PRIMARY KEY, summary TEXT NOT NULL);")?;
        Ok(Batch { tx, conn: &self.conn, resnap })
    }

    /// Reclaims space after large deletions (for example, a new ignore rule dropping hundreds of files).
    pub fn vacuum(&self) -> Result<()> {
        let resnap = self.end_snapshot()?;
        self.conn.execute_batch("VACUUM;")?;
        if resnap {
            self.begin_snapshot()?;
        }
        Ok(())
    }

    pub fn checkpoint(&self) -> Result<()> {
        // A checkpoint can't run inside this connection's own read transaction.
        let resnap = self.end_snapshot()?;
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;")?;
        if resnap {
            self.begin_snapshot()?;
        }
        Ok(())
    }
}

pub struct Batch<'c> {
    tx: Transaction<'c>,
    conn: &'c Connection,
    /// A snapshot was held before the batch: one is taken again after it commits.
    resnap: bool,
}

impl Batch<'_> {
    pub fn commit(self) -> Result<()> {
        self.tx.commit()?;
        if self.resnap {
            begin_snapshot(self.conn)?;
        }
        Ok(())
    }

    /// The batch's connection, inside its transaction: for writers kept outside this module.
    pub fn conn(&self) -> &Connection {
        &self.tx
    }

    /// Resolves what the DI factories whose type only the repo's declarations tell make (see [`di::resolve_made`]):
    /// after the files changed, since a declaration in any file may change the answer.
    pub fn resolve_di_factories(&self) -> Result<()> {
        di::resolve_made(&self.tx)
    }

    /// Replaces the stored SCIP facts (see [`scip`]).
    pub fn replace_scip(&self, facts: &scip::Facts) -> Result<()> {
        scip::replace(&self.tx, facts)
    }

    /// Records the SCIP facts' project inputs as the indexer saw them (see [`scip::record_inputs`]).
    pub fn record_scip_inputs(&self, root: &std::path::Path, scip_mtime: i64) -> Result<()> {
        scip::record_inputs(&self.tx, root, scip_mtime)
    }

    pub fn replace_dirs(&self, dirs: &[(String, i64, String)]) -> Result<()> {
        self.tx.execute("DELETE FROM dirs", [])?;
        let mut stmt = self.tx.prepare_cached("INSERT INTO dirs(path, mtime_ns, names) VALUES (?1, ?2, ?3)")?;
        for (path, mtime, names) in dirs {
            stmt.execute(params![path, mtime, names])?;
        }
        Ok(())
    }

    pub fn set_dir_mtime(&self, path: &str, mtime_ns: i64) -> Result<()> {
        self.tx.prepare_cached("UPDATE dirs SET mtime_ns = ?2 WHERE path = ?1")?.execute(params![path, mtime_ns])?;
        Ok(())
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.tx
            .prepare_cached(
                "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )?
            .execute(params![key, value])?;
        Ok(())
    }

    /// Records a new mtime and size for a file whose content hash didn't change.
    pub fn touch(&self, id: i64, mtime_ns: i64, size: i64) -> Result<()> {
        self.tx.prepare_cached("UPDATE files SET mtime_ns = ?2, size = ?3 WHERE id = ?1")?.execute(params![
            id, mtime_ns, size
        ])?;
        Ok(())
    }

    pub fn remove(&self, id: i64) -> Result<()> {
        self.clear_facts(id)?;
        self.tx.prepare_cached("DELETE FROM file_keys WHERE file_id = ?1")?.execute([id])?;
        self.tx.prepare_cached("DELETE FROM files WHERE id = ?1")?.execute([id])?;
        Ok(())
    }

    fn clear_facts(&self, id: i64) -> Result<()> {
        for sql in [
            "DELETE FROM symbols_fts WHERE rowid IN (SELECT id FROM symbols WHERE file_id = ?1)",
            "DELETE FROM symbols WHERE file_id = ?1",
            "DELETE FROM files_fts WHERE rowid = ?1",
            "DELETE FROM refs WHERE file_id = ?1",
            "DELETE FROM relations WHERE file_id = ?1",
            "DELETE FROM imports WHERE file_id = ?1",
            "DELETE FROM global_usings WHERE file_id = ?1",
            "DELETE FROM import_bindings WHERE file_id = ?1",
            "DELETE FROM ts_configs WHERE file_id = ?1",
            "DELETE FROM packages WHERE file_id = ?1",
            "DELETE FROM package_targets WHERE file_id = ?1",
            "DELETE FROM target_deps WHERE file_id = ?1",
            "DELETE FROM package_deps WHERE file_id = ?1",
            "DELETE FROM http_routes WHERE file_id = ?1",
            "DELETE FROM host_facts WHERE file_id = ?1",
            "DELETE FROM constants WHERE file_id = ?1",
            "DELETE FROM ng_facts WHERE file_id = ?1",
            "DELETE FROM di_bindings WHERE file_id = ?1",
            "DELETE FROM db_facts WHERE file_id = ?1",
            "DELETE FROM bus_facts WHERE file_id = ?1",
            "DELETE FROM ansible_facts WHERE file_id = ?1",
            "DELETE FROM unsupported WHERE file_id = ?1",
        ] {
            self.tx.prepare_cached(sql)?.execute([id])?;
        }
        Ok(())
    }

    /// Inserts or replaces a file and all of its facts. Returns whether what the workspace summary reads of it
    /// changed (see [`summary_key`]): a new file, or facts with another key.
    pub fn put(&self, rec: &FileRecord) -> Result<bool> {
        let existing: Option<i64> =
            self.tx.prepare_cached("SELECT id FROM files WHERE path = ?1")?.query_row([rec.path], |r| r.get(0)).optional()?;
        let file_id = match existing {
            Some(id) => {
                self.clear_facts(id)?;
                self.tx
                    .prepare_cached(
                        "UPDATE files SET lang = ?2, mtime_ns = ?3, size = ?4, hash = ?5, extractor_version = ?6,
                         parse_errors = ?7, lines = ?8, script = ?9 WHERE id = ?1",
                    )?
                    .execute(params![
                        id,
                        rec.lang.as_str(),
                        rec.mtime_ns,
                        rec.size,
                        rec.hash,
                        rec.lang.extractor_version(),
                        rec.facts.parse_errors,
                        rec.lines,
                        rec.facts.script
                    ])?;
                id
            }
            None => {
                self.tx
                    .prepare_cached(
                        "INSERT INTO files(path, lang, mtime_ns, size, hash, extractor_version, parse_errors, lines, script)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    )?
                    .execute(params![
                        rec.path,
                        rec.lang.as_str(),
                        rec.mtime_ns,
                        rec.size,
                        rec.hash,
                        rec.lang.extractor_version(),
                        rec.facts.parse_errors,
                        rec.lines,
                        rec.facts.script
                    ])?;
                self.tx.last_insert_rowid()
            }
        };

        let mut ids = Vec::with_capacity(rec.facts.symbols.len());
        let mut sym_stmt = self.tx.prepare_cached(
            "INSERT INTO symbols(file_id, parent_id, name, kind, container, signature, doc, attrs, tags, start_line,
             end_line, start_byte, end_byte, complexity, type_name, nesting, min_args, max_args, lambda_types, bumps, cond_ops, cond_line,
             param_types)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
        )?;
        let mut fts_stmt = self.tx.prepare_cached("INSERT INTO symbols_fts(rowid, tokens) VALUES (?1, ?2)")?;
        for (i, s) in rec.facts.symbols.iter().enumerate() {
            let parent = s.parent.map(|p| ids[p]);
            sym_stmt.execute(params![
                file_id,
                parent,
                s.name,
                s.kind.as_str(),
                s.container,
                s.signature,
                s.doc,
                s.attrs.join(","),
                s.tags.join(","),
                s.start_line,
                s.end_line,
                s.start_byte,
                s.end_byte,
                s.complexity,
                s.type_name,
                s.nesting,
                s.arity.map(|a| a.0),
                s.arity.and_then(|a| a.1),
                s.lambda_types,
                (!s.bumps.is_empty()).then(|| s.bumps.iter().map(|(a, b)| format!("{a}-{b}")).collect::<Vec<_>>().join(",")),
                s.cond_ops,
                (s.cond_ops > 0).then_some(s.cond_line),
                rec.facts.param_types.get(&i)
            ])?;
            let id = self.tx.last_insert_rowid();
            fts_stmt.execute(params![id, name_tokens(&s.name)])?;
            ids.push(id);
        }
        // A config file read only for its hosts stays the text document `locate` read it as before (see
        // `kc_index::textdocs`).
        if rec.lang != Lang::HostConfig {
            let (fields, len) = locate::file_document(rec.path, rec.facts);
            self.tx
                .prepare_cached("INSERT INTO files_fts(rowid, path, names, docs, refs, len) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
                .execute(params![file_id, fields[0], fields[1], fields[2], fields[3], len])?;
        }

        let mut ref_stmt = self.tx.prepare_cached(
            "INSERT INTO refs(file_id, from_id, name, receiver, receiver_type, kind, line, col, args, arg_types, assigns, usage)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )?;
        for (i, r) in rec.facts.refs.iter().enumerate() {
            ref_stmt.execute(params![
                file_id,
                r.from.map(|f| ids[f]),
                r.name,
                r.receiver,
                r.receiver_type,
                r.kind.as_str(),
                r.line,
                r.col,
                r.args,
                rec.facts.arg_types.get(&i),
                rec.facts.assigns.contains(&i),
                rec.facts.usages.get(&i)
            ])?;
        }
        let mut rel_stmt = self.tx.prepare_cached(
            "INSERT INTO relations(file_id, from_id, target, target_name, kind, line) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for r in &rec.facts.relations {
            let target_name = r.target.rsplit('.').next().unwrap_or(&r.target);
            rel_stmt.execute(params![file_id, ids[r.from], r.target, target_name, "inherits", r.line])?;
        }
        let mut imp_stmt = self.tx.prepare_cached("INSERT INTO imports(file_id, module, line) VALUES (?1, ?2, ?3)")?;
        for i in &rec.facts.imports {
            imp_stmt.execute(params![file_id, i.module, i.line])?;
        }
        let mut bind_stmt = self.tx.prepare_cached(
            "INSERT INTO import_bindings(file_id, local, name, module, export, line) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for b in &rec.facts.bindings {
            bind_stmt.execute(params![file_id, b.local, b.name, b.module, b.export, b.line])?;
        }
        let mut global_stmt =
            self.tx.prepare_cached("INSERT INTO global_usings(file_id, namespace, alias, is_static, line) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        for g in &rec.facts.global_usings {
            global_stmt.execute(params![file_id, g.namespace, g.alias, g.is_static, g.line])?;
        }
        if let Some(c) = &rec.facts.ts_config {
            self.tx
                .prepare_cached("INSERT INTO ts_configs(file_id, config) VALUES (?1, ?2)")?
                .execute(params![file_id, serde_json::to_string(c)?])?;
        }
        let mut route_stmt = self.tx.prepare_cached(
            "INSERT INTO http_routes(file_id, symbol_id, side, verb, path, path_ref, service, service_ref, line)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        let mut unplaced_stmt =
            self.tx.prepare_cached("INSERT INTO unsupported(file_id, area, construct, line, symbol_id) VALUES (?1, 'http', ?2, ?3, ?4)")?;
        for r in &rec.facts.http_routes {
            // A registration whose route can't be placed is a construct met but not read (see [`unsupported`]).
            if r.side == "unsupported" {
                unplaced_stmt.execute(params![file_id, format!("{}: {}", r.verb, r.path), r.line, r.sym.map(|i| ids[i])])?;
                continue;
            }
            route_stmt.execute(params![file_id, r.sym.map(|i| ids[i]), r.side, r.verb, r.path, r.path_ref, r.service, r.service_ref, r.line])?;
        }
        let mut host_stmt =
            self.tx.prepare_cached("INSERT INTO host_facts(file_id, role, kind, host, target, line) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
        for h in &rec.facts.hosts {
            host_stmt.execute(params![file_id, h.role, h.kind, h.host, h.target, h.line])?;
        }
        let mut ng_stmt =
            self.tx.prepare_cached("INSERT INTO ng_facts(file_id, symbol_id, kind, value, extra, line) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
        for n in &rec.facts.ng {
            ng_stmt.execute(params![file_id, n.sym.map(|i| ids[i]), n.kind, n.value, n.extra, n.line])?;
        }
        let mut db_stmt = self
            .tx
            .prepare_cached("INSERT INTO db_facts(file_id, symbol_id, kind, entity, name, line) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
        for d in &rec.facts.db {
            db_stmt.execute(params![file_id, d.sym.map(|i| ids[i]), d.kind, d.entity, d.name, d.line])?;
        }
        let mut const_stmt =
            self.tx.prepare_cached("INSERT INTO constants(file_id, qualified, value, line) VALUES (?1, ?2, ?3, ?4)")?;
        for c in &rec.facts.constants {
            const_stmt.execute(params![file_id, c.qualified, c.value, c.line])?;
        }
        let mut di_stmt = self.tx.prepare_cached(
            "INSERT INTO di_bindings(file_id, service, service_name, implementation, lifetime, kind, line, made, service_made, mode)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        for b in &rec.facts.di {
            let name = b.service.as_deref().map(di::bare_name);
            di_stmt.execute(params![file_id, b.service, name, b.implementation, b.lifetime, b.kind, b.line, b.made, b.service_made, b.mode])?;
        }
        let mut bus_stmt = self.tx.prepare_cached(
            "INSERT INTO bus_facts(file_id, symbol_id, kind, message, message_name, scopes, queue, via, line)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for b in &rec.facts.bus {
            let name = b.message.as_deref().map(bus::bare_name);
            bus_stmt.execute(params![file_id, ids[b.sym], b.kind, b.message, name, b.scopes.join("|"), b.queue, b.via, b.line])?;
        }
        let mut ansible_stmt = self.tx.prepare_cached(
            "INSERT INTO ansible_facts(file_id, symbol_id, kind, name, value, line) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for a in &rec.facts.ansible {
            ansible_stmt.execute(params![file_id, a.sym.map(|i| ids[i]), a.kind, a.name, a.value, a.line])?;
        }
        // Constructs met but not read (see [`unsupported`]).
        let mut unsupported_stmt = self.tx.prepare_cached("INSERT INTO unsupported(file_id, area, construct, line) VALUES (?1, ?2, ?3, ?4)")?;
        for u in &rec.facts.unsupported {
            unsupported_stmt.execute(params![file_id, u.area, u.construct, u.line])?;
        }
        for p in &rec.facts.packages {
            self.tx
                .prepare_cached("INSERT INTO packages(file_id, kind, name, line) VALUES (?1, ?2, ?3, ?4)")?
                .execute(params![file_id, p.kind, p.name, p.line])?;
            let package_id = self.tx.last_insert_rowid();
            for t in &p.targets {
                self.tx
                    .prepare_cached(
                        "INSERT INTO package_targets(file_id, package_id, name, kind, line) VALUES (?1, ?2, ?3, ?4, ?5)",
                    )?
                    .execute(params![file_id, package_id, t.name, t.kind, t.line])?;
                let target_id = self.tx.last_insert_rowid();
                let mut dep_stmt = self.tx.prepare_cached(
                    "INSERT INTO target_deps(file_id, target_id, name, product, package, kind, line)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                for d in &t.deps {
                    dep_stmt.execute(params![file_id, target_id, d.name, d.product, d.package, d.kind, d.line])?;
                }
            }
            let mut dep_stmt = self.tx.prepare_cached(
                "INSERT INTO package_deps(file_id, package_id, name, source, location, requirement, resolved_repo, line)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for d in &p.deps {
                dep_stmt.execute(params![
                    file_id,
                    package_id,
                    d.name,
                    d.source.as_str(),
                    d.location,
                    d.requirement,
                    d.resolved_repo,
                    d.line
                ])?;
            }
        }
        let key = summary_key(rec.lang, rec.facts);
        let old: Option<String> = match existing {
            Some(_) => self.tx.prepare_cached("SELECT summary FROM file_keys WHERE file_id = ?1")?.query_row([file_id], |r| r.get(0)).optional()?,
            None => None,
        };
        let changed = old.as_deref() != Some(key.as_str());
        if changed {
            self.tx.prepare_cached("INSERT OR REPLACE INTO file_keys(file_id, summary) VALUES (?1, ?2)")?.execute(params![file_id, key])?;
        }
        Ok(changed)
    }
}

/// A fingerprint of the facts of a file that the workspace summary (`kc_index::workspace`) can read: every fact, but
/// of symbols and references only the fields its queries select. The summary reads, of `symbols`, the name, kind,
/// container, signature, attributes, tags, lines and parent (`bus::rows`, `sql::defs`, `ansible::summary`,
/// `ci::summary`, `terraform::summary`, route and selector rows, defined names); of `refs`, the name, kind, receiver,
/// line and enclosing symbol (`terraform::summary`, referenced names); everything else whole. So an edit inside a
/// body that moves no line and changes no name keeps the key: byte offsets, complexity, nesting, bumps, conditions,
/// doc comments, property types, arity, delegate and parameter types, columns, receiver types, argument counts and
/// types, assignments and parse errors don't reach the summary. A summary reader selecting another of these must
/// add it here (the workspace tests compare a summary kept by its key with one read afresh).
pub fn summary_key(lang: Lang, facts: &FileFacts) -> String {
    // Destructured without `..`: a fact added to these types fails to compile here until it is either keyed or
    // shown to be out of the summary's reach.
    let FileFacts {
        // Read per repo (`unsupported::in_area`), never from the summary.
        unsupported: _,
        symbols,
        refs,
        relations,
        imports,
        global_usings: _,
        bindings,
        ts_config,
        packages,
        http_routes,
        hosts,
        constants,
        ng,
        di,
        db,
        bus,
        ansible,
        arg_types: _,
        assigns: _,
        usages: _,
        param_types: _,
        parse_errors: _,
        script,
    } = facts;
    let symbols: Vec<_> = symbols
        .iter()
        .map(|s| {
            let kc_extract::Symbol {
                parent,
                name,
                kind,
                container,
                signature,
                doc: _,
                attrs,
                tags,
                start_line,
                end_line,
                start_byte: _,
                end_byte: _,
                complexity: _,
                nesting: _,
                bumps: _,
                cond_ops: _,
                cond_line: _,
                type_name: _,
                arity: _,
                lambda_types: _,
            } = s;
            (parent, name, kind, container, signature, attrs, tags, start_line, end_line)
        })
        .collect();
    let refs: Vec<_> = refs
        .iter()
        .map(|r| {
            let kc_extract::Ref { from, name, receiver, receiver_type: _, kind, line, col: _, args: _ } = r;
            (from, name, receiver, kind, line)
        })
        .collect();
    let mut h = blake3::Hasher::new();
    h.update(lang.as_str().as_bytes());
    h.update(b"\0");
    // Written straight into the hash; serializing these types can't fail.
    let _ = serde_json::to_writer(
        &mut h,
        &(symbols, refs, relations, imports, bindings, ts_config, packages, http_routes, constants, ng, di, db, bus, ansible, script, hosts),
    );
    h.finalize().to_hex()[..32].to_string()
}

/// Splits an identifier into lowercase words: `loadHTTPSettings2` gives `load`, `http`, `settings`, `2`.
pub fn split_words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        let boundary = match prev {
            Some(p) if p.is_alphanumeric() => {
                (c.is_uppercase() && p.is_lowercase())
                    || (c.is_uppercase() && p.is_uppercase() && next.is_some_and(|n| n.is_lowercase()))
                    || (c.is_ascii_digit() != p.is_ascii_digit())
            }
            _ => false,
        };
        if boundary && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// FTS tokens for a symbol name: its words plus the whole name, so both `settings` and `loadhttpsettings2` match.
pub fn name_tokens(name: &str) -> String {
    let mut words = split_words(name);
    let whole: String = name.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
    if words.len() > 1 {
        words.push(whole);
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_has_no_process_wide_locks() {
        // Built as .cargo/config.toml says: a page cache per connection, allocations not counted. Without it,
        // concurrent indexing waits on two global mutexes.
        let conn = Connection::open_in_memory().unwrap();
        let options: Vec<String> =
            conn.prepare("PRAGMA compile_options").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert!(!options.iter().any(|o| o == "ENABLE_MEMORY_MANAGEMENT"), "{options:?}");
        assert!(options.iter().any(|o| o == "DEFAULT_MEMSTATUS=0"), "{options:?}");
    }

    #[test]
    fn concurrent_opens_of_a_new_index_keep_its_schema() {
        let dir = std::env::temp_dir().join(format!("knowcode-test-store-{}", std::process::id()));
        for round in 0..20 {
            let path = dir.join(format!("{round}.sqlite"));
            let opens: Vec<_> = (0..6)
                .map(|_| {
                    let path = path.clone();
                    std::thread::spawn(move || Store::open(&path).unwrap().set_meta("root", "/x").unwrap())
                })
                .collect();
            for t in opens {
                t.join().unwrap();
            }
            assert_eq!(Store::open(&path).unwrap().meta("root").unwrap().as_deref(), Some("/x"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Opening never destroys an index another build can read: a newer schema is refused and left as it was; only an
    /// older one is rebuilt, tables made outside the schema included.
    #[test]
    fn a_newer_schema_is_refused_and_an_older_one_rebuilt() {
        let dir = std::env::temp_dir().join(format!("knowcode-test-schema-{}", std::process::id()));
        let path = dir.join("i.sqlite");
        let _ = std::fs::remove_dir_all(&dir);
        let set_version = |v: &str| {
            let store = Store::open(&path).unwrap();
            store.set_meta("schema_version", v).unwrap();
        };
        {
            let store = Store::open(&path).unwrap();
            store.set_meta("root", "/kept").unwrap();
            store.conn().execute_batch("CREATE TABLE side_cache (k TEXT); INSERT INTO side_cache VALUES ('x');").unwrap();
        }
        set_version(&(SCHEMA_VERSION + 1).to_string());
        let err = Store::open(&path).err().expect("a newer schema is refused").to_string();
        assert!(err.contains("newer knowcode") && err.contains("KNOWCODE_CACHE_DIR"), "{err}");
        // Untouched: the newer build's data is all there.
        let raw = Connection::open(&path).unwrap();
        let root: String = raw.query_row("SELECT value FROM meta WHERE key = 'root'", [], |r| r.get(0)).unwrap();
        assert_eq!(root, "/kept");
        raw.execute("UPDATE meta SET value = '1' WHERE key = 'schema_version'", []).unwrap();
        drop(raw);
        // Older: rebuilt from scratch, as this build's schema.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.meta("root").unwrap(), None);
        assert_eq!(store.meta("schema_version").unwrap(), Some(SCHEMA_VERSION.to_string()));
        let side: bool = store.conn().query_row("SELECT 1 FROM sqlite_master WHERE name = 'side_cache'", [], |_| Ok(true)).optional().unwrap().unwrap_or(false);
        assert!(!side);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The summary key moves with every edit that can change the workspace summary, and only with those: an edit
    /// inside a body keeps it, one adding a reference, moving a line or renaming changes it.
    #[test]
    fn invariant_the_summary_key_ignores_only_what_the_summary_never_reads() {
        let key = |src: &str| summary_key(Lang::CSharp, &kc_extract::extract_file(Lang::CSharp, "a.cs", src));
        let base = "namespace N;\npublic class A\n{\n    public int F(int x)\n    {\n        var s = \"ab\";\n        return x + 1;\n    }\n}\n";
        let k = key(base);
        // Bodies: a literal, an operator, a longer expression on the same line.
        assert_eq!(key(&base.replace("\"ab\"", "\"abcdef\"")), k);
        assert_eq!(key(&base.replace("x + 1", "x * 2 + 1")), k);
        for (what, edited) in [
            ("a new reference", base.replace("return x + 1;", "return Other.G(x);")),
            ("a moved line", base.replace("namespace N;", "namespace N;\n")),
            ("a renamed method", base.replace("F(int x)", "H(int x)")),
            ("a new member", base.replace("public class A\n{\n", "public class A\n{\n    public int P { get; set; }\n")),
        ] {
            assert_ne!(key(&edited), k, "{what}");
        }
    }

    /// A graph another thread is building is waited for, not built again: an ordering waits until its freshness
    /// deadline and then says it is pending, the graph itself until it is built.
    #[test]
    fn a_reference_graph_being_built_is_waited_for() {
        let dir = std::env::temp_dir().join(format!("knowcode-test-rank-turns-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("i.sqlite");
        let store = Store::open(&path).unwrap();
        let generation = store.generation().unwrap();
        let turn = rank::Turn::take(store.conn().path().unwrap(), &generation);

        let op = limits::Operation::new();
        op.set_freshness_deadline(Some(std::time::Instant::now() + std::time::Duration::from_millis(50)));
        let entered = limits::enter(op);
        let (ranked, at) = store.file_rank_at(&[], rank::Flow::Uses, true).unwrap();
        drop(entered);
        assert!(ranked.0.is_empty());
        assert_eq!(at.as_deref(), Some(rank::PENDING));
        let note = answer::stale_derived("order of references (app): file ranking", rank::PENDING, &generation);
        assert_eq!(note.reason, answer::Reason::Deadline);

        // Another thread's query waits for the graph this one builds, then reads it.
        let waiting = std::thread::spawn({
            let path = dir.join("i.sqlite");
            move || Store::open(&path).unwrap().ref_graph().unwrap().files.len()
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!waiting.is_finished());
        store.derived(&format!("ref_graph:{}", rank::RANK_VERSION), &generation, || rank::ref_graph(store.conn())).unwrap();
        drop(turn);
        assert_eq!(waiting.join().unwrap(), 0);
        assert!(store.file_rank_at(&[], rank::Flow::Uses, true).unwrap().1.is_none());
    }

    /// Constructs met but not read have their own table: never among the Ansible (or CI) facts, replaced and removed
    /// with their file.
    #[test]
    fn unsupported_constructs_are_kept_apart_from_infrastructure_facts() {
        let dir = std::env::temp_dir().join(format!("knowcode-test-unsupported-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = Store::open(&dir.join("i.sqlite")).unwrap();
        let src = "- hosts: all\n  tasks:\n    - include_tasks: \"{{ item }}.yml\"\n    - name: restart the api\n      service: name=api state=restarted\n";
        let facts = kc_extract::extract_file(Lang::Ansible, "site.yml", src);
        assert!(!facts.unsupported.is_empty() && !facts.ansible.is_empty(), "{facts:?}");
        let put = |store: &mut Store, facts: &FileFacts| {
            let b = store.batch().unwrap();
            b.put(&FileRecord { path: "site.yml", lang: Lang::Ansible, mtime_ns: 1, size: 1, hash: "h", lines: 5, facts }).unwrap();
            b.commit().unwrap();
        };
        put(&mut store, &facts);
        let count = |store: &Store, sql: &str| store.conn().query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ansible_facts WHERE kind = 'unsupported'"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ansible_facts"), facts.ansible.len() as i64);
        let sites = unsupported::all(store.conn()).unwrap();
        let want: Vec<(String, String, u32)> = facts.unsupported.iter().map(|u| (u.area.to_string(), u.construct.clone(), u.line)).collect();
        assert_eq!(sites.iter().map(|s| (s.area.clone(), s.construct.clone(), s.line)).collect::<Vec<_>>(), want);
        assert!(sites.iter().all(|s| s.file == "site.yml"));
        // Written again: replaced, not added to.
        put(&mut store, &facts);
        assert_eq!(unsupported::all(store.conn()).unwrap().len(), want.len());
        // Removed with its file.
        let id: i64 = store.conn().query_row("SELECT id FROM files WHERE path = 'site.yml'", [], |r| r.get(0)).unwrap();
        let b = store.batch().unwrap();
        b.remove(id).unwrap();
        b.commit().unwrap();
        assert!(unsupported::all(store.conn()).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An endpoint registration whose route can't be placed is kept as a construct not read (area `http`), anchored
    /// to its handler, and read back among the routes as an `unsupported` row; `in_span` leaves it to them.
    #[test]
    fn unplaced_routes_are_constructs_not_read() {
        let dir = std::env::temp_dir().join(format!("knowcode-test-unplaced-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = Store::open(&dir.join("i.sqlite")).unwrap();
        let src = "public static class Api\n{\n    public static void Map(WebApplication app)\n    {\n        app.MapGet(Paths.Item(), GetItem);\n        app.MapGet(\"/items\", ListItems);\n    }\n    static string GetItem(int id) => \"\";\n    static string ListItems() => \"\";\n}\n";
        let facts = kc_extract::extract_file(Lang::CSharp, "Api.cs", src);
        let b = store.batch().unwrap();
        b.put(&FileRecord { path: "Api.cs", lang: Lang::CSharp, mtime_ns: 1, size: 1, hash: "h", lines: 10, facts: &facts }).unwrap();
        b.commit().unwrap();
        let conn = store.conn();
        let stored: Vec<(String, String, u32, Option<String>)> = conn
            .prepare("SELECT u.area, u.construct, u.line, s.name FROM unsupported u LEFT JOIN symbols s ON s.id = u.symbol_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(stored, [("http".to_string(), "MapGet: its route template is not a constant".to_string(), 5, Some("GetItem".to_string()))]);
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM http_routes WHERE side = 'unsupported'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        let routes: Vec<String> = query::http_routes(conn)
            .unwrap()
            .into_iter()
            .map(|r| format!("{} {} {:?} {}:{} {:?}", r.side, r.verb, r.path, r.symbol, r.line, r.detail))
            .collect();
        assert_eq!(
            routes,
            [
                "unsupported MapGet None Api.GetItem:5 Some(\"its route template is not a constant\")",
                "server GET Some(\"items\") Api.ListItems:6 None",
            ]
        );
        assert!(unsupported::in_span(conn, "Api.cs", 1, 10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn splits_identifiers() {
        assert_eq!(name_tokens("loadHTTPSettings2"), "load http settings 2 loadhttpsettings2");
        assert_eq!(name_tokens("URLSession"), "url session urlsession");
        assert_eq!(name_tokens("items"), "items");
        assert_eq!(name_tokens("snake_case_name"), "snake case name snakecasename");
    }
}
