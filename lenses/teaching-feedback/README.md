# Teaching Feedback Lens

完整学习助手的可选内层策略，不添加模型、领域工具或固定教学流水线。它根据用户反馈指导当前回答，并依据真实学习回执保留自述、辅助作答、方法假设与工具失败的区别。数值核验仍由 agent 使用已注册工具完成，本层不提供通用数学正确性判定。

在 Smallhead 仓库启动 TUI，先加载完整学习包，再加载本层：

```text
/lens load ../agenting-agent/learning-agent/lenses/learning
/lens load lenses/teaching-feedback
/lens list
```

学习包已经加载时，第一条改成 `/lens reload learning-agent`。注册表顺序应为 `learning-agent`、`teaching-feedback`：

```text
learning-agent(teaching-feedback(ReAct))
```

也可以直接告诉 agent：“保留当前学习 Lens，再发现并加载 teaching-feedback 作为内层，继续刚才的问题。”在本仓库默认发现目录内就能找到，不需要修改本机路径配置。加载顺序决定层次；已存在的其他层不自动重排。

试问：“继续讲误差到梯度。刚才太抽象了，用同一个例子只改一个数，先给提示，别说答案。”然后给出自己的实际回答或指出仍不懂的那一步。

`prepare_model` 加入反馈策略和依据真实记录的提示；`after_tool` 观察成功记录或失败状态。它不改写真实回执，不把记录正文嵌入 system，不做关键词黑名单判断，也不自动追加 `before_finish` 自评轮。用户纠正通过原有对话/steering 通道进入上下文。未注册学习工具时本层透传；卸载后已保存的学习记录仍留在学习库。

修改后执行 `/lens reload teaching-feedback`，下一条任务使用新版。卸载用 `/lens unload teaching-feedback`。这些 TUI 命令不改变已接收任务；agent 的显式管理工具可在当前任务下一模型轮切换。

Learning 0.2.0 增加材料和课程管理，以及按问题检索教学经验。本层识别课程切换并清除旧任务的证据提示；相关方法可作为当前尝试的候选，不会因检索命中就被认定有效。长期改进保存在学习库，不需要用户反复编辑 Lens 文件。

```sh
node --test lenses/teaching-feedback/loop.test.mjs
```
