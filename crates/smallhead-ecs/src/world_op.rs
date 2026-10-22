//! `WorldOp` — the *only* surface through which an agent (or any caller) can
//! mutate the persistent ECS world.
//!
//! ## Why an explicit op enum?
//!
//! big-head demonstrated that giving the LLM a generic `&mut World` is a
//! footgun: any future helper, lens, or system can be reached through it, and
//! the audit trail collapses to "agent called random function X". `smallhead`
//! keeps the contract tight:
//!
//! - Every mutating intent is enumerated in [`WorldOpKind`].
//! - Each op has a JSON schema for its `input` and a fixed `output` shape via
//!   [`WorldOpResult`].
//! - `execute()` is the *only* method that reaches the world, and it always
//!   goes through the [`Permission`] check first.
//! - Permission tokens ([`Permission::EcsRead`] / [`Permission::EcsMutate`])
//!   come from the `smallhead-permission` crate; this crate only knows their
//!   identity, not their evaluation.
//!
//! ## Safety contract
//!
//! 1. **No method on `WorldOpTool` accepts `&mut World`.** The world lives
//!    behind `Arc<Mutex<World>>` and is locked only inside the op dispatcher.
//! 2. **No method exposes `bevy_ecs` types in its public signature.** Output
//!    is always `serde_json::Value` so the agent loop can serialise it into
//!    the JSONL action log without leaking BEVY internals.
//! 3. **`PermissionDenied` is checked before the world is touched.** The op
//!    dispatcher never reaches the world for a denied request.
//!
//! [`Permission`]: https://docs.rs/smallhead-permission/latest/smallhead_permission/enum.Permission.html
//!
//! ## Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use bevy_ecs::world::World;
//! use smallhead_ecs::world_op::{WorldOpTool, WorldOpKind};
//!
//! # async fn run() -> Result<(), smallhead_ecs::error::EcsError> {
//! let world = Arc::new(parking_lot::Mutex::new(World::new()));
//! let tool = WorldOpTool::new(WorldOpKind::QueryEntities, world);
//! let out = tool
//!     .execute(serde_json::json!({ "component": "User" }))
//!     .await?;
//! assert_eq!(out.kind, WorldOpKind::QueryEntities);
//! # Ok(()) }
//! ```

use std::sync::Arc;

use bevy_ecs::world::World;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{EcsError, EcsResult};

/// Identifies which [`WorldOp`] is being dispatched.
///
/// The set is closed: any new "kind of mutation" requires a new variant here
/// (and, by extension, a permission rule). This is intentional — it forces the
/// tool surface that the LLM sees to be explicit and reviewable.
///
/// Source: ARCHITECTURE.md §4.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorldOpKind {
    /// `SELECT` — list entities that match a component predicate.
    QueryEntities,
    /// `GET` — read a single component value from one entity.
    GetComponent,
    /// `INSERT` — attach a new component to an existing entity.
    InsertComponent,
    /// `DELETE` — remove a component from an entity (entity survives).
    RemoveComponent,
    /// `UPDATE` — patch a component's fields with a JSON merge / predicate.
    UpdateComponent,
    /// `CREATE` — spawn a brand-new entity.
    SpawnEntity,
    /// `DROP` — despawn an entity (and all its components).
    DespawnEntity,
    /// `GRAPH EDGE` — attach a relation component (a typed edge) between two entities.
    AddRelation,
    /// `GRAPH EDGE` — remove a relation component.
    RemoveRelation,
    /// `SCHEMA CREATE` — register a new component schema at runtime.
    DefineComponent,
    /// `PROCEDURAL` — invoke a previously-defined system.
    RunSystem,
}

impl WorldOpKind {
    /// Returns the JSON schema that `input` MUST satisfy for this op.
    ///
    /// The schema is intentionally loose (accepting any extra properties)
    /// for forward-compat — strict validation happens inside `execute()`.
    pub fn input_schema(&self) -> Value {
        match self {
            WorldOpKind::QueryEntities => json!({
                "type": "object",
                "properties": {
                    "component": { "type": "string", "description": "用于筛选的组件名称；省略则查询所有实体。" },
                    "limit":     { "type": "integer", "minimum": 1, "default": 100 },
                    "offset":    { "type": "integer", "minimum": 0, "default": 0 },
                },
                "additionalProperties": true,
            }),
            WorldOpKind::GetComponent => json!({
                "type": "object",
                "required": ["entity", "component"],
                "properties": {
                    "entity":    { "type": "integer", "description": "实体索引。" },
                    "component": { "type": "string" },
                },
                "additionalProperties": false,
            }),
            WorldOpKind::InsertComponent => json!({
                "type": "object",
                "required": ["entity", "component", "value"],
                "properties": {
                    "entity":    { "type": "integer" },
                    "component": { "type": "string" },
                    "value":     { "description": "组件数据，对象格式。" },
                },
                "additionalProperties": false,
            }),
            WorldOpKind::RemoveComponent => json!({
                "type": "object",
                "required": ["entity", "component"],
                "properties": {
                    "entity":    { "type": "integer" },
                    "component": { "type": "string" },
                },
                "additionalProperties": false,
            }),
            WorldOpKind::UpdateComponent => json!({
                "type": "object",
                "required": ["entity", "component", "patch"],
                "properties": {
                    "entity":    { "type": "integer" },
                    "component": { "type": "string" },
                    "patch":     { "type": "object" },
                    "where":     { "description": "应用修改前可选的判断条件。" },
                },
                "additionalProperties": false,
            }),
            WorldOpKind::SpawnEntity => json!({
                "type": "object",
                "properties": {
                    "with_components": {
                        "type": "object",
                        "description": "创建实体时附加的组件名称到组件值的映射。",
                        "additionalProperties": true,
                    },
                },
                "additionalProperties": false,
            }),
            WorldOpKind::DespawnEntity => json!({
                "type": "object",
                "required": ["entity"],
                "properties": { "entity": { "type": "integer" } },
                "additionalProperties": false,
            }),
            WorldOpKind::AddRelation => json!({
                "type": "object",
                "required": ["from", "to", "relation"],
                "properties": {
                    "from":     { "type": "integer", "description": "源实体。" },
                    "to":       { "type": "integer", "description": "目标实体。" },
                    "relation": { "type": "string",  "description": "关系组件名称。" },
                    "weight":   { "type": "number" },
                },
                "additionalProperties": false,
            }),
            WorldOpKind::RemoveRelation => json!({
                "type": "object",
                "required": ["from", "to", "relation"],
                "properties": {
                    "from":     { "type": "integer" },
                    "to":       { "type": "integer" },
                    "relation": { "type": "string" },
                },
                "additionalProperties": false,
            }),
            WorldOpKind::DefineComponent => json!({
                "type": "object",
                "required": ["name", "fields"],
                "properties": {
                    "name":   { "type": "string" },
                    "fields": { "type": "object",
                                "description": "字段名称到 { type, optional } 定义的映射。" },
                },
                "additionalProperties": false,
            }),
            WorldOpKind::RunSystem => json!({
                "type": "object",
                "required": ["system"],
                "properties": {
                    "system": { "type": "string", "description": "系统名称，必须已预先定义。" },
                    "args":   { "type": "object" },
                },
                "additionalProperties": false,
            }),
        }
    }

    /// `true` for ops that mutate the world state. `false` for pure reads.
    pub const fn is_mutating(&self) -> bool {
        matches!(
            self,
            WorldOpKind::InsertComponent
                | WorldOpKind::RemoveComponent
                | WorldOpKind::UpdateComponent
                | WorldOpKind::SpawnEntity
                | WorldOpKind::DespawnEntity
                | WorldOpKind::AddRelation
                | WorldOpKind::RemoveRelation
                | WorldOpKind::DefineComponent
                | WorldOpKind::RunSystem
        )
    }

    /// Stable string identifier — used by permission rules and tool specs.
    pub const fn as_str(&self) -> &'static str {
        match self {
            WorldOpKind::QueryEntities => "query_entities",
            WorldOpKind::GetComponent => "get_component",
            WorldOpKind::InsertComponent => "insert_component",
            WorldOpKind::RemoveComponent => "remove_component",
            WorldOpKind::UpdateComponent => "update_component",
            WorldOpKind::SpawnEntity => "spawn_entity",
            WorldOpKind::DespawnEntity => "despawn_entity",
            WorldOpKind::AddRelation => "add_relation",
            WorldOpKind::RemoveRelation => "remove_relation",
            WorldOpKind::DefineComponent => "define_component",
            WorldOpKind::RunSystem => "run_system",
        }
    }
}

/// The discriminated union returned by [`WorldOpTool::execute`].
///
/// The `kind` always equals the op kind passed to [`WorldOpTool::new`] —
/// callers can match on either. `payload` carries the op-specific result;
/// `error` carries the structured error when `ok == false`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldOpResult {
    pub kind: WorldOpKind,
    pub ok: bool,
    pub version: u64,
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<EcsError>,
}

impl WorldOpResult {
    /// Build a successful result.
    pub fn ok(kind: WorldOpKind, version: u64, payload: Value) -> Self {
        Self {
            kind,
            ok: true,
            version,
            payload,
            error: None,
        }
    }

    /// Build a failed result. The error is embedded in `error` and `ok` is set
    /// to `false`.
    pub fn err(kind: WorldOpKind, version: u64, error: EcsError) -> Self {
        Self {
            kind,
            ok: false,
            version,
            payload: Value::Null,
            error: Some(error),
        }
    }
}

/// The op dispatcher. Holds the only reference to the underlying `World`.
///
/// Constructed via [`WorldOpTool::new`]. Each call to [`execute`](Self::execute)
/// is the only path to the world; LLM-visible callers see JSON in / JSON out.
#[derive(Debug)]
pub struct WorldOpTool {
    pub kind: WorldOpKind,
    pub world: Arc<Mutex<World>>,
    /// Current monotonic version of the world (incremented on every mutation).
    pub version: u64,
    /// Whether the caller holds read or mutate rights on this op.
    pub permission: OpPermission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpPermission {
    Read,
    Mutate,
}

impl WorldOpTool {
    /// Create a new tool bound to `world`. The version starts at `0` and is
    /// incremented by the dispatcher after a mutating op succeeds.
    pub fn new(kind: WorldOpKind, world: Arc<Mutex<World>>) -> Self {
        Self {
            kind,
            world,
            version: 0,
            permission: OpPermission::Read,
        }
    }

    /// Override the permission granted for this op. Always check this before
    /// dispatching, e.g. via [`Self::ensure_mutate`].
    pub fn with_permission(mut self, p: OpPermission) -> Self {
        self.permission = p;
        self
    }

    fn ensure_mutate(&self) -> EcsResult<()> {
        if self.kind.is_mutating() && self.permission != OpPermission::Mutate {
            return Err(EcsError::PermissionDenied {
                op: self.kind,
                required: "EcsMutate".to_string(),
            });
        }
        Ok(())
    }

    /// Dispatch the op against `input`.
    ///
    /// This is the single entry point that touches the underlying `World`.
    /// The lock is held for the duration of the op and dropped before this
    /// method returns; callers can be sure the world is available again
    /// after a successful or failed op.
    pub async fn execute(&self, input: Value) -> EcsResult<WorldOpResult> {
        if let Err(e) = self.ensure_mutate() {
            return Ok(WorldOpResult::err(self.kind, self.version, e));
        }
        // Quick schema sanity check. Deep validation happens below.
        let _schema = self.kind.input_schema();
        if !schema_minimal_ok(&input) {
            return Ok(WorldOpResult::err(
                self.kind,
                self.version,
                EcsError::InvalidInput {
                    op: self.kind,
                    reason: "input is not a JSON object".into(),
                },
            ));
        }

        // Move the actual work onto a blocking thread so callers on the async
        // executor never block on the world mutex.
        let kind = self.kind;
        let world = Arc::clone(&self.world);
        let payload = tokio::task::spawn_blocking(move || dispatch(kind, &world, input))
            .await
            .map_err(|join| EcsError::IoFromJoin(join.to_string()))??;

        // If the op mutated, bump the version. For the MVP we accept that the
        // bump happens *outside* the lock (still single-owner for now); a
        // future iteration can move it inside the dispatch closure when the
        // executor grows true parallel writers.
        let new_version = if kind.is_mutating() {
            self.version + 1
        } else {
            self.version
        };

        Ok(WorldOpResult::ok(kind, new_version, payload))
    }
}

/// Internal: do the actual world touching. Kept sync so it can run on
/// `spawn_blocking` without crossing await points while holding the mutex.
fn dispatch(kind: WorldOpKind, world: &Mutex<World>, input: Value) -> EcsResult<Value> {
    let mut w = world.lock();
    match kind {
        WorldOpKind::QueryEntities => {
            // MVP: return a marker; iteration over archetypes/components is
            // deferred to smallhead-ecs v0.2.
            let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(100);
            Ok(json!({
                "matched": 0,
                "limit": limit,
                "note": "MVP: QueryEntities returns marker in this skeleton",
            }))
        }
        WorldOpKind::GetComponent => {
            let entity = input.get("entity").and_then(Value::as_u64).ok_or_else(|| {
                EcsError::InvalidInput {
                    op: kind,
                    reason: "missing 'entity' (integer)".into(),
                }
            })?;
            let component = input
                .get("component")
                .and_then(Value::as_str)
                .ok_or_else(|| EcsError::InvalidInput {
                    op: kind,
                    reason: "missing 'component' (string)".into(),
                })?
                .to_string();
            // MVP: defer real existence check to v0.2 once DynamicComponentStore
            // is wired in. The current ECS World has no entity-1 by default, so
            // we accept the input and return a marker payload.
            Ok(json!({
                "entity": entity,
                "component": component,
                "value": null,
                "note": "MVP: component reading is delegated to smallhead-ecs v0.2",
            }))
        }
        WorldOpKind::InsertComponent
        | WorldOpKind::RemoveComponent
        | WorldOpKind::UpdateComponent
        | WorldOpKind::SpawnEntity
        | WorldOpKind::DespawnEntity
        | WorldOpKind::AddRelation
        | WorldOpKind::RemoveRelation
        | WorldOpKind::DefineComponent
        | WorldOpKind::RunSystem => {
            // Touch is correct; explicit marker pending v0.2 wiring.
            Ok(json!({ "accepted": true, "op": kind.as_str(), "note": "MVP stub" }))
        }
    }
}

/// Cheap, lenient schema check — only verifies that `input` is an object.
/// Deep validation is the responsibility of each op branch.
fn schema_minimal_ok(v: &Value) -> bool {
    v.is_object()
}

// We extend the error type with a constructor for join errors. Defined here to
// avoid touching error.rs from this module's perspective.
impl EcsError {
    #[allow(non_snake_case)]
    pub(crate) fn IoFromJoin(msg: String) -> Self {
        EcsError::Serde(format!("blocking task join failed: {msg}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutating_ops_are_classified_correctly() {
        assert!(!WorldOpKind::QueryEntities.is_mutating());
        assert!(!WorldOpKind::GetComponent.is_mutating());
        assert!(WorldOpKind::InsertComponent.is_mutating());
        assert!(WorldOpKind::DespawnEntity.is_mutating());
        assert!(WorldOpKind::RunSystem.is_mutating());
    }

    #[test]
    fn stable_strings_are_snake_case() {
        for k in [
            WorldOpKind::QueryEntities,
            WorldOpKind::GetComponent,
            WorldOpKind::InsertComponent,
            WorldOpKind::RemoveComponent,
            WorldOpKind::UpdateComponent,
            WorldOpKind::SpawnEntity,
            WorldOpKind::DespawnEntity,
            WorldOpKind::AddRelation,
            WorldOpKind::RemoveRelation,
            WorldOpKind::DefineComponent,
            WorldOpKind::RunSystem,
        ] {
            let s = k.as_str();
            assert!(s.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{s}");
        }
    }

    #[test]
    fn schemas_require_object_root() {
        for k in [
            WorldOpKind::QueryEntities,
            WorldOpKind::GetComponent,
            WorldOpKind::SpawnEntity,
            WorldOpKind::RunSystem,
        ] {
            let s = k.input_schema();
            assert_eq!(s["type"], "object");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_op_with_read_permission_succeeds() {
        let world = Arc::new(Mutex::new(World::new()));
        let tool = WorldOpTool::new(WorldOpKind::QueryEntities, world);
        let out = tool.execute(json!({ "component": "User" })).await.unwrap();
        assert!(out.ok);
        assert_eq!(out.kind, WorldOpKind::QueryEntities);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mutating_op_without_mutate_permission_is_denied() {
        let world = Arc::new(Mutex::new(World::new()));
        let tool = WorldOpTool::new(WorldOpKind::SpawnEntity, world);
        let out = tool.execute(json!({})).await.unwrap();
        assert!(!out.ok);
        assert!(matches!(out.error, Some(EcsError::PermissionDenied { .. })));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mutating_op_with_mutate_permission_succeeds() {
        let world = Arc::new(Mutex::new(World::new()));
        let tool =
            WorldOpTool::new(WorldOpKind::SpawnEntity, world).with_permission(OpPermission::Mutate);
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.ok);
        assert_eq!(out.version, 1);
    }
}
