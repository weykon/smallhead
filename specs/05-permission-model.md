# 05 — Permission Model

> 状态：v0.1 — 2026-10-04
> Crate: `smallhead-permission/src/`
> 上游: `ARCHITECTURE.md` §6, §15; `AGENT_METHODOLOGY.md` §1.5

---

## 1. 概述

Permission 是 smallhead 在 "agent 自动性" 与 "用户控制" 之间的边界。设计继承自三套共识：

1. **三态 Ruleset**（opencode `evaluate.ts`）：`allow` / `ask` / `deny` —— 优于二态。
2. **last-match-wins**（opencode）：规则数组顺序敏感，后写优先。
3. **arity prefix + doom_loop**（Claude Code + opencode）：精确到子命令级别 + 连续重复检测。

smallhead 在这之上加了 **ECS-aware 权限**：`EcsRead` 默认 allow，`EcsMutate` 默认 ask（核心安全策略）—— 这是 big-head 沿用来的差异化能力。

---

## 2. 类型定义

```rust
// smallhead-permission/src/ruleset.rs

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub permission: Permission,
    pub pattern: Pattern,
    pub action: Action,
    pub reason: Option<String>,                  // 弹窗时显示
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Bash,
    Read,
    Write,
    Edit,
    Webfetch,
    EcsRead,
    EcsMutate,
    /// Skill / extension 工具
    Custom(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Pattern {
    /// 精确匹配: "Read"
    Exact(String),
    /// 前缀匹配: "Read .md"
    Prefix(String, String),
    /// Glob: "Bash git *"
    Glob(String, String),
    /// Tree-sitter 解析的 command shape: ["git", "checkout", "*"]
    CommandShape(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct EvalContext {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub cwd: PathBuf,
    pub doom_loop_history: VecDeque<(String, String)>,   // (name, args_hash)
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub action: Action,
    pub reason: String,
    pub rule_matched: Option<usize>,   // 哪个 rule 命中（调试用）
}

#[derive(Debug, Clone, Default)]
pub struct Ruleset {
    pub rules: Vec<Rule>,
}

impl Ruleset {
    /// last-match-wins 评估
    pub fn evaluate(&self, perm: &Permission, ctx: &EvalContext) -> Decision {
        for (i, rule) in self.rules.iter().enumerate().rev() {
            if rule.matches(perm, ctx) {
                return Decision {
                    action: rule.action,
                    reason: rule.reason.clone()
                        .unwrap_or_else(|| format!("rule #{} matched", i)),
                    rule_matched: Some(i),
                };
            }
        }
        Decision {
            action: Action::Ask,
            reason: "no matching rule".into(),
            rule_matched: None,
        }
    }
}

impl Rule {
    pub fn matches(&self, perm: &Permission, ctx: &EvalContext) -> bool {
        if &self.permission != perm { return false; }
        match &self.pattern {
            Pattern::Exact(s) => ctx.tool_name == *s,
            Pattern::Prefix(prefix, arg_prefix) => {
                ctx.tool_name == *prefix && extract_arg_string(&ctx.args).starts_with(arg_prefix)
            },
            Pattern::Glob(glob_pat, _) => {
                glob_match(glob_pat, &ctx.tool_name)
                    || glob_match(glob_pat, &format!("{} {}", ctx.tool_name, extract_arg_string(&ctx.args)))
            },
            Pattern::CommandShape(shape) => {
                matches_command_shape(&ctx.args, shape)
            },
        }
    }
}
```

---

## 3. Doom Loop Detector

```rust
// smallhead-permission/src/doom_loop.rs

use std::collections::VecDeque;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct DoomLoopDetector {
    pub threshold: usize,                   // 默认 3
    history: VecDeque<(String, String)>,    // (name, args_hash)
}

impl DoomLoopDetector {
    pub fn new(threshold: usize) -> Self {
        Self { threshold, history: VecDeque::new() }
    }

    pub fn record(&mut self, name: &str, args: &serde_json::Value) {
        let hash = short_hash(args);
        self.history.push_back((name.to_string(), hash));
        if self.history.len() > self.threshold * 2 {
            self.history.pop_front();
        }
    }

    pub fn would_trigger(&self, name: &str, args: &serde_json::Value) -> bool {
        let hash = short_hash(args);
        let last_n: Vec<_> = self.history.iter().rev()
            .take(self.threshold)
            .collect();
        last_n.len() == self.threshold
            && last_n.iter().all(|(n, h)| n == name && *h == hash)
    }

    pub fn triggered(&self) -> bool {
        if self.history.len() < self.threshold { return false; }
        let last = self.history.back().unwrap();
        self.history.iter().rev()
            .take(self.threshold)
            .all(|x| x == last)
    }

    pub fn threshold(&self) -> usize { self.threshold }
}

fn short_hash(args: &serde_json::Value) -> String {
    let canonical = serde_json::to_string(args).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..8])    // 64 bits 足够
}
```

---

## 4. Bash Tool 的 Tree-sitter 解析

精确识别 mutating commands（rm, mv, >, >>, dd, chmod, chown...）：

```rust
// smallhead-permission/src/bash_parser.rs

use tree_sitter::{Parser, Query};

pub fn matches_command_shape(args: &serde_json::Value, shape: &[String]) -> bool {
    let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let parsed = parse_bash(command);    // tree-sitter bash grammar

    // shape 例：["git", "checkout", "*"]
    // 匹配：
    //   git checkout main      ✓
    //   git checkout -b foo    ✓ (* 匹配 -b foo)
    //   git checkout           ✗ (arity 不够)
    //   git push origin main   ✗ (子命令不匹配)

    let mut parsed_iter = parsed.subcommands.iter();
    for pat in shape {
        match pat.as_str() {
            "*" => return true,    // wildcard 匹配剩余
            p => {
                if parsed_iter.next().map(|s| s == p) != Some(true) {
                    return false;
                }
            }
        }
    }
    true
}

pub fn is_mutating_command(command: &str) -> bool {
    let parsed = parse_bash(command);
    parsed.contains_any(&[
        "rm", "mv", "cp", "dd", "chmod", "chown", ">",
        ">>", "sed -i", "awk -i",
    ])
}

pub fn extracts_external_paths(command: &str) -> Vec<PathBuf> {
    // 提取 command 中引用的所有路径，标记是否在 cwd 外
    // 用于 safety: 默认拒绝向 cwd 外写
    todo!()
}
```

**为什么用 tree-sitter**：字符串前缀匹配 `Bash rm *` 会误判 `arm`、`format` 等子串。tree-sitter 解析出 AST 后精确识别 subcommand。

---

## 5. WebFetch 域名白名单

```rust
// smallhead-permission/src/webfetch.rs

pub struct WebFetchPolicy {
    pub allowed_domains: Option<Vec<String>>,   // None = all + ask
    pub max_response_bytes: usize,
    pub timeout: Duration,
}

impl WebFetchPolicy {
    pub fn evaluate(&self, url: &str) -> Decision {
        let domain = extract_domain(url);
        match &self.allowed_domains {
            Some(domains) if domains.iter().any(|d| glob_match(d, &domain)) => {
                Decision { action: Action::Allow, reason: "domain whitelisted".into(), rule_matched: None }
            }
            Some(_) => Decision { action: Action::Deny, reason: "domain not in whitelist".into(), rule_matched: None },
            None => Decision { action: Action::Ask, reason: "no domain whitelist configured".into(), rule_matched: None },
        }
    }
}
```

**默认行为**：`allowed_domains = None` → 所有 WebFetch 都 ask。用户配置白名单后 allow / deny。

---

## 6. ECS Mutate 默认 Ask（核心安全策略）

这是 smallhead 与其他 agent 项目的核心差异化：

```toml
# ~/.config/smallhead/smallhead.toml
[permission.rules]
# 全部 bash 默 allow，但 mutating ask
"Bash *" = "allow"
"Bash rm *" = "ask"
"Bash sudo *" = "deny"

# 文件系统
"Read *" = "allow"
"Read .env" = "deny"
"Write *" = "ask"
"Edit *" = "ask"

# Webfetch：白名单或 ask
"Webfetch github.com/*" = "allow"
"Webfetch *" = "ask"

# ECS：读 allow，写 ask（**核心安全策略**）
"EcsRead *" = "allow"
"EcsMutate *" = "ask"

# Skill tools
"Custom my-skill.*" = "allow"
```

**为什么 ECS mutate 默认 ask**：
1. ECS 状态是跨会话的——一个错误的 mutation 会污染后续所有 session。
2. 大部分 query / read 操作安全——模型需要看 facts 才能做决策。
3. mutate 操作应每次都有 traceable reason（big-head 设计哲学）。

---

## 7. Doom Loop 与 Permission 的交互

```rust
// 在 AgentLoop::run_one_turn() 中（见 specs/02-agent-loop.md §6）
// 8b. DoomLoop 检查（先于 permission）
if self.doom_loop.would_trigger(&tc.name, &tc.input) {
    self.handle_doom_loop(tc).await?;
    continue;
}

// 8c. Permission 决策
let decision = self.ruleset.evaluate(&tool_to_permission(&tc.name), &ctx);
```

**逻辑**：doom_loop 检测在 permission evaluate 之前。如果"ruleset 允许但 doom loop 触发"，强制 ask——避免 ambiguity。

---

## 8. smallhead.toml 配置示例

```toml
[permission]
mode = "default"                              # default | permissive | strict
ask_threshold = 3                             # doom_loop 阈值

[permission.rules]
# ── 全部 bash 默 allow ──────────────────────────────────────
"Bash *" = "allow"

# ── 危险命令 ask / deny ──────────────────────────────────────
"Bash rm *" = "ask"
"Bash mv *" = "ask"
"Bash chmod *" = "ask"
"Bash sudo *" = "deny"

# ── Git 子命令级精细控制（arity prefix） ──────────────────────
"Bash git status" = "allow"
"Bash git diff *" = "allow"
"Bash git log *" = "allow"
"Bash git commit *" = "ask"
"Bash git push *" = "ask"
"Bash git reset --hard *" = "deny"
"Bash git checkout *" = "ask"

# ── 文件系统 ─────────────────────────────────────────────────
"Read *" = "allow"
"Read .env" = "deny"
"Read .env.*" = "deny"
"Read **/.git/**" = "deny"
"Write *" = "ask"
"Edit *" = "ask"
"Edit src/**" = "allow"                       # 项目源代码允许

# ── Webfetch：白名单或 ask ───────────────────────────────────
"Webfetch github.com/*" = "allow"
"Webfetch api.anthropic.com/*" = "allow"
"Webfetch *" = "ask"

# ── ECS（核心安全策略） ──────────────────────────────────────
"EcsRead *" = "allow"
"EcsMutate *" = "ask"
"EcsMutate WorldOp/SpawnEntity" = "ask"
"EcsMutate WorldOp/DefineComponent" = "ask"   # schema 变更尤其敏感

# ── Skill tools（按 name 区分） ──────────────────────────────
"Custom docker.*" = "ask"
"Custom search.*" = "allow"
```

---

## 9. TypeScript 类比（opencode `evaluate.ts`）

```ts
// opencode: packages/console/core/src/permission/evaluate.ts (简化)
type Action = "allow" | "ask" | "deny"

type Rule = {
  permission: Permission
  pattern: Pattern
  action: Action
}

function evaluate(ruleset: Rule[], perm: Permission, ctx: EvalContext): Decision {
  // last-match-wins
  for (let i = ruleset.length - 1; i >= 0; i--) {
    if (matches(ruleset[i], perm, ctx)) {
      return { action: ruleset[i].action, reason: ruleset[i].reason ?? `rule #${i}` }
    }
  }
  return { action: "ask", reason: "no matching rule" }
}
```

smallhead 多了 `doom_loop` 与 `tree-sitter bash 解析` 两个增强。

---

## 10. 验收测试

### 10.1 fixtures + evaluate

```rust
#[test]
fn last_match_wins_semantics() {
    let ruleset = Ruleset {
        rules: vec![
            Rule { permission: Permission::Bash, pattern: Pattern::Glob("Bash *".into(), "*".into()), action: Action::Allow, reason: None },
            Rule { permission: Permission::Bash, pattern: Pattern::Glob("Bash rm *".into(), "*".into()), action: Action::Ask, reason: Some("rm needs confirmation".into()) },
        ],
    };
    let ctx = EvalContext {
        tool_name: "Bash".into(),
        args: json!({"command": "rm foo.txt"}),
        cwd: PathBuf::from("/tmp"),
        doom_loop_history: VecDeque::new(),
    };

    let decision = ruleset.evaluate(&Permission::Bash, &ctx);
    assert_eq!(decision.action, Action::Ask);   // 后写的 rm rule 胜出
}
```

### 10.2 Doom loop 触发

```rust
#[test]
fn doom_loop_triggers_after_3_repeats() {
    let mut detector = DoomLoopDetector::new(3);
    let args = json!({"command": "ls"});
    for _ in 0..2 {
        detector.record("Bash", &args);
        assert!(!detector.would_trigger("Bash", &args));
    }
    detector.record("Bash", &args);
    assert!(detector.would_trigger("Bash", &args));
}
```

### 10.3 Bash tree-sitter 解析

```rust
#[test]
fn tree_sitter_distinguishes_arm_from_rm() {
    let args_arm = json!({"command": "arm-linux-gnueabihf-gcc foo.c"});
    let args_rm = json!({"command": "rm -rf /tmp/foo"});
    assert!(!is_mutating_command(args_arm["command"].as_str().unwrap()));
    assert!(is_mutating_command(args_rm["command"].as_str().unwrap()));
}
```

### 10.4 Webfetch 域名白名单

```rust
#[test]
fn webfetch_denies_unknown_domain() {
    let policy = WebFetchPolicy {
        allowed_domains: Some(vec!["github.com".into()]),
        max_response_bytes: 5_000_000,
        timeout: Duration::from_secs(15),
    };
    let decision = policy.evaluate("https://evil.com/payload");
    assert_eq!(decision.action, Action::Deny);
}
```

### 10.5 ECS mutate 默认 ask

```rust
#[test]
fn ecs_mutate_defaults_to_ask() {
    let ruleset = Ruleset::default_rules();
    let ctx = EvalContext {
        tool_name: "WorldOp::InsertComponent".into(),
        args: json!({"entity": 1, "component": "Position"}),
        cwd: PathBuf::from("/"),
        doom_loop_history: VecDeque::new(),
    };
    let decision = ruleset.evaluate(&Permission::EcsMutate, &ctx);
    assert_eq!(decision.action, Action::Ask);
}
```

---

## 11. 不变量

1. **last-match-wins**：规则顺序敏感，后写优先。
2. **无匹配规则 → Ask**（默认保守）。
3. **Doom loop 在 permission evaluate 之前**——避免"rule allow 但 doom 触发 ask"的歧义。
4. **Doom loop 阈值默认 3**——用户可调。
5. **EcsMutate 默认 ask**（核心安全策略，不可改）。
6. **EcsRead 默认 allow**（模型需要看 facts 才能决策）。
7. **`Webfetch *` 默认 ask**（无白名单时保守）。
8. **Bash mutating commands 通过 tree-sitter 精确识别**——不依赖字符串前缀。
9. **`Custom("...")` 用于 skill / dynamic tools**——按 name 区分。
10. **规则 reload 无需重启**（热加载）。

---

## 12. 与 big-head 对照

| 维度 | big-head | smallhead |
|------|---------|-----------|
| 权限模型 | OS sandbox + 显式 ECS 操作 | Ruleset + Doom Loop + Tree-sitter |
| ECS mutate | 全权给 agent | **默认 ask**（差异化） |
| Doom loop | 无显式检测 | 连续 3 次相同 args 触发 ask |
| Bash 解析 | 字符串前缀 | tree-sitter AST |
| 配置文件 | `BIGHEAD_LAYA_SHADOW` env | `smallhead.toml` |

big-head 的 `BIGHEAD_LAYA_SHADOW` 是 advisory-only 限制（不阻止），smallhead 的 Ruleset 是 **enforcing**（真正拦截 / 弹窗）。

---

## 13. 引用

- 上游 spec: `specs/01-message-model.md`（`Notification` payload）
- 引用本 spec:
  - `specs/02-agent-loop.md` — `evaluate()` + doom loop 集成点
  - `specs/03-tool-model.md` — `Permission` ↔ `ToolKind` 映射
  - `specs/04-session-storage.md` — `PermissionDecision` entry 持久化
- 源码: `ARCHITECTURE.md` §6, §15
- 方法论: `AGENT_METHODOLOGY.md` §1.5

---

## 14. 变更日志

- **2026-10-04 v0.1** — 初版。三态 Ruleset + arity prefix + doom_loop + tree-sitter bash + ECS-aware 安全策略。
- **未来**: 考虑引入"权限策略学习"——基于用户历史决策自动调整 ruleset（但默认 opt-in 且可审计）。
