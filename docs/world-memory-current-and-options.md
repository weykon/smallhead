# World and memory: current implementation and open options

Research snapshot: 2026-10-16. This is a discussion document, not an accepted design or implementation plan. Inspection was read-only in both repositories; this document is the only change made for this task. Findings come from source and existing tests, not a fresh execution of the memory/ECS/big-head test suites or live embedding inference.

Follow-up: [agenthand paper research and a concrete maintenance option](world-memory-research-notes.md), based on the October 15 research conversation and focused readings of the local original PDFs. The follow-up is also discussion only.

The direction under discussion is shared world prompt records with agent-managed relations. It does **not** require introducing a new Session/World hierarchy, mapping every conversation to a separate world, or replacing current session persistence. Record scope, relation semantics, and automatic retrieval are undecided. Lens is deferred.

## What small-head actually has

| Surface | Implemented | Application wiring / qualification |
| --- | --- | --- |
| `smallhead-memory` storage | Real async LanceDB connection, table creation/open/list/drop, Arrow schema | No production caller found outside the crate; the TUI, core, and server do not depend on it |
| Ingestion and search | Character-window chunking, embedding batches, append rows, vector nearest-neighbor search | Library implementation and tests exist; no automatic conversation ingestion or registered memory tool found |
| Local embeddings | Fastembed BGE-small-ZH-v1.5, 512 dimensions | Construction can require model/runtime downloads; real-model tests are ignored by default |
| Memory configuration | YAML defaults/merge/validation; ingest, search, retention settings | Many settings describe future policy rather than behavior applied by the pipeline |
| `smallhead-ecs::WorldOpTool` | Eleven operation kinds, JSON schemas, read/mutate permission check, mutex and blocking dispatch | Dispatch is still a stub; no runtime tool registration found in core/TUI/server |
| ECS checkpoint store | Versioned JSON files, temp write/file sync/rename, expected-version comparison, list/read/latest | Persists opaque caller JSON; not a serializer/restore path for a live ECS world; no application caller found |
| TUI recovery and session storage | Session messages, pending steering, interruption recovery, spec records/comments/approval | Actively wired; these are not ECS checkpoints or cross-project semantic memory |
| Core artifacts | Large tool-result and compacted-history artifacts, bounded projections, artifact reads | Actively usable in TUI; useful existing source material, but not a discovery/retrieval index |
| `ContextRef::MemoryHit` | Serializable reference carrying source, row ID, summary | Protocol vocabulary, not a memory query implementation or evidence that results enter prompts |

### Memory is beyond the scaffold, but not integrated

The opening comments of [memory/lib.rs](../crates/smallhead-memory/src/lib.rs) and the package description still say scaffold and claim real connections/ingest/search are absent. That is stale. [store.rs](../crates/smallhead-memory/src/store.rs) implements `LanceStore::connect`, and [ingest.rs](../crates/smallhead-memory/src/ingest.rs) implements `IngestPipeline::ingest` and `search` against actual Lance tables.

Important limits in the current code:

- All three logical tables (`agent_memory`, `personal_rag`, `ecs_checkpoints`) use the same `id`, `text`, `vector`, `ts` schema. `EcsCheckpoints` is a table name, not an integration with `FileCheckpointStore`.
- `ts` is ingestion time in Unix milliseconds. It does not capture source publication time, when an event happened, or when a statement ceased to apply.
- Ingestion creates fresh UUIDs for chunks and appends them. There is no source-record identity, artifact reference, project scope, source revision, chunk-offset persistence, deduplication/upsert policy, or relation model in those rows.
- Search takes explicit `k`, embeds the query, and calls vector search. `Hit.score` is distance, lower being closer; it is not confidence or truth. There is no BM25 blend, project/time filter, or recency re-ranking in this path.
- `hybrid_ratio`, `recency_decay_days`, `retain_days`, and `SessionMessages` source configuration do not install retrieval, deletion, or ingestion jobs. Configuration alone does not activate those behaviors.
- The schema is fixed at 512 dimensions; configuration is not a general backend/model factory. MiniMax variants are reserved configuration values, not implemented hosted embedding/storage integrations.
- [chunker.rs](../crates/smallhead-memory/src/chunker.rs) uses Unicode scalar counts, not tokenizer tokens, despite token wording in parts of the config. `Chunker` is in a private module and is not re-exported, although the public pipeline constructor requires it; the crate needs an accessible construction path before ordinary external callers can assemble the pipeline.
- There is a validation gap: `build_record_batch` substitutes a zero vector for a wrong-length embedding instead of returning the error its comments suggest. This should not be mistaken for validated embedding quality.
- Synchronous `LanceStore::new` only prepares the path. Async table methods expect a connection created with `connect`; using the lazy handle for those methods can panic.

[tests.rs](../crates/smallhead-memory/src/tests.rs) includes real-store tests with mock embeddings and ignored real embedding tests. These establish intended library coverage, not production wiring or a current test-run result.

### ECS operation names do not yet imply working world operations

[world_op.rs](../crates/smallhead-ecs/src/world_op.rs) declares query/get/insert/remove/update/spawn/despawn/relation/definition/system operations. In `dispatch`, query returns `matched: 0` with a skeleton marker, get returns `value: null`, and mutations return `accepted: true` with `MVP stub`. They do not perform the named mutations.

The reported mutation version is `self.version + 1`; `execute(&self)` does not update that field or maintain a shared world revision. Repeated calls are not a durable monotonic world history. General input validation checks only that input is an object, with a few additional get-component checks. The declared JSON schemas are not fully enforced there.

[checkpoint.rs](../crates/smallhead-ecs/src/checkpoint.rs) is more substantial: it persists `WorldCheckpoint { world_id, version, world, action_log_digest, timestamp, anchor_refs }`. However:

- `world` is caller-provided JSON, not captured ECS state. The store does not apply it back to Bevy.
- `action_log_digest` and `anchor_refs` are supplied by callers; the store does not establish the claimed relationship to an action log or source entries.
- Expected-version checking and file rename are not protected by a shared writer lock. Atomic replacement avoids partial files, but does not make the check-and-write sequence safe against concurrent writers.
- Parent-directory sync errors are ignored and metadata updates are best-effort. This is weaker than an unconditional crash-durability guarantee.
- The `.jsonl` checkpoint files contain serialized checkpoint JSON, not an implemented event log.

These are integration constraints to revisit if this code is reused, not a reason to impose a new world abstraction now. [ARCHITECTURE.md](../ARCHITECTURE.md) describes the intended ECS-centric design and even sketches a `WorldOp` tool variant; that text is not evidence of present application behavior.

### What currently reaches the applications

The inspected Cargo manifests for core, TUI, and server have no `smallhead-memory` or `smallhead-ecs` dependency. Searches for `LanceStore`, `IngestPipeline`, `WorldOpTool`, and `FileCheckpointStore` found implementations/tests rather than application call sites.

- [TUI runtime](../crates/smallhead-tui/src/runtime.rs) registers concrete task, choice, plan, spec, fast-decision, search, shell, process, read, and artifact capabilities. It does not register memory ingestion/search or ECS world operations. Fast-decision examples that rank “memory” candidates do not fetch stored memories.
- [TUI App](../crates/smallhead-tui/src/app.rs) persists/restores existing session data and uses [turn_recovery.rs](../crates/smallhead-tui/src/turn_recovery.rs) for messages, pending steering, and interruption state. [spec_review.rs](../crates/smallhead-tui/src/spec_review.rs) persists review records under the caller's artifact root. These are concrete durable records worth reusing as sources.
- [Core artifact storage](../crates/smallhead-core/src/artifact.rs) preserves large outputs and compacted history with content-addressed references; current artifact roots and lookup rules still matter. A hash/reference is not automatically a globally resolvable cross-session reference.
- [Monad protocol](../crates/smallhead-core/src/monad/protocol.rs) has `UserQuote`, `PrevResult`, and `MemoryHit` context references. The inspected `MemoryHit` uses are declarations/serialization tests, not LanceDB retrieval.
- [Server driver](../crates/smallhead-server/src/driver.rs) builds the headless A2A loop with `MonadBridge`, conversation history, and an empty `relevant_context` list. [a2a.rs](../crates/smallhead-server/src/a2a.rs) manages conversation/session mirroring; neither path opens the memory store or world checkpoint store.

Persisting a conversation and recovering an interrupted task already work independently of the proposed shared memory. The discussion should not rename those mechanisms or require users to create another session/world container.

## Relevant big-head precedents, read only

The sibling repository is substantially further along in ECS-backed runtime state. Its concepts are useful references, not a ready-made small-head design.

| Area | Observed implementation | Useful lesson / boundary |
| --- | --- | --- |
| Spark | [Spark component](../../big-head/crates/big-head-ecs/src/components/spark.rs) stores text, epoch timestamp, status, cycle ID, result summary, optional ambient source reference. [Runtime](../../big-head/crates/big-head-loop/src/runtime.rs) creates ECS entities through `push_spark`/`push_spark_front` and queues them | A prompt can remain a durable, inspectable record. Spark also schedules work; merely remembering a prompt need not enqueue it |
| World persistence | Runtime checkpoint calls [big-head-save](../../big-head/crates/big-head-save/src/persistence.rs), which captures registered world data; persistence includes content-addressed blobs and journal replay on restore | The implemented ECS checkpoint path is materially different from small-head's opaque JSON checkpoint store |
| Context artifacts | [context_artifact.rs](../../big-head/crates/big-head-loop/src/context_artifact.rs) stores lossless UTF-8 content in bounded `ShardData` chunks, keyed by namespace/content digest, and checks/reuses existing content | Keep original source available behind compact model-facing context; do not treat a summary as the only surviving record |
| Agent-authored facts | [Tool dispatch](../../big-head/crates/big-head-loop/src/tools/dispatch.rs) routes `record_fact`, `record_interpretation`, and materialization operations to implementation modules, alongside real ECS tools | Observation, agent interpretation, and action can be distinguished rather than silently merging them into one truth field |
| Work records and connections | [Work-world implementation](../../big-head/crates/big-head-loop/src/work_world.rs) and [prototype documentation](../../big-head/docs/work-world.md) use stable work IDs, evidence snapshots, revisions, and directed connections | Reuse the provenance idea without importing the city/book presentation, project ontology, or demo evidence catalog |

The work-world prototype explicitly says its initial evidence is a reviewed historical import, not automatic Git/chat/test ingestion. A captured quotation or digest proves where bytes came from, not that an agent's interpretation is correct. Its guards also do not make broad ECS/script access a security boundary.

### Lens is two different things here, and both are deferred

[Lens World Protocol](../../big-head/docs/lens-world-protocol.md) describes cross-world exchange objects and inbox/outbox buckets. Receiving/accepting an exchange does not automatically install local facts. It has its own world identity and transport boundaries; those should not be projected onto this shared-memory discussion.

[Local Lens applications](../../big-head/docs/lens-applications.md), [components](../../big-head/crates/big-head-ecs/src/lens.rs), and [bindings](../../big-head/crates/big-head-loop/src/lens_bindings.rs) describe/implement versioned read-only context transformations, source references, receipts, and optional coordinator `before_reasoning` bindings. They are not synonymous with cross-world exchange. In particular, observation time is not source freshness, and rerunning a transformation does not refresh its source.

Those provenance/time distinctions are useful now. Lens definitions, bindings, caches, learning, transport, and sovereign world separation are **not** proposed requirements for small-head's next step.

## Minimal options to discuss, not a finalized schema

One possible starting point is a shared catalog of prompt records and source artifacts, accessible from existing conversations and agents. “Shared world” could name that body of records without deciding whether its physical implementation is files, a database, ECS entities, or a combination. Existing session IDs can remain provenance rather than becoming a new ownership hierarchy.

Candidate information for a record:

| Information | Why it could be useful | Decision still open |
| --- | --- | --- |
| Stable record ID and source revision/digest | Refer to the same prompt or artifact across turns; detect changed sources | Whether ID belongs to each utterance, request, or larger document |
| Exact source text or resolvable artifact reference | Preserve what was actually said/read, with bounded previews | Which source types are captured automatically and which are selected |
| Provenance | Existing session/message/task/tool-call identifiers, author/origin, source path or URL, excerpt offsets where applicable | Minimum metadata for imported files and older history with missing IDs |
| Time | Separate capture time, source/event time when known, and update/supersession time | Whether valid-from/valid-until is needed initially; unknown source time stays unknown |
| Project scope | Retrieve within the intended project while allowing explicitly shared records | Project identity, default scope, and whether one record can belong to multiple projects |
| Agent annotations | Summaries, labels, interpretations, and relation suggestions attributable to their creator | Which writes are automatic, reviewable, or user-confirmed |

This is a list of useful properties, not a required new Rust type. Raw user wording should remain distinguishable from model-produced summaries. Imported prompts are historical material; retrieving one should not silently turn it into a new active instruction or execution request.

### Agent-managed relations

Relations could be separate records pointing to stable record/artifact IDs, carrying creator, creation time, and supporting source references. Possible meanings include continuation, elaboration, contradiction, supersession, or common project/topic. These are examples, not a fixed relation vocabulary.

The unresolved choice is how much structure the agent may invent and revise. Free-form labels are flexible; a small shared vocabulary is easier to query consistently. Either can retain the original prompt and make the relation attributable/retractable. An agent asserting “supersedes” should not physically erase the previous statement or silently convert a hypothesis into a user decision. Whether relation edits need revisions or approval should follow the desired workflow, not the fact that ECS has mutation APIs.

### Artifacts and retrieval

A small, independently useful retrieval path could be:

1. Select the requested project/shared scope and any explicit time range.
2. Find prompt/artifact candidates using direct IDs, metadata, and text search; optionally add vector search once source links and scope filters exist.
3. Return bounded excerpts with record ID, source reference/revision, timestamps, scope, and an explanation of selection. Allow full source reads through a resolvable artifact reference.
4. Let the agent inspect sources and use or revise relations. Keep retrieval results distinct from approval, verified facts, and new user commands.

This does not require putting all history into every prompt. Explicit agent retrieval, a small automatic context preview, and a combination are separate options. Related-record expansion should have an explicit bound; whether it happens automatically is still open.

The vector index can be a rebuildable derivative of source records, rather than the only place original material survives. Current `smallhead-memory` lacks the provenance/scope columns needed for this; directly indexing every message now would lose useful attribution and make repeated ingestion ambiguous. Existing artifact storage offers a starting point, but cross-root resolution and ownership must be decided before sharing references across projects.

Neither ECS integration nor embeddings must be the first implementation commitment. Conversely, this document does not rule them out: a later choice can use actual query needs to decide whether files/catalog storage, LanceDB indexing, or an ECS representation earns its complexity.

## Questions for discussion

1. What should a prompt record represent: every user message, an explicitly captured request, a whole exchange, or a document? Should agent-generated prompts be included but marked separately?
2. Does “shared” mean all local agents see one catalog by default, or that agents can explicitly consult records beyond the current project? Is a cross-project reference enough, or should content be copied?
3. What identifies a project: repository root, a user-selected label, an existing task context, or something else? How should moved repositories and multi-project requests behave?
4. Should source capture be automatic while annotations/relations remain agent-managed, or should the agent decide what to remember? What should the user be able to inspect, correct, exclude, or delete?
5. Can agents invent relation types? Are contradictions/supersessions merely attributed claims, or can they change which record retrieval presents as current?
6. Should retrieval start as an explicit tool, automatic relevant excerpts, or both? Which real question should the first version answer well: “what did I ask?”, “why was this decided?”, or “what relates to this project?”
7. Which timestamps are available and meaningful for those questions? Should older-but-still-valid decisions outrank recent discussion, and who marks a decision as superseded?
8. Is storage representation worth deciding now, or should we first agree on a few example records, relations, and retrieval results while leaving existing session/recovery behavior intact?

No implementation choice is approved by this document. The next discussion can use concrete prompts and expected retrieval results to settle semantics before selecting storage or introducing abstractions.
