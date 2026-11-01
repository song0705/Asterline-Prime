# Asterline 命令与键盘完整参考

本文档完整说明 Asterline 的启动参数、输入语法、斜杠命令、抽屉界面和键盘操作。
安装与产品概览请返回[中文 README](../README.md)。也可查看
[英文版](commands.md)。

## 快速上手

```text
检查仓库并解释整体架构
/mode brainstorm
怎样降低索引延迟？
/runs
/new
/resume
```

输入 `/` 可列出 Asterline 命令，输入 `@` 可列出团队成员。用 `↑`、`↓` 选择，
按 `Tab` 或 `Enter` 插入，按 `Esc` 关闭候选列表。

## 本文的参数写法

- `<参数>` 表示必填，`[参数]` 表示可选；实际输入时不要键入尖括号或方括号。
- `run-<id>` 表示界面显示的运行编号，例如 `run-2`。
- `<消息>`、`<备注>`、`<原因>`、`<标题>`、`[命令]` 会读取这一行剩余的全部
  内容，因此可以包含空格。
- 命令名和模式名使用小写。
- 省略 `[run-<id>]` 时，命令作用于当前对话中最近的一次运行。
- 必填参数缺失、参数无效或命令不存在时，不会把内容发给 Agent，而会打开
  `/help`。

## 启动 Asterline

```text
asterline [OPTIONS]
```

`--team`、`--workspace` 和 `--db` 同时支持 `--参数 值` 与 `--参数=值` 两种写法。

### `--team <PATH>`

从 `PATH` 加载 JSON 团队配置，而不是使用已保存的团队。

```bash
asterline --team .asterline/team.json
```

### `--workspace <PATH>`

设置各成员继承的工作目录。默认值是启动 Asterline 时所在的目录。

```bash
asterline --workspace /path/to/project
```

### `--db <PATH>`

设置 SQLite 数据库路径。默认是
`<workspace>/.asterline/asterline.sqlite3`。

```bash
asterline --db /path/to/asterline.sqlite3
```

### `--no-restore`

启动时不重放最近保存的对话。它不会删除历史对话，之后仍可用 `/resume` 选择恢复。

```bash
asterline --no-restore
```

### `--manual-approvals`

为 Codex 工具询问显示输入框上方的审批卡片。默认关闭：这些询问会自动通过。
`team.json` 里写 `"approvals": { "manual": true }` 效果相同。

```bash
asterline --manual-approvals
```

### `--debug`

开发模式。不再改变审批行为。

```bash
asterline --debug
```

### `--fake`

使用确定性的离线 Fake Agent，不启动真实后端 CLI。适用于开发、演示和测试。提示词里含有 `slow` 时，Fake Agent 会用约五秒钟才回复，便于
查看运行中的状态。提示词里含有单词 `tools` 时，Fake Agent 会先执行八次不同种类的工具调用（其中一次失败）再回复，便于
查看较长的工具组。对 [`/goal`](#goal)：目标里包含 `fake-goal-<N>` 时，假 Agent 会在第 N 个目标回合
宣布目标完成；包含 `fake-goal-silent` 时，每个目标回合都什么都不回复（用来看“无进展”暂停）。

```bash
asterline --fake
```

### `--banner`

进入 TUI 前打印精简启动横幅。

```bash
asterline --banner
```

### `update`

按当前正在运行的二进制所属安装方式执行显式更新：

- Windows Setup 管理的安装保留原有的校验安装器流程：检查最新稳定 Release，使用
  同一 Release 的 `SHA256SUMS` 校验 Setup，安排在 Asterline 退出后运行，然后退出。
- macOS 或 Linux 上的 Homebrew 安装会先执行 `brew update`，再执行
  `brew upgrade song0705/asterline/asterline`。Asterline 会先确认自己的可执行文件
  确实位于该 Formula 的安装前缀内。

便携归档、直接安装的 `.deb`/`.rpm`、macOS 安装包和源码构建版不会被猜测覆盖；
命令会明确提示对应的手动更新路径。

```bash
ast update
```

`ast --update` 继续作为向后兼容别名。

### `--no-auto-update`

本次启动跳过 Windows Setup 管理版本每 24 小时一次的后台更新检查。它不会永久
关闭更新，也不会阻止显式 `--update`；对于本就不会自动更新的平台和安装方式没有
效果。

```powershell
ast --no-auto-update
```

### `-h`、`--help`

打印命令行帮助并退出。它与产品内的 `/help` 不是同一个功能。

```bash
asterline --help
```

## 发送消息

### `@<member> <message>`

可选：直接向一个团队成员发送消息，不切换视图。成员名必须存在于 `/team`。
不带前缀的纯文本已经会发给你当前查看的成员（在空输入框按 `←` 切换）。

```text
@builder 实现解析器修改并运行测试
```

在 “All” 视图中，纯文本发给团队的默认成员。在协作模式中，纯文本会按该模式配置的参与者开始或继续运行。

### `@all <message>`

可选：向所有已启用成员发送同一条消息。

```text
@all 审查这个 API 方案并各自指出一个风险
```

### `@<member> /<skill> [arguments]`

通过指定成员的原生 CLI 调用已发现的 Skill。在成员前缀后输入 `/` 可打开 Skill
补全。

```text
@builder /asterline-team 检查当前运行
```

定向补全会填入该成员实际发现的原生调用语法：Codex 使用 `$skill`，Claude 插件 Skill
保留 `/插件名:skill` 命名空间。为兼容手动输入，只有精确匹配已发现
Codex Skill 的 `@<Codex 成员> /skill` 才会转换。`@member /` 会列出真正的 `/attach`，
以及仅属于该成员的已发现 Skill。`/attach` 会打开[原生会话](#接入原生会话)，在其中可以使用
后端自己的交互式斜杠命令菜单。除非精确匹配已发现的 Skill，未知定向 slash 命令不会进入
非交互 runner。Slash 控制命令必须指定一个成员；Asterline 会拒绝 `@all /…`，不会把它广播出去。

## 消息与对话命令

### 旧写法

这些写法完整输入后仍然有效，但 `/help` 和补全不再列出：

- `/ask <member|all> <message>` 和 `/all <message>` 是 `@member` 和 `@all` 的斜杠写法。
- `/agents` 打开 Agents 列表，与 `←` 打开的是同一个。
- `/focus <member>` 是 `/logs <member>` 的旧写法。

### `/new`

```text
/new
/new <prompt>
```

保存当前对话，创建一个新对话，清空当前显示的聊天记录和运行列表，为所有后端
创建新的会话 ID，并把终端模式重置为 `normal`。上一对话里配置的模式字段
（审阅人、Builder、迭代上限等本对话覆盖）会带到新对话，同一项目下不必重配。
新对话里的运行从 `run-1` 重新计数。如果仍有成员、协作运行或验证处于活动
状态，`/new` 会被拒绝；请按 `Esc` 并等待取消完成。

`/new <prompt>` 先新建对话，再把这段提示作为普通的第一条消息发给主成员。提示原样保留（中间的
空格和换行都不动），并且只在新对话建好之后才发送：如果 `/new` 被拒绝，就什么都不会发，旧对话
也不受影响。

`/clear` 不接受参数，效果与单独的 `/new` 相同，是完整的新建对话操作，而不是仅隐藏屏幕历史
（`/clear text` 会提示 `Usage: /clear`）。正常重新打开会复用当前选中的对话，不会清空它。`/new`
还会移除本对话的子 Agent（见[子 Agent](configuration.zh-CN.md#子-agent)）。旧对话会留在
Agents 列表的 **Inactive** 分组里（见 [Agents 列表](#agents-列表)）；在该列表里按 `Ctrl+N` 等同于 `/new`。

### `/resume`

```text
/resume
/resume <number>
```

`/resume` 打开 Agents 列表，并把高亮放在最新的 **Inactive** 行（已保存的对话）上。对其按 `Enter`
（或 `→`、点击）即可恢复该对话的聊天记录、团队配置、成员列表、原生后端会话 ID、活动模式以及
属于该对话的运行；恢复出来的聊天本身就是确认，不再额外提示。该对话里的子 Agent 会连同它们的会话
一起恢复。`/resume <number>` 直接恢复该编号的对话；`/resume` 后面跟其他内容会提示用法。
当成员或验证任务仍在运行时不能切换，应先使用 `Esc` 取消。

### `/name`

```text
/name <name>
```

给当前对话命名。名字会去掉首尾空白，不能为空（为空会提示 `Usage: /name <name>`）。它取代第一条消息成为
该对话的标题：在你身处其中时显示在 Agents 列表的 **Main** 行，离开后显示在它的 **Inactive** 行。名字随对话
一起保存，重启和 `/resume` 后依然保留；`/new` 开始的是没有名字的新对话。也可以在 Agents 列表里按
`Ctrl+R` 重命名（同样可以清除名字）。有名字的对话即使还没发过消息，也会列在 **Inactive** 里。

### `/goal`

```text
/goal
/goal <objective>
/goal pause
/goal resume
/goal clear
```

给主成员设一个持久目标，让它一直工作到目标完成。`/goal <objective>` 设置目标，并立即把目标提示发给主成员
（发给 `main` 成员，没有则发给第一个顶层成员，与 `/new <prompt>` 的目标一致）。聊天里会把你的目标显示为一条
普通消息。目标会去掉首尾空白，不能为空，最长 4000 个字符。目标只能在 `normal` 模式下使用；在 `review`、
`plan`、`brainstorm`、`team` 模式下命令会被拒绝并给出提示，切换到这些模式也会暂停正在运行的目标。

当主成员的回合结束（包括属于它的子 Agent 回传结果）而目标仍是进行中时，Asterline 会把一条“继续”提示作为它的
下一个回合发过去，循环往复。这条提示不会显示成消息；主 Agent 的窗口和“全部”视图里会出现一行暗色标记，例如
`↻ goal · turn 4`。目标进行中你发的消息会照常送达，该回合结束后循环继续。目标**没有回合数、token 或时间上限**，
只会在以下情况停止：

- 主成员在回复里单独写一行 `@@goal_complete {"summary":"一句话"}`（summary 可省略）。只有该目标的成员、且目标
  处于进行中时才有效。这一行不会显示在文本里，Asterline 会提示 `Goal complete: <summary>`；
- 你执行 `/goal pause`（进行中的回合先结束）或 `/goal clear`；
- 你用 `Esc` 打断主成员，或它的回合失败：目标会暂停并给出原因（`Paused by interrupt` 或错误文本）；
- 无进展保护触发：连续三个继续回合既没有回复文本也没有工具调用，目标会以
  `Paused: 3 turns in a row made no progress` 暂停。只要某个回合有产出，计数就清零。

单独的 `/goal` 显示目标、状态、已用回合数和存续时间。`/goal resume` 让已暂停的目标立即继续（若主成员正忙则在当前
回合之后）。在已有目标的对话里再执行 `/goal <objective>` 会替换目标内容，保留回合数和存续时间，并发送“目标已被
编辑”的提示；主成员正忙时这条提示成为它的下一个回合。已完成的目标会被新目标取代。

目标处于进行中或已暂停时，主成员窗口和“全部”视图（子 Agent 窗口不显示）的输入框上方有一行暗色状态：
`◎ Goal · <objective> · turn 4 · 12m`，或 `◎ Goal paused · <reason> (/goal resume)`。

目标属于对话：随对话一起保存，不会带入 `/new`，对话被删除时一并删除。关闭 Asterline 时（或离开该对话时）仍在进行
的目标，在重启或 `/resume` 后会以**暂停**状态回来（`Paused: restored — /goal resume to continue`）；目标绝不会
自己开始运行。

### `/retry`

```text
/retry
```

把最近一条用户请求按照当前活动模式重新发送。它不会恢复被阻塞的运行，也不会
处理暂停的审批路由；这两种情况分别使用 `/continue` 和 `/approve`。如果当前
对话没有历史用户请求，则不会发送任何内容。

### `/attach`

```text
/attach <member>
@member /attach
```

暂时挂起 Asterline，打开该成员真正的原生交互式 CLI；存在会话时会恢复该后端会话。
使用该原生 CLI 支持的退出方式（通常是它自己的 `/exit`）后，Asterline 会自动恢复。
新的 Claude 接入会由 Asterline 通过 `claude --session-id` 指定 UUID，因此返回后会自动导入
记录并绑定到该成员。Codex 只导入能安全识别的已绑定会话；Claude fork 只有在既有记录能
唯一证明谱系时才会导入。存在歧义的原生会话绝不靠猜测导入。Grok 和 Agy 可以恢复会话，
但目前尚不会导入接入期间的消息。

Claude 自带的交互式 resume picker 会有意隐藏由 `claude -p` 或 Agent SDK 创建的
session。要接入这类 Asterline 会话，打开 `/team`，选中成员的 **session id** 字段并按
`Enter`，选择会话后按 `s` 保存，再执行 `/attach <member>`。Asterline 会自行发现
transcript，并直接用 UUID 恢复。

### `/import`

```text
/import <session_id>
/import <member> <session_id>
@member /import <session_id>
```

从 Claude Code、Codex 或 Grok 的原生会话记录（JSONL）中导入历史消息到 Asterline 聊天流中，并将该会话绑定到对应成员。

### `/export`

```text
/export
/export claude
```

将 Asterline 当前会话记录导出为 Claude Code 原生 JSONL 格式（存储于 `~/.claude/projects/.../<session_id>.jsonl`），以便在项目目录下直接通过 `claude --resume`（`claude -r`）查看与继续对话。

### `/exit`

```text
/exit
```

立即退出 Asterline。正常退出路径会取消正在执行的后端工作并恢复终端。它仅是
Asterline 的命令；接入原生后端 CLI 时，那个 CLI 自己的 `/exit` 会返回 Asterline。

### `/approve`

```text
/approve
```

批准当前选中的待处理请求（未切换时是最早一条）。日常用法是输入框上方的卡片：
`y` 或 Enter 同意，`n` 拒绝。没有待审批项时会给出提示。

### `/reject`

```text
/reject
```

拒绝最早的一条待处理 Asterline 审批请求。没有待审批项时会给出提示。

## 团队与诊断命令

### `/team`

```text
/team
```

打开实时 Team 编辑器。打开时会刷新系统中可用的 Codex、Claude、Grok、Agy
可执行文件。模型目录会在 `ast` 启动时异步加载一次，因此编辑器不会卡住且会显示
实际检测到的模型。在该成员的 `model` 字段按 `t` 可随时重新拉取；同一后端和
工作目录的成员会共享结果。缺失的 CLI 仍会显示以便诊断，但不能被选中。

可编辑成员列表、名称、后端、模型、effort、原生 session ID 和审批行为。模型字段显示
成员实际使用的模型：固定的模型和 effort，否则是该 CLI 自己配置里选择的，再否则是模型目录的
默认值。修改先保存在草稿中，按 `s` 才会应用并保存。完整按键见
[Team 编辑器](#team-编辑器)。

### `/model`

```text
/model [text]
```

在输入框上方打开内联模型选择器，用来修改你当前正在查看的 Agent 的模型（在 “All” 视图中
作用于默认目标的 Agent），与编程 CLI 自带的 `/model` 修改当前会话类似。聊天内容仍显示在
选择器上方。命令后面的文字会预填到搜索框。

选择器按 Codex、Claude、Grok、Agy 分组列出每个已安装 CLI 的模型，每组一行灰色标题
（`codex`、`claude`、`grok`、`agy`）。标题不可选，`↑` / `↓` 会跳过它们。`current` 紧跟在 effort 档位名之后（间隔两列；没有档位的模型则在名称列之后两列），
只在目标当前模型那一行显示。当前模型就是目标固定的 `model`；没有固定模型的 Agent，则是该 CLI 自己
配置里选择的模型，再否则是模型目录的默认模型。目录仍在加载或查询失败的后端在标题下只显示一行
灰色状态，没有可选项。搜索框是单独一行 `Search: …`。搜索无结果时显示
`No matching models`，此时按 Enter 不会有任何动作。

Claude Code 没有列出模型的命令，因此 Asterline 始终提供它的别名（`opus`、`sonnet`、
`haiku`、`fable`）以及 `modelSettings` 里出现的每个完整 id；Claude 设置里非空的
`availableModels` 会把列表限制为这些模型（外加当前选中的模型）。AGY 把每个推理档位列成独立
模型（`…-low`、`…-medium`、`…-high`），选择器会把这些同族模型合并成一行，用 `←`/`→`
选择具体变体。

提供多个 effort 档位的模型每个档位一个方块，填充到当前档位，两侧是 `←` `→` 箭头（仅高亮
行）和档位名。每个模型的起始档位依次为：目标自身的 effort（仅对其当前模型）、该 CLI 自己
配置里设置的 effort（Codex 的 `model_reasoning_effort`，Claude 的 `effortLevel` /
`modelSettings`）、模型默认档位、`medium`、最低档。档位少于两个的模型什么都不显示，
`←`/`→` 对它无效。列表滚动时会出现 `(3/12)` 位置行（只统计模型，不含标题）。

| 按键                | 功能                                  |
| ------------------- | ------------------------------------- |
| 输入 / `Backspace`  | 搜索后端、模型 ID 和模型名            |
| `↑` / `↓`           | 选择模型（鼠标滚轮同样有效）          |
| `←` / `→`           | 切换高亮模型的 effort 档位            |
| `Enter`             | 应用到目标 Agent 并关闭               |
| `Esc`               | 不做修改直接关闭                      |

应用时总会把所选模型 id 固定到该 Agent 的 `model`。只有在该行用 `←`/`→` 改过 effort 才会
保存 `effort`；Agent 的当前模型沿用它原有的 effort，其他模型则把 effort 交给 CLI 决定。
AGY 的合并行里由变体 id 携带档位，因此不会写入 `effort`。换成另一个 CLI 的模型时，还会把该
Agent 切换到那个 CLI 的默认权限预设，并启动全新的原生 session。修改会保存到
`.asterline/team.json`，聊天里会出现一行 `Model: …` 记录。Agent 正在工作时会拒绝并给出提示，
先按 `Esc` 停止即可。

**切换时如何延续对话。** 同一个 CLI 内换模型会沿用原生 session，上下文原样保留，
不会额外添加任何内容。换到另一个 CLI 时，新 CLI 的原生 session 没有看过之前的对话，
Asterline 就把保存下来的聊天记录当作依据：下一条发给该 Agent 的消息前面会附上一份简短
回顾，包含这个 Agent 自己对话里的用户消息、回复和转发消息，工具调用和文件改动只保留一行
摘要，不含工具输出。回顾只发给模型，不会显示在聊天里，也不会作为聊天消息保存。回顾最长
约 40000 个字符，放不下时保留最新的部分；这种情况下切换时那行 `Model: …` 会加上
` · earlier messages not carried over`。每个 Agent 的每个原生 session 都记着自己看到
聊天的哪个位置（与 session id 一起保存，重启和 `/resume` 后仍在，`/new` 会清除）。切回
一个仍记得的 CLI 时，会恢复它原来的 session，只补上它错过的那一段。

### Agents 列表（`←`）

在空输入框按 `←` 打开 Agents 列表。它会占满整个屏幕（不显示聊天记录、输入框和聊天页脚），直到
你选中一个 Agent 或关闭它；`/agents` 打开的是同一个列表。主聊天窗口一次只显示一个成员的对话：
该成员的回复、思考、工具调用和 diff，你发给它的提示，以及它发出或收到的转发。你发给其他成员的
提示、其他成员之间的转发，以及只与另一个成员有关的通知（如 `Model: …` 切换记录、被拒绝的创建
子 Agent 请求）都不会出现在这个窗口里，`/find` 也不会匹配它们。通用通知（模式切换、`/new`、
团队更新）在所有视图里都会显示。选择 **All messages** 则按时间顺序显示所有成员的活动。

屏幕从上到下依次是：

- 一行暗色的计数，如 `Agents · 2 running, 1 idle, 3 inactive`，统计顶层 Agent 和已保存的对话。
- 搜索框（`> Search agents`）。输入、`Backspace` 和粘贴都作用于搜索，而不是聊天输入框。它匹配
  名字（主成员那一行匹配它的标题和 `main`）、后端和模型（先子串，再按顺序的字母，所以 `rvw` 能找到 `Reviewer`；每个词都要匹配）。
  输入搜索后列表变成按相关度排序的平铺列表，不再分组；没有匹配时显示
  `No agents match your search.`。
- 粗体表头（`Agent  Model  Activity`），然后第一行是 **All messages**，再是 **Running (n)**、
  **Idle (n)** 和 **Inactive (n)** 三个分组。顶层 Agent 自己或它的任一子 Agent 处于工作、排队、等待或待审批时归入
  *Running*，否则归入 *Idle*。子 Agent 留在父 Agent 下面，在名字列内缩进四列，其余列保持对齐。*Inactive* 列出已保存的对话
  （不是在线成员），按最近更新从新到旧排列，没有已保存对话时不显示。这些行有暗色 `•`，标题是该对话
  的第一条消息（折成一行），再是主成员的后端和模型，以及距该对话上次更新的时间（`45s`、`12m`、`3h`、
  `2d`）。如果给对话命过名（`/name`，或下面的 `Ctrl+R`），标题就是这个名字而不是第一条消息。它们没有预览，搜索也会匹配它们的标题和模型。对其按 `Enter` 或 `→` 会恢复该对话并关闭列表；
  `Ctrl+N` 新建对话。每次打开列表时都会重新加载已保存的对话。
- 每行有图标（Agent 活跃时是脉动的 `◇◈◆◈`，等待你审批时是黄色的 `!`，否则是 `•`，失败为错误色）、名字、后端以及它实际使用的模型和 effort，还有
  活动。主成员那一行用聊天内容而不是名字作标题：你给对话起的名字（`/name`），否则是发给它的第一条提示（连续空白折叠成单个空格），
  还没有提示时用工作区文件夹名。子 Agent 和在 `/team` 中添加的成员保持配置的名字。名字列最宽
  28 列，超出部分以 `…` 结尾，所以很长的首条提示不会挤压其他列。活动：活跃时是 `working 12s`（或 `queued`、`waiting`、`needs approval`），失败后是 `failed`，
  否则是距上次回复的时间（本次会话内，如 `3m`；尚未回复时留空）。
- 选中行是整行宽度的高亮底色。只有选中行会在下方显示该 Agent 最近一条消息的预览，暗色折行，
  最多三行，被截断时以 `…` 结尾。列表比屏幕高时，选中项保持在中间附近，被裁掉的上方或下方用
  暗色 `...` 行标示。
- 最后一行是按键提示。

选中某个子 Agent 即可看到它的完整对话并直接继续聊；你和它的对话不会转给它的父 Agent。列表打开时
会选中你当前查看的 Agent。

- `↑` / `↓` 移动选择；`Home` / `End` 跳到第一行或最后一行；`PageUp` / `PageDown` 翻页；鼠标滚轮
  每次移动一行。
- `Ctrl+N` 新建对话（等同于 `/new`）并关闭列表。
- `Ctrl+R`（搜索为空，选中 **Inactive** 行或 **Main** 行时）重命名该对话。搜索行变成
  `> Rename: <名字>`，预填当前名字；`Enter` 保存（名字为空则清除名字），`Esc` 取消。期间提示行显示
  `Enter save   Esc cancel`。
- `Ctrl+X`（搜索为空，仅限 **Inactive** 行）删除已保存的对话及其全部消息、运行和审批。第一次按下时提示行变为
  `Press Ctrl+X again to delete`；在同一行再按一次 `Ctrl+X` 才会删除。按其他任何键或移动选择都会取消。当前
  对话和在线 Agent 不能删除。提示行只在适用的行上列出 `Ctrl+R rename` 和 `Ctrl+X delete`。
- `Enter` 或 `→` 把主聊天切换到选中项并关闭列表；用鼠标点击某一行效果相同。
- `Esc` 先清空已输入的搜索，搜索为空时才关闭列表。`←` 仅在搜索为空时关闭列表。

启动时默认显示第一个成员（或团队的默认目标）。纯文本会发给你当前查看的 Agent。
在 “All” 视图中，纯文本发给团队的默认成员。

### `/logs`

```text
/logs [member]
```

打开持久化运行日志，其中包括后端 stderr、适配器警告和运行时警告。按时间从旧到新排列（与聊天记录一致），打开时停在最新一条，只有 warn / error 带级别标签。指定成员时只显示该成员的
stdout、stderr、工具活动和适配器警告。

```text
/logs builder
```

### `/diff`

```text
/diff
```

打开实时工作树视图，显示 `git diff` 以及未跟踪文件信息。它不会暂存、还原或
修改任何文件。工作树干净或不是 git 仓库时，聊天里只出现一行提示，不会打开空面板。

### `/find`

```text
/find [text]
```

在当前聊天记录中执行不区分大小写的搜索。底栏显示当前匹配和总数，例如
`find: "timeout" (2/5)`。输入框为空且没有抽屉打开时，按 `n` 跳到下一项，
按 `p` 跳到上一项。不带文本的 `/find` 或 `Esc` 会清除搜索。匹配项位于已折叠的工具组（见[工具调用](#工具调用)）内时，该组会自动展开。

```text
/find timeout
```

### `/help`

```text
/help
```

打开命令面板（标题带 Asterline 版本号），按 Common（常用）、Approvals & sessions（审批与会话）、Runs（运行）三组列出值得了解的命令。输入 `/` 时的建议列表顺序相同，已输入的内容匹配最好的排在前面。不存在的斜杠命令，以及缺少必填参数或参数无效的命令，也会打开
这个面板。

## 协作模式

### `/mode`

```text
/mode
/mode <normal|review|plan|brainstorm|team>
```

无参数的 `/mode` 在输入框上方打开面板（与 `/model` 选择器类似），高度随内容而定。
第一层列出每种模式及其当前绑定摘要，打开时落在本对话当前模式上。Enter
是进入高亮模式字段层的唯一按键；Esc 或 q 关闭且不应用未保存的改动。

每个字段显示当前生效值以及来源：`default` / `team.json` / `this chat`
（本对话）。改动先作为 pending：`s` 选择当前模式并应用到本对话，`w` 把当前
模式被覆盖的字段写入 `team.json` 作为这支队伍的默认，`r` 清除该字段的本对话
覆盖并回落。不按 `s` / `w` 直接关闭会丢弃 pending。

`/mode <name>` 仍是键盘快路径：立刻切模式、不打开面板，聊天里出现一行 `Mode: …` 说明新模式；若当前配置无法运行该模式（只剩显式绑定不可行的情况，例如把 reviewer 绑成 builder），会直接说明原因。非 `normal` 模式下，成员栏会显示 ` review mode ` 标签。选择
`review`、`plan`、`brainstorm` 或 `team` 后，下一条直接输入任务即可，**不要**加
`@member` 前缀；该消息会按所选模式交给配置的参与成员。`@member <消息>` 有意
始终是一对一指令，会绕开当前协作 Run。`/new` 把新对话重置为 `normal`，但保留
上一对话的模式字段设置；`/resume` 恢复所选历史对话的模式和覆盖。

#### `/mode normal`

使用普通的直接消息派发。纯文本发给你当前查看的 Agent；`@member` 和 `@all` 仍可发给其他对象。

#### `/mode review`

启动 builder/reviewer 循环。Builder 执行工作，Reviewer 输出结构化
`@@review` 结论；未批准时继续修改，直到批准或用尽 `max_iterations`。用尽后
运行会进入 blocked。Reviewer 被要求亲自检查 working tree，而不是只信
Builder 的报告。可选的 `reviewer_hint` 会追加到这条提示里。
Reviewer 回复中没有结构化结论时会被提醒
一次，之后整段回复按 `request_changes` 处理（会有 Notice 说明）并消耗一次
迭代。没有其他顶层成员可担任 Reviewer 时，运行开始前会在 `main` 名下开一个使用 `main` 模型的
`reviewer` [子 Agent](configuration.zh-CN.md#子-agent)，下次复用。

```text
/mode review
重构解析器，但不要改变公开行为
```

#### `/mode plan`

启动由 Leader 驱动的规划运行。Leader 创建 checklist（步骤不需要
owner）后，最终 checklist 会派给 Builder；Builder 默认就是 Leader 自己，除非 `modes.plan.builder` 指定了别人。Reviewer 是可选的：若配置，它只审计划本身——
完整性、顺序、风险、验收标准与可测性——从不审代码或 diff；要求修改时回到 Leader 修订
checklist。`modes.plan.auto_execute` 默认 `true`；设为 `false` 时，最终计划会停在显式
`/approve`，确认后才派给 Builder。Builder 完成后再按配置验证。

```text
/mode plan
迁移缓存格式并验证向后兼容
```

#### `/mode brainstorm`

启动结构化、多参与者的头脑风暴。一次完整运行先进行不带评判的 `seed`、`build`、
`stretch` 三类生成波次，再收集私密排序选票、计算排名并综合入选想法，同时保留
异议和证据。想法卡片与选票遵循内置 Asterline brainstorm Skill，便于稳定提取。只有 `main` 时，会开
`ideator-1`、`ideator-2`……子 Agent（用 `main` 的模型），人数由 `modes.brainstorm.participant_count`
决定，默认 3。

```text
/mode brainstorm
怎样让没有节点文本的图检索更稳健？
```

#### `/mode team`

启动 Coordinator 驱动的团队运行。Coordinator 创建并负责清单、向其他成员派发
任务并整合结果。修复循环持续到配置的迭代上限。Team 默认只能使用当前队员；
若允许 Coordinator 加人，在 Mode 面板打开 `allow_add_members`，新队员会立刻
加入，不再经过 `/approve`。

```text
/mode team
实现功能、完成审查、更新文档并运行测试
```

模式角色、参与者和迭代上限定义在 `team.json` 中。Reviewer 使用类似
下面的一行式结论通信：

```text
@@review {"verdict":"approve","summary":"LGTM"}
```

## 运行命令

运行只属于当前对话。`/new` 的新对话没有运行记录，下一轮从 `run-1` 开始；
`/resume` 只恢复所选对话的运行。

### `/runs`

```text
/runs
```

打开 Runs 抽屉，查看运行 ID、模式、阶段、状态、贡献、验证结果、清单、时间线和
建议的下一步操作。按键见 [Runs 抽屉](#runs-抽屉)。还没有任何运行时只显示一行提示，不打开抽屉。

### `/continue`

```text
/continue [run-<id>] [note]
```

恢复 blocked 或 failed 的模式/团队运行，并可向 Coordinator 或模式引擎附带
备注。省略运行 ID 时选择本对话最近的运行。活动中的运行不能再次 continue；
没有持久化模式状态的旧版运行也无法重建。

恢复会重入运行被阻塞时所处的阶段：review 运行重新派发 Builder（若审阅未出
结论则派发 Reviewer）；plan 运行回到 Leader 或重新派发未完成的已分配步骤；
brainstorm 运行重跑当前生成波次（或投票/综合阶段）；处于验证中的运行会重新
执行验证。运行记录中的所有成员必须仍在 roster 中。

```text
/continue run-4 使用刚安装的依赖重试
/continue 改用更简单的备用方案
```

### `/note`

```text
/note [run-<id>] <text>
```

向运行时间线添加检查点，但不会唤醒 Agent 或触发派发。

```text
/note run-4 已与 reviewer 确认 API 契约
```

### `/block`

```text
/block [run-<id>] <reason>
```

把所选运行标记为 blocked 并记录原因。正在验证的运行必须先按 `Esc`，才能手动
标记 blocked。

```text
/block run-4 等待 schema 决策
```

### `/step`

管理最近一次运行或明确指定运行的清单。步骤编号从 `1` 开始。

#### 添加步骤

```text
/step add [run-<id>] [@owner] <title>
```

添加清单项，并可立即指定负责人。

```text
/step add run-4 @builder 实现迁移
```

#### 修改步骤状态

```text
/step todo [run-<id>] <n> [note]
/step doing [run-<id>] <n> [note]
/step done [run-<id>] <n> [note]
/step block [run-<id>] <n> [note]
```

分别把步骤设为待办、进行中、完成或阻塞。`/step blocked` 是 `/step block`
的别名。

```text
/step doing run-4 2 API 批准后已开始
/step done run-4 2 测试通过
```

#### 重命名步骤

```text
/step rename [run-<id>] <n> <title>
```

替换步骤标题。`/step edit` 是别名。

#### 删除步骤

```text
/step remove [run-<id>] <n>
```

删除清单项。`/step delete` 和 `/step drop` 是别名。

#### 分配或清除负责人

```text
/step assign [run-<id>] <n> <member>
/step unassign [run-<id>] <n>
```

设置或清除负责人。成员名可以带或不带 `@`。`/step owner` 是 `assign` 的
别名；`/step clear-owner` 和 `/step clear_owner` 是 `unassign` 的别名。

## 全局键盘操作

成员还在工作时，`Enter` 会把下一条消息排进队列，而不是立刻再开一轮。
`Esc` 打断当前成员，然后把队列里的消息发出去。如果这条队列消息还没开始、
你想改，用 `Shift+←` 拉回输入框。当前对话可以一直往上滑到第一条消息。

| 按键                           | 功能                                                        |
| ------------------------------ | ----------------------------------------------------------- |
| `Enter`                        | 发送，或接受当前选项                                        |
| `Shift+Enter`                  | 插入换行                                                    |
| `Alt+Enter`                    | 终端无法区分 Shift+Enter 时的换行备用键                     |
| `↑` / `↓`                      | 在输入框、历史或当前选择列表中移动                          |
| `Tab`                          | 接受补全                                                    |
| `Ctrl+R`                       | 反向搜索提示历史（在 Agents 列表里：重命名对话）            |
| `Ctrl+X`（Agents 列表）        | 连按两次删除选中的已保存对话                                |
| `n` / `p`                      | 输入框为空时跳到下一/上一条 `/find` 结果                    |
| `PageUp` / `PageDown`          | 按页滚动聊天或当前抽屉                                      |
| `←`（输入框为空时）            | 打开 Agents 列表（成员、子 Agent、已保存对话）              |
| `Shift+←`                      | 把尚未发给成员的最后一条排队消息拉回输入框修改              |
| 鼠标拖动                       | 选择并复制聊天、输入框、状态栏或抽屉文字                    |
| 点击工具组标题行               | 折叠或展开该工具组（只影响被点的那一组）                    |
| 鼠标滚轮                       | 滚动聊天或当前抽屉                                          |
| `Esc`                          | 关闭浮层、清除搜索，或打断当前成员并把下一条排队消息发出    |
| `Ctrl+T`                       | 展开或折叠思考过程                                          |
| `Ctrl+G`                       | 展开或折叠文件改动                                          |
| `Ctrl+O`                       | 显示完整工具输出并展开所有工具组（开关）                    |
| `Ctrl+L`                       | 打开日志                                                    |
| `Ctrl+P`                       | 打开命令面板                                                |
| `Ctrl+N` / `Ctrl+B`            | 聚焦下一/上一个成员                                         |
| `Ctrl+A` / `Ctrl+E`            | 移到行首/行尾                                               |
| `Ctrl+U` / `Cmd+Backspace`     | 清空输入框当前行（其它行保留）                              |
| `Ctrl+W`                       | 删除前一个词                                                |
| `Ctrl+C`                       | 复制聊天/输入框选区；否则保持原有的中断行为                 |
| `Ctrl+V` / `Cmd+V`             | 把剪贴板图片附到下一条消息（不是 Ctrl+C）                   |

截图或复制的图片写在进程临时目录
`std::env::temp_dir()/asterline-pasted/<pid>/`：Windows 是 `%TEMP%`，macOS 是
`$TMPDIR`，Linux 是 `/tmp`。不依赖系统自动回收。输入框 Backspace / 清空会立刻
删未发送的拷贝；退出时清掉本进程目录；下次启动会清掉已经退出的进程留下的目录。
再按各后端原生格式发出：Codex `localImage`、Grok ACP image 块、Claude/Agy
可读的附件路径。每条消息最多四张。粘贴 PNG/JPEG/GIF/WebP 文件路径会拷进临时
目录，而不是插入路径文本。

提示历史的行为与 Shell 类似：浏览旧记录时，`↑`、`↓` 会保留当前草稿。在
`Ctrl+R` 搜索中，继续输入可缩小范围，再按 `Ctrl+R` 找更早的匹配，按 `Enter`
接受，按 `Esc` 取消。

### 工具调用

同一个 Agent 连续的工具调用会合成一组，标题是 `⚒ tools`，每次调用一行。调用不超过 3 次的组始终完整列出；更长的组默认折叠成一行：

```text
  ▸ ⚒ tools · 12 calls · 3 read, 5 edit, 4 shell · 2 failed
```

这一行按种类统计调用次数（按首次出现的顺序最多列四种，之后用 `…`）；有调用失败时再显示失败数（用错误色）。组里最新一次调用仍在运行时，这一行下面会保留那一行，便于看到进度。

点击标题行展开该组（`▾ ⚒ tools`），再点一次折叠。点击只切换被点中的那一组；在该行上拖动仍然是选择文字。`Ctrl+O` 打开时会显示每次调用的完整输出，并展开所有折叠的组；关闭后各组回到自己的折叠或展开状态。`/new` 与 `/resume` 后所有组重新折叠。`⇄ agents` 组不会折叠。

## Team 编辑器

Team 编辑器包含“选择成员”和“选择字段”两层。

| 按键      | 选择成员                         | 选择字段                         |
| --------- | -------------------------------- | -------------------------------- |
| `↑` / `↓` | 选择成员                         | 选择字段或模型                   |
| `←` / `→` | —                                | 在模型列表中选择 effort          |
| `Enter`   | 打开成员字段                     | 编辑或打开 Agent/模型/会话列表   |
| `Esc`     | 关闭 Team                        | 返回成员选择                     |
| `a` / `d` | 添加/删除成员                    | —                                |
| `t`       | —                                | 重试失败的模型目录               |
| `y`       | 复制 session ID                  | 复制字段值                       |
| `s`       | 应用并保存                       | 应用并保存                       |
| `e`       | —                                | 手动输入模型或 session ID        |

当前层级可用的按键只在编辑器最后一行显示一次。文本字段会打开专用输入框；`Enter` 提交，`Esc` 取消。模型列表用 `↑`、`↓`
选择模型，用 `←`、`→` 选择该模型的 effort，按 `Enter` 同时应用。发现到模型时，
会直接选中 CLI 报告的实际默认模型；只有空目录才显示通用的 `default` 项。
只用 `↑`、`↓` 浏览不会改动已有的 effort 覆盖值；若新模型没有公布该覆盖值，
按 `Enter` 会保留当前设置，必须用 `←`、`→` 明确选择它公布的值，绝不会猜测替换。

在 `session id` 字段按 `Enter` 会打开 Asterline 原生会话表。它读取本地 Codex、
Claude 或 Grok 历史，显示标题、项目、更新时间和原生 ID，并只保留属于该成员
有效工作目录的会话。输入可筛选，用 `↑`、`↓` 或 `PageUp`、`PageDown` 移动，
按 `Enter` 把 ID 放入草稿，再按 `s` 保存。按 `e` 可手工输入。Agy 暂时必须
手工输入，因为 Asterline 尚无经过验证的本地 Agy 历史格式。

## Runs 抽屉

| 按键                  | 功能                                                        |
| --------------------- | ----------------------------------------------------------- |
| `←` / `→`             | 选择更早/更新的运行                                         |
| `↑` / `↓`             | 选择清单步骤；没有步骤时选择运行                            |
| `x`                   | 切换精简/详细视图                                           |
| `Enter`               | 把所选步骤状态或运行下一步填入输入框                        |
| `Tab`                 | 把发给所选步骤负责人的可编辑派发填入输入框                  |
| `PageUp` / `PageDown` | 滚动详情                                                    |
| `Esc`                 | 关闭抽屉                                                    |

`Enter` 和 `Tab` 只会把文字放入输入框，不会立即执行。切换运行会清除当前步骤
选择。要取消活动工作，先按 `Esc` 关闭抽屉，再按一次 `Esc`。

## 其他抽屉

模式面板位于输入框上方，Agents 列表则占满整个屏幕；其余抽屉（`/help`、`/team`、`/logs`、`/runs`、
`/diff`）是宽度占满、高度恰好容纳内容的浮层，边框内留一列空白。所有抽屉均可用 `PageUp`、`PageDown` 或鼠标滚轮滚动，用 `Esc` 关闭。聊天、
状态栏和抽屉中的文字均可用鼠标拖动选择，再使用终端常规复制快捷键复制。

## 接入原生会话

按 `Ctrl+N` 或 `Ctrl+B` 聚焦顶部成员栏，用 `←`、`→` 移动，再按 `Enter`。
也可以使用 `/attach <member>` 或 `@member /attach`。Asterline 会暂时挂起 TUI，打开
所选成员的原生交互式 CLI。请使用该 CLI 支持的退出方式（通常是 `/exit`）；只有后端接受
EOF 时，EOF 才会返回 Asterline。

新的 Claude 接入会由 Asterline 通过 `claude --session-id` 指定 UUID，返回后会自动导入并绑定。
Codex 只导入能安全识别的已绑定会话；Claude fork 只有在既有记录能唯一证明谱系时才会导入。
存在歧义的原生会话绝不靠猜测导入。Grok 和 Agy 可以恢复原生会话，但目前不会导入接入期间的消息。
