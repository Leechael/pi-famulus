# 在 Claude Code 中使用 pi agent：peer agent

**日期：** 2026-11-05
**状态：** 探索阶段。E2、T0、T1a（herdr 创建）已通过（第 3 节），T1a 发现的 `--name` 未传给 pi 已在工作区修复；其余实验未做，未进入实现。

---

## 1. 目标

让 Claude Code 会话使用 pi agent，借此调用非 Claude 模型（OpenRouter、OpenAI、Kimi、xAI 等），并沿用 famulus 已有的托管、容量和可查性。

前提：机器上必须装有 pi（≥ 1.0.0）和 pi-famulus，并开启 messaging；herdr 可选。

## 2. 核心思路

Claude Code 已经能把 pi 会话当作 peer：famulus 把 pi 会话以 `entrypoint: pi-famulus` 写入 `~/.claude/sessions`，Claude 自带的 ListAgents / SendMessage 可以直接列出并给它发消息，pi 用 SendMessage 回复后，回复会原生出现在 Claude 会话里；pi 的注册行带 `notify_idle`，Claude 可以订阅“对方空闲”。

因此派任务、steer、续聊、取结果都只是消息往来，不需要新的 MCP 工具或结果投递通道。**唯一缺的是“创建”：** 不依赖 herdr，由 daemon 托管并加入 intercom 的 pi agent。

## 3. 实验

### E2 会话定位 — 已通过（2026-11-05）

stub MCP 在交互式 CLI（v2.1.318）下，三种启动方式（直接、`sh -c exec`、两层 sh）都拿到与注册表一致的 `CLAUDE_CODE_SESSION_ID` 和 `CLAUDE_CODE_MESSAGING_SOCKET`；`CLAUDE_PID` 只在 Bash 工具的 shell 中有。Claude 嵌套运行时（会话的 Bash 里跑 `claude -p`），沿祖先链会错配到外层会话，所以若需要定位，只用环境变量。

在 peer agent 方案下，消息由 Claude 自己的 SendMessage 发出，发送方身份天然正确，创建 agent 时也只需记录创建者，此结果仅用于该记录。

### T0 Claude ↔ pi peer 往返 — 通过（2026-11-05）

方法：终端里启动 `pi --name t0-peer`（pi v1.5.0，pi-famulus 0.2.3，xai grok-4.3），启动后显示 `pi-famulus intercom active as "t0-peer"`，约 5 秒内出现在 Claude 的 ListAgents 中。Claude desktop 会话用 SendMessage 派一个只读任务（统计 `manager/src` 下 `.rs` 文件数），并设置 `notify_when_idle: true`。

结果：

- 投递：Claude 侧先收到 `[Cross-session delivery notice] … approved and released`，随后 pi 开始处理。“approved” 由谁做出尚未确认（pi 侧是否有审批步骤）。
- 回复：pi 自行调用 `SendMessage to="claude/<sid>"`，回复以 `<cross-session-message>` 出现在 Claude 会话；答案 57 经核对正确。约 10 秒，花费 $0.028。
- 回复的 `from` 是 `uds:/tmp/cc-socks/<pid>.sock`，不是 peer 名 `t0-peer`。Claude 侧要把回复对应回某个 agent，需要按 socket 的 pid 查注册表。
- 空闲通知：收到一次 `[Cross-session idle notice] "t0-peer" … is idle now`，按名字而非 socket 指明对象。它在回复之后到达，且明显晚于 pi 那一轮结束（回复到达时注册表已是 `idle`，通知仍未到）；具体延迟未测，放进 T4。

### T1a 经 herdr 创建 peer — 通过，有一处缺陷（2026-11-05）

方法：Claude 的 Bash 里执行 `pi-famulus intercom session add --name t1-peer --cwd <repo> --agent pi`（herdr 在跑，无需在 herdr pane 内执行）。herdr 按 cwd 找到已有 workspace，新开 tab 启动 pi。随后 SendMessage 派只读任务（统计 `extension/src` 下非测试 `.ts` 文件），`notify_when_idle: true`。

结果：

- 创建：命令 2.2 秒返回 `pane_id` / `session_id`（均为 herdr pane id `w1B:p2C`，不是 intercom id）。pi 约 9 秒后写入 `~/.claude/sessions`，此前 ListAgents 看不到；创建者要等注册出现才能发消息。
- **缺陷：`--name` 没传到 pi。** 名字只作为 herdr 的 tab label 和 `agent.start` 的 `name`，pi 仍按 cwd 命名为 `pi-famulus`。同 cwd 已有两个同名行，ListAgents 只能用 `pi-famulus [6a0f14]` 这种带 ref 的形式区分；Claude 无法从 `add` 的输出知道 ref，只能靠“started 29s ago”猜。
- **已修（工作区，未发布）：** `manager/src/intercom/herdr.rs` 的 `start_agent` 现在：(1) 调用方 args 里没有 `--name`/`-n` 时，在 agent argv 前插入 `--name <name>`（pi 和 Claude 都支持）；(2) 等 agent 自己的注册行，不再把 herdr 的占位行（id 即 pane id）当结果，因此输出的 `session_id` 是 `pi/<uuid>`，并新增 `name`；最多等 10 秒（低于 relay 调用的 15 秒），超时仍返回 pane id；(3) 名字以 `-` 开头时拒绝，避免被 agent 当成 flag。实测 herdr 原样传 args，不经 shell（名字 `t1 $(echo X) ;y` 原样注册）。合约测试覆盖三点，修改前均为红。未用新 daemon 实跑，因为需要重启正在服务其他会话的 daemon。
- 往返：与 T0 相同。先收到 “approved and released”，回复 `from` 仍是 `uds:/tmp/cc-socks/<pid>.sock`，答案 72 经核对正确，pi 一轮 9.4 秒，$0.029。
- 空闲通知：收到一次，按名字 `"pi-famulus"` 指明（同名时无法区分是哪一个）。粗测：pi 一轮约 23:27:55 结束，回复约 23:28:00 到达，通知 23:28:24 到达，比回复晚约 25 秒。用作“停了没回复”的兜底够用，用作主完成信号偏慢。
- 回收：`pi-famulus intercom session rm <pane_id>` 关掉 pane，`~/.claude/sessions` 行与 intercom 行随之消失。

### 待做

- **T1b 创建：** daemon 托管的 peer agent（第 4 节）。
- **T2 生命周期：** 创建者离开后的保留与 park；容量计数；多个 Claude 会话同时使用同一个 peer。
- **T3 路由：** skill 写明“需要非 Claude 模型时创建 pi agent 并发消息”，用 `claude plugin eval` 测 Claude 在 Agent 与 pi agent 之间选得对不对。
- **T4 回复可靠性：** 多次重复 T0，统计 pi 模型忘记回复的比例，以及 `notify_idle` 是否可靠兜底。

## 4. 两种 daemon 托管的 agent

| | subagent（现有） | peer agent（新增） |
|---|---|---|
| 创建者 | pi 父会话的 `subagent` 工具 | 任何会话：Claude 用 CLI 或 MCP，pi 会话也可以 |
| intercom | 不加入（`extension/src/index.ts:135` child mode 直接返回） | 加入，Claude 的 ListAgents / SendMessage 可见 |
| 沟通 | 父会话 wake、`agent_message`、`contact_supervisor` | 只走 intercom 消息；结束信号用 `notify_idle` |
| 归属与生命周期 | 父会话结束时中断，之后按保留规则 park | 归创建者；创建者离开后按保留规则 park，之后任何发消息者可把它唤醒 |
| 容量 | 计入 `max-agents`，按运行中的一轮计 | 同 |
| `ls` / `top` | `agent` | `agent`，标注 peer 与创建者 |

herdr 的 `session add --agent pi` 已是一种 peer agent，只是不由 daemon 托管。

待定：

1. **能力边界。** 倾向沿用 child mode 的限制（前台 bash，不能再起 subagent 或 monitor），只额外加入 intercom。这样“报告完成时，它启动的东西都已结束”仍然成立。
2. **完成信号。** 以 pi 主动回复为主，`notify_idle` 兜底，用于发现“停了但没回复”。
3. **subagent 与 peer 互转。** 第一期不支持。

## 5. 不再采用

- 新的 stdio / HTTP MCP 工具集与 `famulus_spawn` 结果投递：消息往来已由原生 peer 机制覆盖。
- `pi-famulus wait` 后台等待：原生投递与 `notify_idle` 已覆盖。
- 在 `agents/*.md` 里定义调用 famulus 的 Claude subagent：多一层 Claude 模型，费用与延迟翻倍。

deferred: peer agent 暴露 bash 后台与 monitor 能力 | impact: 无客户可见错误状态，peer 只能前台执行 | trigger: T1–T4 通过且用户需要 peer 自己跑长任务时
