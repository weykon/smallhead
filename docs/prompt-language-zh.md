# 中文提示词与 Monad 桥接阅读入口

2026-10-08：将项目内置模型指令、工具及参数说明、计算工具返回的下一步指令、ECS 参数描述、示例/TUI 系统提示、Laya/Qwen 选择指令与实验案例改为中文。

保持协议标识不变：discover/dispatch、monad_id、expression、JSON Schema 的 type/required、工具名，以及 Qwen 模板标记和 yes/no 输出 token。错误日志、普通代码注释和用户自行传入的提示词不属于本次统一翻译范围。

已有 baseline JSON 保留历史原文，不能用旧测试分数代表中文提示词效果。Qwen 系统模板中的自然语言也已翻译，因此现在是中文改写模板，不是官方英文提示词原文。Laya service.py 的 defer 说明已变更，不再与 big-head 源文件逐字一致。本次未修改 big-head。

Rust 示例重新运行即可加载中文提示词；常驻 Python 服务需重启才能加载其内置文本。快速调用已有服务时，Rust 请求里的中文指令会立即生效，但服务自身的 defer 文本／Qwen 模板需要重启。

## 阅读顺序

1. `crates/smallhead-core/src/monad_bridge.rs`：spec 告诉模型如何调用；execute 的 discover/dispatch 分支实际执行动作。
2. `crates/smallhead-core/src/monad.rs`：StaticMonadRegistry 保存 `(MonadId, CatalogEntry, factory)`；search 子串过滤；instantiate 精确匹配 ID 后调用工厂。
3. `crates/smallhead-core/src/fast_route.rs`：业务模型偏好和快速选择请求。
4. `crates/smallhead-core/examples/monad_demo.rs`：第五段配置入口。

`discover` 当前遍历三个写死的业务 ID，实例化仅为读取 catalog_entry，既不调用 search，也不执行 start/step。它返回业务目录与参数 schema，帮助模型下一轮构造 dispatch。

`dispatch` 先从 JSON 提取 monad_id、objective 和 inputs；as_str/as_object 做类型转换，filter 拒绝空白文本，ok_or_else 将缺失或错误类型转成 Result 错误，? 在失败时提前返回。验证通过后 instantiate 创建子实例，再按偏好选择方法、构造 handoff、start、step 并回填结果。

StaticMonadRegistry 是“目录加工厂”的注册表，不是语义模型。search("算术") 仅对 ID、摘要、工具名、关键词作不区分大小写的子串匹配；instantiate("calculation") 要求 ID 精确匹配。自然语言选择哪个业务由上层模型处理。
