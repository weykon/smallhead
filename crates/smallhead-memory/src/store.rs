//! `LanceStore` — async handle to a LanceDB root.
//!
//! v0.5 PR-2: replaces the v0.5 scaffold (pure path management) with a real
//! `lancedb::Connection`. The disk-level path helpers are kept for backward
//! compatibility with the v0.5 tests; new code should use the async API.

use std::path::PathBuf;
use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema, SchemaRef};
use lancedb::{Connection, Table};

use crate::config::MemoryConfig;
use crate::error::MemoryError;
use crate::table::MemoryTable;

/// Vector dimension reserved by [`memory_table_schema`].
///
/// Matches the default `bge-small-zh-v1.5` model (fastembed-rs). Configurable
/// later once the embedding pipeline (B/C teams) lands; for now it's a const
/// so the schema is stable.
pub const EMBEDDING_DIM: i32 = 512;

/// Disk-level + LanceDB-connection handle to a memory root.
///
/// One [`LanceStore`] owns one root directory plus one open
/// [`lancedb::Connection`]. Each [`MemoryTable`] corresponds to one named
/// LanceDB table inside that root.
#[derive(Clone)]
pub struct LanceStore {
    /// The configuration the store was constructed with.
    pub config: MemoryConfig,
    /// Resolved root path (= `config.path`, post-`create_dir_all`).
    pub base_path: PathBuf,
    /// Open LanceDB connection to `base_path`. Always present after
    /// construction via either [`LanceStore::new`] (sync, deferred open — see
    /// `inner`) or [`LanceStore::connect`] (async, eagerly opened).
    ///
    /// `None` only for the rare sync-via-`new()` path; async callers should
    /// use [`LanceStore::connect`] so they can rely on `Some`.
    inner: Option<Arc<Connection>>,
}

/// Live, ready-to-use handle to one logical memory table.
///
/// The inner `lancedb::Table` is `pub` so downstream teams (B / C) can drive
/// `add` / `query` / `delete` directly against it without round-tripping
/// through `LanceStore` for every call.
pub struct MemoryHandle {
    /// Which logical table this handle represents.
    pub table: MemoryTable,
    /// The underlying LanceDB table.
    pub inner: Table,
}

impl LanceStore {
    // ---- Construction -----------------------------------------------------

    /// Create a new store rooted at `config.path`, creating the directory if
    /// it doesn't already exist.
    ///
    /// Sync, lazy: does **not** open the LanceDB connection. Preserved for
    /// the v0.5 sync tests; new code should use [`LanceStore::connect`].
    pub fn new(config: MemoryConfig) -> Result<Self, MemoryError> {
        let base = config.path.clone();
        std::fs::create_dir_all(&base)?;
        Ok(Self {
            config,
            base_path: base,
            inner: None,
        })
    }

    /// Async, eager: create the root directory if needed and open a real
    /// [`lancedb::Connection`] to it.
    ///
    /// This is the preferred entry point for new code — all async methods
    /// (`open_table`, `ensure_lance_table`, `drop_table`, `list_lance_tables`)
    /// rely on `inner == Some`.
    pub async fn connect(config: MemoryConfig) -> Result<Self, MemoryError> {
        let base = config.path.clone();
        std::fs::create_dir_all(&base)?;
        let uri = base
            .to_str()
            .ok_or_else(|| {
                MemoryError::LanceDb(format!(
                    "config.path is not valid UTF-8: {}",
                    base.display()
                ))
            })?
            .to_string();
        let conn = lancedb::connect(&uri)
            .execute()
            .await
            .map_err(|e| MemoryError::LanceDb(e.to_string()))?;
        Ok(Self {
            config,
            base_path: base,
            inner: Some(Arc::new(conn)),
        })
    }

    /// Borrow the underlying LanceDB connection. Panics if the store was
    /// constructed via [`LanceStore::new`] without a subsequent `connect` —
    /// use [`LanceStore::connect`] to guarantee a live connection.
    fn conn(&self) -> &Connection {
        self.inner
            .as_deref()
            .expect("LanceStore: no open connection; use LanceStore::connect()")
    }

    // ---- Legacy disk-level helpers (preserved for v0.5 tests) -------------

    /// Path on disk for a given [`MemoryTable`] under this store.
    pub fn table_path(&self, table: MemoryTable) -> PathBuf {
        self.base_path.join(table.dir_name())
    }

    /// Scan the store root and return the set of known [`MemoryTable`]s whose
    /// subdirectory already exists. Order matches [`MemoryTable::all`].
    pub fn list_tables(&self) -> Result<Vec<MemoryTable>, MemoryError> {
        let mut present: Vec<MemoryTable> = Vec::new();
        for entry in std::fs::read_dir(&self.base_path)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(t) = MemoryTable::from_dir_name(&name) {
                present.push(t);
            }
        }
        let mut out = Vec::new();
        for t in MemoryTable::all() {
            if present.contains(t) {
                out.push(*t);
            }
        }
        Ok(out)
    }

    /// Make sure the subdirectory for `table` exists, returning its path.
    ///
    /// This is the v0.5 sync filesystem helper. For LanceDB table creation
    /// see [`LanceStore::ensure_lance_table`].
    pub fn ensure_table(&self, table: MemoryTable) -> Result<PathBuf, MemoryError> {
        let p = self.table_path(table);
        std::fs::create_dir_all(&p)?;
        Ok(p)
    }

    // ---- Async LanceDB API (new) -----------------------------------------

    /// Open an existing LanceDB table, returning `Ok(None)` if it doesn't
    /// exist yet (callers who want create-if-missing should use
    /// [`LanceStore::ensure_lance_table`]).
    pub async fn open_table(
        &self,
        table: MemoryTable,
    ) -> Result<Option<MemoryHandle>, MemoryError> {
        let name = table.dir_name();
        match self.conn().open_table(name).execute().await {
            Ok(t) => Ok(Some(MemoryHandle { table, inner: t })),
            Err(lancedb::Error::TableNotFound { .. }) => Ok(None),
            Err(e) => Err(MemoryError::LanceDb(e.to_string())),
        }
    }

    /// Open `table` if it exists; otherwise create an empty LanceDB table
    /// with [`memory_table_schema`] and return a handle to it.
    pub async fn ensure_lance_table(
        &self,
        table: MemoryTable,
    ) -> Result<MemoryHandle, MemoryError> {
        if let Some(h) = self.open_table(table).await? {
            return Ok(h);
        }
        let schema = memory_table_schema();
        let tbl = self
            .conn()
            .create_empty_table(table.dir_name(), schema)
            .mode(lancedb::database::CreateTableMode::Create)
            .execute()
            .await
            .map_err(|e| MemoryError::LanceDb(e.to_string()))?;
        Ok(MemoryHandle { table, inner: tbl })
    }

    /// Drop the LanceDB table for `table`. Idempotent: dropping a missing
    /// table is a no-op.
    pub async fn drop_table(&self, table: MemoryTable) -> Result<(), MemoryError> {
        let name = table.dir_name();
        match self.conn().drop_table(name).await {
            Ok(()) => Ok(()),
            Err(lancedb::Error::TableNotFound { .. }) => Ok(()),
            Err(e) => Err(MemoryError::LanceDb(e.to_string())),
        }
    }

    /// List all LanceDB table names known to this store's connection.
    pub async fn list_lance_tables(&self) -> Result<Vec<String>, MemoryError> {
        self.conn()
            .table_names()
            .execute()
            .await
            .map_err(|e| MemoryError::LanceDb(e.to_string()))
    }
}

/// The shared arrow [`Schema`] used for every memory table.
///
/// Columns:
/// - `id`: utf8 — uuid-v7 string (assigned by ingest, B-team)
/// - `text`: utf8 — raw chunk text
/// - `vector`: fixed-size list of float32, dim = [`EMBEDDING_DIM`] —
///   reserved for `bge-small-zh-v1.5` (fastembed-rs)
/// - `ts`: int64 — milliseconds since unix epoch
pub fn memory_table_schema() -> SchemaRef {
    let vector_field = Field::new("item", DataType::Float32, true);
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(vector_field), EMBEDDING_DIM),
            true,
        ),
        Field::new("ts", DataType::Int64, false),
    ]))
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    #[test]
    fn schema_has_expected_columns() {
        let s = memory_table_schema();
        let names: Vec<&str> = s.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, vec!["id", "text", "vector", "ts"]);
    }

    #[test]
    fn schema_vector_dim_is_512() {
        let s = memory_table_schema();
        let v = s.field_with_name("vector").unwrap();
        match v.data_type() {
            DataType::FixedSizeList(inner, dim) => {
                assert_eq!(*dim, 512);
                assert_eq!(inner.data_type(), &DataType::Float32);
            }
            other => panic!("expected FixedSizeList, got {other:?}"),
        }
    }
}
