# Shared world memory: research notes and a concrete option

Date: 2026-10-16. Discussion only; these are proposed mechanisms, not implemented or approved behavior. No runtime, agenthand database, transcript, or paper-project files were changed.

## Where the research came from

Located the agenthand group `论文研究`, session `获取` (`216d1fa9-872`), mapped to Claude transcript `e1c76f77-b267-452e-97bf-27fe0fcedd09` under `~/.claude/projects/-Users-weykon-Desktop-p-fetch-paper-agent/`. The latest research completion found was 2026-10-15, covering W42. Its reports are:

- [W42 harness and run-loop report](../../fetch-paper-agent/reports/2026-10-W42/137_W42-Supplement_Run-Loop-Strategy-Patterns_2026-10-12.md).
- [W41 memory and reliability report](../../fetch-paper-agent/reports/2026-10-W41/135_Weekly-Report_2026-10-05.md).
- Earlier [graph-memory survey notes](../../fetch-paper-agent/reports/87_Graph-based-Agent-Memory_2026-02-06.md) and [BudgetMem notes](../../fetch-paper-agent/reports/69_BudgetMem_2026-02-06.md).

The local PDFs below were read directly: first 3 pages of DAEDALUS, Persistent Memory, and Self-evolving Agent Harness; first 2 pages of CRG, BudgetMem, and Graph-based Agent Memory. This is a focused reading of their problem statements/methods and a few findings, not an independent reproduction or full-paper audit. Online versions were not refreshed. Performance numbers in the generated reports are not adopted as smallhead results.

Agenthand's session index was refreshed on October 16 while the research conversation's last completion was October 15. Index update time, source-event time, and last-use time need distinct meanings.

## Useful ideas and their limits

| Local paper / primary reference | What it actually contributes | Possible smallhead use |
| --- | --- | --- |
| [DAEDALUS, 2610.08048](https://arxiv.org/abs/2610.08048), PDF `fetch-paper-agent/content/papers/2610.08048.pdf`, pp. 1-3 | Derives heuristics from failures and accepts them after repeated subsequent successes. Reports exploration-only memories can hurt; in its small-bank setting consolidated whole-bank injection beats per-turn retrieval | Separate candidate experience from demonstrated reusable guidance. Try a small stable context plus task-directed source retrieval; do not require per-turn vector retrieval for every memory |
| [Persistent Memory, 2610.07782](https://arxiv.org/abs/2610.07782), pp. 1-3 | No detectable accuracy gain from trace recall on independent-item benchmarks; explicitly does not disprove useful persistence. Separates per-call working set from cross-query persistence | Evaluate genuinely connected work across restarts and corrections. Context compaction and long-term recall are separate mechanisms |
| [Self-evolving Agent Harness, 2610.18813](https://arxiv.org/abs/2610.18813), pp. 1-3 | Accepted revisions persist across runs. Separates interpreted state from executable changes and proposals from admission; theoretical guarantees require stated assumptions | Agent-authored summaries/relations can have revisions and admission rules without immediately introducing script/plugin self-modification |
| [CRG, 2610.07948](https://arxiv.org/abs/2610.07948), pp. 1-2 | Connects contextualized claims to trajectory evidence; reports graph construction alone does not explain the calibration gain | Link a conclusion to the exact messages/tool results that support or undermine it. A relation or model confidence is not proof |
| [BudgetMem, 2602.06025](https://arxiv.org/abs/2602.06025), pp. 1-2 | Allocates query-dependent compute to memory modules through budget tiers and a trained router | Start with inexpensive exact/metadata search, then deeper retrieval when needed. Using JEV/Laya/Qwen here is a design option, not a reproduced RL router |
| [Graph-based Agent Memory, 2602.05665](https://arxiv.org/abs/2602.05665), pp. 1-2 | Organizes extraction, storage, retrieval, and evolution, with hierarchical/temporal/relational representations | A small relation table may help explain continuation, supporting evidence, or supersession. The survey does not establish a need for Neo4j or ECS |

The latest W42 report also covers latent/text reasoning and training environments. Those are less directly applicable to an API-based agent's durable memory than revision management; they do not justify implementing model-level KV persistence in smallhead.

## Proposed meaning of one shared world

All runs consult one durable body of records. A conversation/process restart changes the currently active working context; it does not make earlier work undiscoverable. Internal conversation IDs remain provenance. Users do not need to select a session/world before asking to continue work.

Shared discoverability does not mean loading every record into every model request. Nor does an old retrieved instruction become a new task to execute. Project/topic scope can be filterable metadata and agent-maintained associations, with explicit cross-project references; scope rules remain undecided.

Three kinds of data serve different purposes:

| Data | Example | Maintenance |
| --- | --- | --- |
| Original records | User wording, assistant response, completed/failed/unknown tool outcome, versioned spec, artifact reference | Program captures stable IDs and source order. Routine summarization does not overwrite them |
| Agent-maintained views | Current work summary, a preference, a candidate lesson, continuation/supersession links | Agent proposes bounded changes with source references and expected revision. Program commits the change and its attribution |
| Search indexes | Full-text index, embeddings, optional derived relation indexes | Versioned derivatives that can be rebuilt from original records and maintained views |

A provisional storage option is a per-user SQLite catalog for records, revisions, relations, maintenance jobs, and cursors; existing content-addressed artifacts hold large payloads. Full-text search can start with SQLite FTS. LanceDB can later index embeddings without owning the only source copy. Runtime ECS, if used later, can be a loaded view rather than an additional competing authoritative store.

This is an option to test, not a database migration decision. Existing session recovery remains intact until an explicit durable-write boundary and migration have been designed. A shared artifact catalog also needs to resolve sources across current session roots and keep referenced content available.

## An internal maintenance cycle

1. Capture the original user/message/tool record through a durable write boundary, using a stable source ID to make repeated delivery idempotent. Distinguish a tool's submission from its observed outcome. Define how capture failures are surfaced before calling the write guaranteed.
2. In the same catalog transaction, enqueue maintenance for the new source range. Each job names its inputs and transformation/model version. Background work is bounded; it must not block a foreground turn on model summarization.
3. A maintenance worker proposes a summary, associations, or candidate experience. New observations need not all incur a model call; accumulate records and trigger on useful boundaries or explicit retrieval needs.
4. Apply through ordinary typed operations with source references and an expected revision. Concurrent updates retry by rereading the new revision; one worker must not silently overwrite another's edits. Advance the processed cursor only when the corresponding derived writes commit.
5. After restart, retry uncommitted maintenance idempotently. A leased job can be reclaimed after a crash. Index generation/revision identifies stale search results so they can be revalidated against current records.
6. User corrections update which derived conclusion is current and retain the original evidence/revision history. Unused entries can leave the default context without maintenance physically erasing their sources. Explicit user deletion needs its own semantics.

Automatic acceptance should depend on the kind of update: recording a quotation, proposing a summary, asserting a user preference, and claiming a proven procedure have different evidence requirements. They do not all need user approval. A heuristic gains demonstrated support from observed checks/reuse; a preference can be supported directly by explicit user wording. Numeric confidence alone must not promote either to verified truth.

One small maintenance component could run inside the application; a permanently running independent agent/service is not a prerequisite. Agents should receive useful operations such as search/read/update/link, with validated writes, rather than having to understand the database schema or issue arbitrary maintenance SQL.

## What reaches a model request

- A bounded stable context may contain well-supported current preferences and the work presently being continued.
- Task-directed retrieval returns excerpts and source IDs, project/time metadata, revision, and conflicts when relevant. The agent can read the full source.
- Cheap matching may resolve an explicit file/record reference. Semantic search and relation expansion are optional additional stages with bounded cost/hops.
- Capture the selected context versions for that request. Background maintenance may update the next request; it should not mutate an already submitted provider request.
- Log which records were offered/read and the observable task outcome. Selection frequency alone is not evidence a memory improved the result.

## Concrete scenario for discussion

Yesterday: user asks to put MCP connections in Services. Implementation evidence shows stdio is owned by McpManager, while the UI is shared with ordinary services. Sources preserve those statements and results. A derived work summary points to the implementation and records the ownership decision.

Today, after restart: user asks, "昨天 MCP 那个服务页，我们接着改。" The program retrieves the work summary and original decision, lets the agent inspect current files, and continues without asking for a session ID.

Later: user says, "只有 HTTP MCP 的本地服务进程才交给普通 ServiceManager。" The new record clarifies the existing decision. Maintenance relates it to the old record and updates the current summary; it must not infer that all stdio children now belong to ServiceManager.

A week later: a test failure shows a claimed cleanup guarantee has limits. Record the counterevidence and revise the guidance; do not keep presenting the earlier summary as unqualified success.

## Useful acceptance checks

Use a small reproducible set of linked work histories with memory-on/off controls: restarting and continuing; changing wording; correcting a preference; overlapping projects with the same term; conflicting observations; interrupted maintenance; changing model/embedding version. Check source-backed continuation accuracy, stale-claim rate, unresolved contradictions, latency, and token cost. Include relevant sources deliberately absent so the agent must admit it cannot recover them.

The central open choice is the default behavior: automatically bring a small relevant work summary into the next turn, or initially require the agent to request memory through tools. A combination is plausible, but its triggers and context budget need agreement. Source capture granularity, scope defaults, and when candidate lessons become reusable guidance also remain open.
