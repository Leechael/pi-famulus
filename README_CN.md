<div align="center">
  <img src="https://raw.githubusercontent.com/Leechael/pi-famulus/main/assets/logo.png" alt="pi-famulus" width="120" />
  <h1>pi-famulus</h1>
  <p><b>让主 agent 专注主线，其余的交出去。</b></p>
  <p><a href="https://github.com/Leechael/pi-famulus/blob/main/README.md">English</a> · 中文</p>
  <p>为 <a href="https://pi.dev">pi</a> 以及基于 pi 开发的 agent 提供子代理和长时间运行的 bash。任务结束时结果会唤醒 agent，对话可以一直往下走。shell 和 monitor 由同一个 Rust 守护进程启动并运行。</p>
  <p>
    <a href="https://github.com/Leechael/pi-famulus/blob/main/docs/tools.md">工具</a> ·
    <a href="https://github.com/Leechael/pi-famulus/blob/main/docs/cli.md">CLI</a> ·
    <a href="https://github.com/Leechael/pi-famulus/blob/main/docs/design.md">架构</a> ·
    <a href="https://github.com/Leechael/pi-famulus/blob/main/eval/RESULTS.md">评测结果</a>
  </p>
  <a href="https://www.npmjs.com/package/pi-famulus"><img src="https://img.shields.io/npm/v/pi-famulus?style=flat-square" alt="npm"></a>
  <a href="https://github.com/Leechael/pi-famulus/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/Leechael/pi-famulus/ci.yml?branch=main&style=flat-square&label=ci" alt="CI"></a>
  <a href="https://github.com/Leechael/pi-famulus/stargazers"><img src="https://img.shields.io/github/stars/Leechael/pi-famulus?style=flat-square" alt="Stars"></a>
  <a href="https://github.com/Leechael/pi-famulus/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square" alt="License"></a>
  <a href="https://github.com/Leechael/pi-famulus/commits"><img src="https://img.shields.io/github/commit-activity/m/Leechael/pi-famulus?style=flat-square" alt="Commits"></a>
  <a href="https://x.com/leechael"><img src="https://img.shields.io/badge/follow-leechael-red?style=flat-square&logo=x" alt="X"></a>
</div>

## Why

pi-famulus 关心的与其说是子代理，不如说是主 agent：让它的上下文保持干净，让它盯住目标，活在跑的时候你也能一直跟它对话。用主 agent 做规划，把写代码交给跑在更便宜模型上的子代理，让测试和日志在后台跑，到需要处理时再来找它。

famulus 是替学者跑腿的助手，《浮士德》里的瓦格纳就是浮士德的 famulus。pi 的核心有意把子代理、后台 bash 和输出监控留给扩展来做（[设计文档第 1 节](https://github.com/Leechael/pi-famulus/blob/main/docs/design.md#1-background-and-goals)），pi-famulus 补上了这几样。它面向在真实项目里用 pi 或基于 pi 开发的 agent、想把慢活或并行的活交出去又不想卡住对话的人，比如跑测试、构建、起开发服务器、做代码评审。

agent 需要等待时，会出三种问题：

- **前台等待会卡住对话。** 装了 pi-famulus 后，`bash` 命令运行超过 20 秒就转到后台，这一回合继续往下走。
- **后台命令结束时没人知道。** 装了 pi-famulus 后，命令一退出，结果就注入对话，模型收到后直接接着做，不用轮询。
- **子代理报告完成时，它启动的活还在跑。** 装了 pi-famulus 后，子代理的 `bash` 不能转到后台，子代理也不能再启动子代理或 monitor。所以它报告完成时，它通过 pi-famulus 启动的东西都已经停了。例外是用 `setsid` 脱离出去的命令，以及用 `&` 留在后台的进程。

整台机器只有一个守护进程（`pi-famulus`，用 Rust 编写）。它启动每一条 shell 命令和每一个 monitor，包括子代理运行的那些，并作为它们的父进程。守护进程崩溃或被杀掉时，它启动的进程组都会被停止；用 `setsid` 进入新会话的命令除外。子代理本身运行在 pi 内部。

pi-famulus 是一个 pi 扩展，pi 能加载扩展的地方它都能用：pi TUI、RPC 模式，以及用 pi SDK 开发的 agent。离开 TUI 就没有 `/tasks` 列表和 fleet 行，但结果照样会唤醒 agent。print 模式（`pi -p`）在第一轮运行结束时就退出，之后才到的结果收不到。

### 不是 pane 管理器

如果你在用 Herdr 或 tmux pane，继续用就好。pane 是用来看着 agent、必要时接手的。pi-famulus 服务的是主 agent：子代理超出预算、需要决定或结束时，它都会被唤醒，不用盯着 pane 也知道活干完了没有。卡住的子代理会自动重试，父代理收到的是最终结果。

## Quick Start

> **安装前先看**：pi-famulus 会注册这些工具：`bash`（替换内置的 bash）、`subagent`、`monitor`、`agent_message`、`task_list`、`task_output`、`task_stop`；还会注册 `/tasks`、`/famulus`、`/reply` 三个命令。在 pi 1.0.0 中，两个扩展注册同名工具时，先加载的扩展保留这个名字，另一个扩展的同名工具会被隐藏。
>
> - `pi-subagents` 也注册了 `subagent`，不要和 pi-famulus 一起装。
> - `pi-background-tasks` 也注册了 `/tasks`，pi 会把两个命令分别列成 `/tasks:1` 和 `/tasks:2`。它和 pi-famulus 的用途重叠，两个里选一个。
>
> 详见[包冲突说明](https://github.com/Leechael/pi-famulus/blob/main/docs/development.md#conflicts-with-other-packages)（英文）。

```bash
pi install npm:pi-famulus
```

需要 pi 1.0.0 或更新版本。守护进程有这些平台的原生构建：Linux x64 和 arm64、macOS x64 和 arm64（macOS 13+）、Windows x64 和 arm64。守护进程作为 npm 包的精确版本可选依赖（optional dependency）发布，所以不要关闭可选依赖。不需要 Rust 编译器，也不需要额外下载。

不用自己写工具调用。装好后，直接用自然语言跟 agent 说：

```text
跑一遍完整的测试，告诉我哪些失败了。
```

`bash` 会等 20 秒。命令还没结束的话，它就转到后台，这一回合继续。命令退出时，结果出现在对话里。

## Features

- **bash 自动转后台**：运行时间超过 `foregroundBudgetMs`（默认 20 秒）的命令会转到后台。设置 `run_in_background` 可以让命令一开始就在后台运行。单独的 `sleep` 命令会被拒绝。
- **完成即唤醒**：命令结束、monitor 产生事件、子代理返回结果时，内容会注入对话，agent 接着往下做，不用轮询。
- **子代理**：一次调用最多 10 个子代理，默认同时跑 4 个；也可以用 `chain` 按顺序执行。等待超过 45 秒时，调用先返回，任务在后台继续。每个子代理完成后各自汇报。
- **干净收尾**：子代理的 `bash` 在前台运行，直到命令退出或超时；子代理不能启动子代理或 monitor。所以子代理完成后，它通过 pi-famulus 启动的东西都不会留下。例外是用 `setsid` 脱离出去的命令，以及用 `&` 留在后台的进程。
- **出问题早知道**：按默认设置，子代理 5 分钟没有任何动静，会在同一个会话里自动恢复一次；再卡住就标记为失败。工具执行期间，以及 `contact_supervisor` 的问题等待回答期间，都不计时。超过 30 分钟的软预算（`timeout_ms`）后，子代理继续运行，父代理每 10 分钟收到一次超预算唤醒（`subagent-overrun`），可以延长时间、引导或中断它；子代理等待回答期间，这个唤醒会先压着。这些值在[配置](https://github.com/Leechael/pi-famulus/blob/main/docs/configuration.md)的 `subagent` 部分设置。
- **一个守护进程管所有 shell 和 monitor**：守护进程是你所有 pi 会话里每个 shell 和 monitor 任务的父进程。它崩溃或被 `kill -9` 杀掉时，每个任务的进程组都会被停止。用 `setsid` 进入新会话的命令不受这个约束。
- **共享并发上限**：默认情况下，整台机器上所有 pi 会话加起来最多同时运行 8 个子代理（用 `pi-famulus config set max-agents <n>` 修改），其余的排队等待。标为 `work_kind: "test-suite"` 或 `"test"` 的子代理同时最多 2 个；没有标注的子代理算作 `other`。
- **父子对话**：子代理用 `contact_supervisor` 向父代理提问并等待回答，你也可以用 `/reply` 自己回答。`agent_message` 在父代理和子代理之间传消息。
- **可查**：在终端里，`pi-famulus top` 是会话、子代理、shell 和 monitor 的实时面板。打开子代理能看它的对话，打开任务能看它的输出，也能在面板里停掉正在跑的 shell 或 monitor。`ls`、`show`、`agent`、`events` 能看到跑了什么、为什么结束、通知为什么到了或没到。
- **用真实模型测过**：工具描述按真实模型的运行结果打分。最近一轮矩阵（2026-10-08；12 种 xAI 和 OpenAI 配置，10 个场景，k=10）记录了 1198 次通过、2 次失败，另有 6 个无效或出错的 episode 不计入（[eval/RESULTS.md](https://github.com/Leechael/pi-famulus/blob/main/eval/RESULTS.md)）。这轮矩阵早于之后对 `subagent` 和唤醒提示词的一次措辞修改（[PR #62](https://github.com/Leechael/pi-famulus/pull/62)），那次修改还没跑过完整矩阵。

## Usage Guide

| 跟 agent 说 | 会发生什么 |
| :--- | :--- |
| `跑一遍完整的测试，告诉我哪些失败了。` | `bash` 在 20 秒后把任务转到后台；命令退出时结果送达 |
| `启动开发服务器，输出里出现错误就告诉我。` | `monitor` 把新输出合成有长度上限的事件；完整重复行通常在同一时间窗口内只通知一次，元数据缺失或容量淘汰时可能再次通知 |
| `用子代理并行评审这三个模块。` | `subagent` 并行执行这些任务；每个子代理完成后各自汇报 |
| `子代理能用哪些模型？` | agent 调用 `subagent({action: "models"})` 列出可选模型 |

| 在 pi TUI 里 | 作用 |
| :--- | :--- |
| `/tasks` | 实时列出 shell、monitor 和子代理；`ctrl+x` 停止选中的项 |
| `/reply <child> <text>` | 直接回答子代理的问题，不经过模型 |
| `/famulus` | 显示 `pi-famulus` 二进制路径，以及 manager 和扩展的版本 |
| fleet 行（编辑器下方的状态行） | 显示正在运行的 shell、monitor 和子代理 |

**用最强的模型做计划，用便宜的模型做实现。** 在 pi 里选好你最强的模型，然后把下面的内容存成 `~/.pi/agent/agents/worker.md`：

```markdown
---
name: worker
description: 端到端完成一个具体任务
model: haiku
---
完成交给你的任务。做出修改，并运行能证明它可用的测试。
不要超出任务范围。汇报你改了什么、怎么验证的。
```

然后说：

```text
规划把 src/api/ 从回调迁移到 async/await。计划定下来后，
让子代理一次实现一个模块，再评审它们交回来的结果。
```

会话模型负责写计划。`subagent` 默认运行 `worker`，`worker` 使用 `haiku` 匹配到的模型。

- `model` 会和 pi 允许你使用的模型 id 及名称做匹配：也就是你的 `enabledModels` 或 `--models` 范围，没有设置范围时就是所有可用模型。必须恰好匹配一个。在 agent 文件里，匹配不到或匹配到多个时，会回退到父代理的模型并给出警告。在单次 `subagent` 调用里，这两种情况都会报错，并把候选模型告诉模型。
- 可以加后缀指定思考级别，比如 `model: haiku:high`。
- `<project>/.pi/agents/` 里的 `worker.md` 会覆盖你主目录里的那份。没有任何 `worker.md` 时，子代理使用父代理的模型。
- 内置的 agent 有 `explorer`（read、grep、find、ls、bash）和 `worker`（继承你的工具）。要加自己的 agent，在同样的目录里放 markdown 文件即可。
- `subagent({action: "steer" | "resume" | "extend" | "interrupt"})` 分别用来给运行中的子代理改方向、让已完成的子代理继续、给它更多时间、停止它。

## Reliability

pi-famulus 替你启动和停止进程。这一节写明它保证什么，以及保证到哪里为止。

- 在 Unix 上，守护进程把每个 shell 和 monitor 任务作为一个进程组启动，并作为它的父进程。守护进程结束时（`shutdown`、`kill -9`、panic），每个进程组先收到 SIGTERM，2 秒后收到 SIGKILL。Windows 上的任务跑在 Job Object 里，被停止或守护进程退出时作为一个整体终止。
- 把子进程移到新会话（`setsid`）的命令会离开进程组，不受上面的约束。用 `&` 留在后台的进程会一直运行，直到它自己退出或被守护进程停止。
- 最后一个 pi 会话关闭后，运行中的任务不会保留。最后一个会话断开 5 秒后，守护进程停止剩下的任务并退出。记录、输出和 transcript 在磁盘上保留 24 小时。
- 子代理的 30 分钟回合预算用完时还在运行，不会被杀掉，除非你设置了 `hardTimeoutMs`。父代理会收到通知，再决定延长、改方向还是中断。
- 子代理 5 分钟没有动静时，会在同一个会话上恢复一次，transcript 保留。工具运行期间，或 `contact_supervisor` 的问题等待回答期间，计时暂停。如果它再次停滞，就被标记为失败。
- `monitor` 每 200 ms 把输出合成一批。同一 monitor 的完整重复行默认在 60 秒内去重，默认精确匹配，忽略时间戳需要显式开启。通知是有长度上限的预览，原始输出可通过 `task_output` 读取。去重不计为过载丢弃。通过去重的批次允许一次连发 10 个事件，之后每 2 秒一个，超出的批次会被丢弃。在一个至少有 10 批的 30 秒窗口里，如果至少一半的批次被丢弃，monitor 会停止并通知你。
- 在 Linux 和 macOS 上，守护进程每 2 秒检查一次自己的可执行文件。文件被替换后，运行中的守护进程会以同一个 pid 重新执行自己。`pi-famulus upgrade` 可以立即触发同样的升级。运行中的命令继续跑，客户端重新连接。实测中断时间为 30 到 46 ms。

## FAQ

- **pi-famulus 和 `pi-subagents`、`pi-background-tasks` 有什么区别？** 它们是替代关系，选一个用。想要现成的角色、保存的工作流、`/council`，或在独立进程里跑的后台子代理，选 [`pi-subagents`](https://github.com/nicobailon/pi-subagents)。想要带输出文件的具名后台 shell 任务、只读的委派 agent，或它的 Fusion 工作流，选 [`pi-background-tasks`](https://github.com/ismailsaleekh/pi-background-tasks)。想要一个守护进程管理你所有 pi 会话里的 shell 和 monitor 进程、子代理共享并发上限、以及一个能查历史运行的 CLI，选 pi-famulus。
- **能像 tmux pane 那样 attach 到子代理上吗？** 不能。子代理运行在 agent 的进程内部，没法 attach，也没法接管它的会话。可以通过父代理给它改方向、恢复或中断它，用 `/reply` 回答它的问题，用 `pi-famulus agent <id> -f`、`show <id>` 和 `top` 查看它做了什么。
- **能在 pi TUI 以外使用吗？** 能。pi 能加载扩展的地方它都能加载，包括 RPC 模式（`pi --mode rpc`）和用 pi SDK 开发的 agent。print 模式（`pi -p`）也能加载，但第一轮运行结束就退出，之后的结果收不到。在 SDK 开发的 agent 里，通过 `DefaultResourceLoader` 加载它（比如用 `additionalExtensionPaths`），再调用 `session.bindExtensions()`。真实模型评测就是在 RPC 模式下驱动 pi 的。
- **子代理能再启动子代理吗？** 不能，嵌套只有一层。
- **pi 退出后后台任务会怎样？** 最后一个 pi 会话关闭 5 秒后，守护进程停止仍在运行的任务并退出。之后可以用 `pi-famulus show <id>` 查看记录。
- **`/reload` 会中断正在运行的任务吗？** 不会。shell、monitor 和子代理继续运行，结果照常送达。如果这次 reload 加载的是另一个版本的 pi-famulus，旧版本的任务会先被停止。
- **怎么知道用的是哪个二进制、哪个版本？** 输入 `/famulus`，它显示 `pi-famulus` 二进制路径及其版本、正在运行的 manager 的版本和 pid，以及扩展的版本。
- **为什么 pi 的 bash 命令在长时间构建时卡住？** 装了 pi-famulus 后，`bash` 等 20 秒就不再等，把命令转到后台，这一回合继续。要改这个时限，在 `~/.pi/agent/pi-famulus/config.json` 里设置 `foregroundBudgetMs`（[配置说明](https://github.com/Leechael/pi-famulus/blob/main/docs/configuration.md)）。
- **怎么查看或停止运行中的任务？** 在 pi TUI 里输入 `/tasks`，选中一项后按 `ctrl+x`。在终端里，`pi-famulus ls` 列出正在运行的任务。
- **支持 Windows 吗？** 支持 x64 和 arm64。守护进程原地重新执行自己的升级方式只在 Linux 和 macOS 上可用。在 Windows 上，需要替换二进制并手动重启守护进程。默认情况下 `bash` 命令在 Git Bash 里运行，而 Git Bash 的运行时在多个副本同时启动时可能崩溃或卡住（[git-for-windows/git#6368](https://github.com/git-for-windows/git/issues/6368)）。pi-famulus 会错开 shell 的启动来避开这个问题。疑似启动崩溃的任务以 `shell-start-failed` 结束。任务 20 秒内没有输出、job 里除了 shell 之外没有别的进程时，会以 `shell-start-hung` 被停止，所以只跑 shell 内置命令、20 秒不出声的命令也会被停掉。两种情况下 agent 都会被告知先检查有没有副作用再重试。
- **通知没到怎么办？** 运行 `pi-famulus events --id <id>`，查看唤醒有没有发出、送达和注入。运行 `pi-famulus doctor` 检查守护进程的状态。

## Docs

以下文档目前只有英文版。

- [工具参考](https://github.com/Leechael/pi-famulus/blob/main/docs/tools.md)：参数、agent 文件、终端界面
- [配置](https://github.com/Leechael/pi-famulus/blob/main/docs/configuration.md)：`config.json`、超时、二进制查找、降级模式
- [CLI 手册](https://github.com/Leechael/pi-famulus/blob/main/docs/cli.md)：`doctor`、`ls`、`show`、`top`、`agent`、`stats`、`events`
- [架构](https://github.com/Leechael/pi-famulus/blob/main/docs/design.md)：守护进程、lifeline、唤醒、子代理运行器
- [整机并发上限](https://github.com/Leechael/pi-famulus/blob/main/docs/global-capacity.md)：`max-agents` 和按工作类型的限制
- [评测结果](https://github.com/Leechael/pi-famulus/blob/main/eval/RESULTS.md)：真实模型场景矩阵
- [开发](https://github.com/Leechael/pi-famulus/blob/main/docs/development.md)：从源码构建、测试、包冲突
- [发布](https://github.com/Leechael/pi-famulus/blob/main/docs/releasing.md)

## Background

pi-famulus 借鉴了其他 agent 的几种做法。来自 Claude Code 的有：超时后转后台而不是杀掉、完成结果注入对话、按批次输出的 `monitor`、拦截单独的 `sleep`。来自 grok-build 的有：子代理 45 秒的等待预算，以及排队等待而不是直接拒绝的准入队列。来自 lain 的是：子代理的 bash 只能在前台运行。它还从 `pi-subagents` 和 `pi-intercom` 那里学到了哪些东西不该做。调研笔记见[设计文档第 1 节](https://github.com/Leechael/pi-famulus/blob/main/docs/design.md#1-background-and-goals)。

## Support

- 如果 pi-famulus 帮到了你，可以给它一个 star，或者提 [issue](https://github.com/Leechael/pi-famulus/issues) 和 pull request。
- 报告问题时，请附上 `pi-famulus doctor` 和 `pi-famulus events --id <id>` 的输出。

## License

以 [MIT License](https://github.com/Leechael/pi-famulus/blob/main/LICENSE) 发布。
