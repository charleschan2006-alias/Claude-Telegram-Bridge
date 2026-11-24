# 远程审批（Telegram 回答 Claude 的提问）

**状态**：一期（是/否）、二三期（选项 / 自由文本，已合并为一个功能）、四期（无头 turn 的审批）**均已实现并通过真机端到端验证**。

## 目标

away 模式下，Claude 需要用户决策时，不只是把问题推到 Telegram，还要能**在 Telegram 上作答**，答案回到会话里让 Claude 继续。分三期：

| 期 | 形态 | 例子 |
|---|---|---|
| 1 | 是/否 | `Claude needs your permission to use Bash` → 允许 / 拒绝 |
| 2 | 单选 A/B/C/D/E/F | `用哪个数据库？` → A) Postgres B) SQLite … |
| 3 | 排序 1/2/3/4/5 | `按优先级排列这几项` → 3,1,2（实为 2 的自由文本形态）|

1 已单独完成；**2 与 3 调研后合并为一个功能**一次做完（理由见下）。

## 可行性调研（2026-08-11，claude 2.1.227）

### 已否决：uds 消息通道

活跃会话的 `cc-socks/<pid>.sock` 只接受两类消息：

- `type:"user"` —— 普通用户消息（tinyCTB 直投功能已在用）
- `type:"control"` —— 仅 `rename` 与 `peer_message_status` 两个 action

**没有回答权限对话框的控制消息**。`can_use_tool` / `request_user_dialog` 是 remote-bridge（云端控制面）的 `control_request` 子类型，本地 socket 不接受。注入一条普通用户消息也无法解除一个正在等待按键的模态对话框。

### 已否决（仅限交互式会话）：`PreToolUse` hook

能返回决定，但**对每次工具调用都触发**，运行时还不知道这次是否真需要授权，因此必须自己维护"哪些工具危险"的名单、还要重新实现一遍 `permissions.allow` 的匹配规则——既啰嗦又必然与 Claude 的判定不一致（评审据此判为 P1）。

**这条否决只对交互式会话成立。** 无头（`-p`）turn 里 `PermissionRequest` 根本不触发，`PreToolUse` 是唯一可用的事件，所以四期正是用它来拦无头 turn——代价（工具名单、每次调用起进程）也如实付了：靠 matcher 把它限定在会改东西的那几个工具上。见文末「四期」。

### 选定方案：`PermissionRequest` hook 返回决定

`PermissionRequest` 的语义是 **"Run before permission prompt"**——只在 Claude Code 真要弹权限框之前触发。这正是我们要的触发点：**已被 `permissions.allow` 允许的调用根本不会走到这里**，所以不需要名单、也不需要复刻规则匹配。

输出契约：

```json
{"hookSpecificOutput": {
  "hookEventName": "PermissionRequest",
  "decision": {"behavior": "allow", "updatedInput": …?, "updatedPermissions": […]?}
            | {"behavior": "deny", "message": "…"?, "interrupt": bool?}
}}
```

hook 同步阻塞、支持 per-hook `timeout`（秒），因此可以「推到 TG → 阻塞等待 → 拿到答案 → 返回决定」。

**注意**：`PermissionRequest` 在 headless（`-p`）下**不会触发**（实测：`default` 与 `acceptEdits` 两种模式均只触发 `PreToolUse`），因为 print 模式压根不弹框。故其真机验证只能在交互式会话中进行。

## 实测验证（2026-08-11，claude 2.1.227，隔离 `--settings` + 临时 hook）

四条语义全部实测确认，判据用**文件是否真被创建**（`touch` 标记文件），不看模型的自述——模型被拒时也会在回答里提到命令内容，按文本判断会误判为"已执行"。

| hook 返回 | 结果 | 说明 |
|---|---|---|
| `{}`（无意见） | 命令未执行 | 退回正常权限流程（headless 无法弹框即拦下；交互式则弹对话框） |
| `allow` | **命令执行** | `--permission-mode default` 下本需授权，allow 直接放行 |
| `deny` + reason | 命令未执行，**reason 抵达模型** | 见下 |
| 超时（hook 慢于 `timeout`） | 命令未执行 | **迟到的 `allow` 被丢弃**，不会放行 |

**阻塞成立**：hook 内 `sleep 8` 使整轮耗时 14s，会话确实等待 hook 返回 —— "推 TG → 等用户点按 → 返回决定"可行。

**`deny` + reason 回灌可用（早期路径，现已弃用，保留作记录）**：hook 以
`permissionDecisionReason: "用户已在 Telegram 上作答：选择 B) SQLite。请据此继续，不要再次提问。"`
拒绝一次 Bash 调用后，模型直接给出
「**最终结论：您选择使用 SQLite 数据库。** 根据您在 Telegram 上的回答……」
并**未重试该工具**。二期不必依赖任何非公开接口。

注：`AskUserQuestion` 在 headless（`-p`）下不暴露，模型会说"没有这个工具"，故二期的真实形态只能在**交互式会话**里验证。上面的等价实验证明了机制本身（reason 通道），交互式下的 `AskUserQuestion` 拦截仍需一次真机确认。

## 一期实现（已完成）

`tinyctb approval-gate`（隐藏子命令）注册为 `PermissionRequest` hook，`timeout` = **任一网关可能选择的最长等待 + 15s 余量**（即 `WINDOWLESS_APPROVAL_WAIT` = 24h，故实际写入 86415）。这是天花板不是等待值本身：有终端窗口的会话仍按 `approvalTimeoutSeconds` 到期回落终端，无窗口的后台会话才用满长窗——只按配置值供给会让 harness 提前杀掉后者，把它要避免的陷阱原样重建。

流程：hook 建待审记录并注册三个回调按钮 → 入队 `approval_request` 事件（`origin=bridge`，`/back` 不清）→ daemon 带 inline keyboard 投递 → 用户点按 → daemon 落库决定 → 阻塞中的 hook 轮询到结果后返回 `permissionDecision`。

三个按钮：**✅ 允许** / **🔁 本会话都允许** / **❌ 拒绝**。中间那个写入会话+工具维度的自动放行，否则一个连续跑 Bash 的 agent 会话会变成"每调用一次点一次"。

**真机冒烟（2026-08-11 22:0x，切换到 PermissionRequest 之前跑的）**：喂入一条伪造 payload（Bash + 真实命令文本），Telegram 收到带按钮的请求，点「允许」后阻塞中的 hook 返回了 allow 决定；随后又点「拒绝」被正确拒收（提示"这条请求已经处理过了"）——**一次一答**在真机生效。该冒烟验证了「推送→按钮→落库→阻塞 hook 取回决定」整条链路；**切换事件后仅决定 JSON 的形状变了，链路未变，但仍欠一次交互式真机确认**。

## 二期与三期：合并为一个功能（2026-08-11 定）

用户观察「2 期和 3 期没啥区别」——调研确认这是对的，而且理由来自工具自身的设计。bundle 里 `AskUserQuestion` 的说明写着：

> AskUserQuestion **always includes a Skip button and a free-text input box** for custom answers

也就是说它本来就是**一个对话框、两种作答方式**：点选项 或 自由文本。所谓「A/B/C/D 单选」与「1/2/3/4/5 排序」不是两种问题类型（`questions[].options[{label,description}]` 里根本没有排序类型），而是同一个对话框的两种答案形态——排序只是自由文本的一个特例。因此二三期合并实现，一次做完。

**触发方式**：Claude Code 的 hook 事件全表（`PermissionRequest / PreToolUse / PostToolUse / PostToolUseFailure / Notification / Stop / PreCompact / PostCompact / UserPromptSubmit / SessionStart`）里**没有任何一个专门管问题对话框**，`PermissionRequest` **其实也会**为它触发（后台 fork 会对该工具弹权限提示；2026-09-11 实测 DB `pending_approvals` 116 行），但它承载不了答案、只是多余的权限提示——0.2.15 起 `run_approval_gate` 对 AskUserQuestion 直接 `allow`、不推送（见下方「权限层直接放行 AskUserQuestion」）。能把答案直接填进工具入参的只有 `PreToolUse`（见下方"正式契约"）。

关键是 **PreToolUse 的 matcher 就是工具名**，因此注册为 `matcher: "AskUserQuestion"`，只对该工具触发——不会重蹈「每次工具调用都启动一个进程」的覆辙。

**作答通道两条并存**：
- 选项 → inline keyboard，一点即答；
- 自由文本（含排序 `3,1,2`、Skip、以及任何不在选项里的答案）→ 对该消息用 Telegram 的 Reply 回复文字。

拿到答案后走**工具自身的正式契约**：`permissionDecision: "allow"` + `updatedInput = { questions: <原样>, answers: { "<问题文本>": "<答案>" } }`。`AskUserQuestion` 的 schema 里 `answers` 的说明是 *"The answers provided by the user (question text -> answer string; multi-select answers are comma-separated)"*，所以答案是作为**工具入参**送达、由工具正常完成并把答案作为结果交给模型，而不是靠模型去理解一条拒绝理由。

这里的 `allow` **不是权限授权**——提问工具没有副作用；授权仍然只能来自审批按钮。（早期版本用 deny + reason 回灌，实验证明可行但依赖模型理解，已弃用。）

**答案保真规则（2026-08-12 三审 P2 修正）**：字母代号（`A` / `a,c`）只在**整条答案全是代号**时才展开成 label；除此以外答案**一字不改**地送达。早期版本无条件按逗号拆分再重组，制造了两个 bug：点了 label 含逗号的按钮（`Washington, D.C.`）会被重组成 `Washington,D.C.`；自由文本 `A, but only locally` 会把开头的 `A` 换成第一个选项——而"but only locally"才是这句话的重点。识别顺序也因此固定为**先精确匹配 label，再考虑代号展开**。

**新契约真机验证（2026-08-12 01:01）**：交互式会话调用 `AskUserQuestion`，答案经 `allow + updatedInput.answers` 送回后，**工具正常完成并把答案作为结果交给模型**（`Your questions have been answered: … = "交给 Sol 再审"`）——与旧 deny 路径返回一条错误形成明确对比。

**旧路径真机验证（2026-08-12 00:00，deny + reason，现已弃用）**：由一个真实交互式会话调用 `AskUserQuestion`（那条问题本身即验证）→ 00:00:54 带 A/B/C 按钮推送到 Telegram → 00:01:26 用户点选 →「已作答：提交给 Sol 评审」→ 答案原样回灌进会话，模型据此继续未重试。同一晚也验证了审批的 **deny** 路径：一条未被 allow 规则覆盖的 Bash 调用被远程拒绝，会话收到 `Denied from Telegram` 且命令未执行。

★验证方法教训：**不要用 pty 脚本自动起会话**——新会话首次进入某目录会弹「信任文件夹」对话框，模拟按键时机极难对上，三次尝试全部卡死空等。直接在一个已存在的交互式会话里调用该工具即可。

**按钮样式（2026-08-12 与用户逐版敲定，定稿=彩色圆点+字母）**：选项按钮形如 `🔴A 选项文本`，颜色序列 `🔴🟠🟡🟢🔵🟣🟤⚫`。两个要素缺一不可——**颜色**让整行一眼读成按钮（与审批的 ✅🔁❌ 同一类），**字母**让多个选项彼此好区分、也是文字作答时可以直接说的代号。

逐版试错记录（避免重走）：纯 `A.` 前缀→混进正文文字，看不出是按钮；深色 keycap `1️⃣`→同样偏暗；区域指示符 `🇦🇧🇨`→用户客户端根本不渲染成彩色；只有彩色圆点无字母→够亮但"没有 ABCDE 好区分"。Unicode 没有全套 A–Z 彩色字母（`🅰️🅱️` 只到 B 且同为红色），所以"圆点供色 + 字母供辨"是现有字符集下的最优解。

选项文本**只出现在按钮上**，正文不再重复列一遍——重复会让按钮读起来像又一段文字。（多选无按钮时正文才列 A/B/C。）

**排布**：按标签显示宽度贪心装箱（CJK/emoji 记 2 列，预算 32 列，每行最多 3 个）。短标签并排（两个左右分布最好看），长标签自动各占一行。

**多选**：`multiSelect: true` 的问题**不给按钮**——一次点击会立刻提交并丢掉其余选项。这类问题只列出 A/B/C 并要求回复逗号分隔（`A,C`），正是工具文档规定的多选答案形状。答案里的字母会按选项顺序还原成 label，不在范围内的原样保留（自由文本因此不受影响）。

**已知限制**：一次调用含多个问题时不接管（回退终端对话框）；选项按钮最多 8 个。

## 授权边界（用户 2026-08-12 定）

**只有"对话框内"的作答才具有效力**，其余一律当作对话：

| 来源 | 语义 | 处理 |
|---|---|---|
| 审批消息下的**按钮** | 肯定句 = 授权 | 唯一能放行工具的途径 |
| 对**问题消息**的 Reply 文本 | 在对话框内作答（含排序 `3,1,2`） | 作为该问题的答案 |
| 对**审批消息**的 Reply 文本 | 不构成授权 | 拒收并提示改用按钮，**且不注入会话** |
| 普通消息（未 Reply 任何对话框） | 对话 / 疑问句 | 走原有注入或无路由提示，问题继续等待 |

Telegram 里「对话框内」映射为「Reply 明确指向那条对话框消息」——这是唯一等价于"在对话框输入框里打字"的动作，它显式指名了在回答哪一个对话框。

**识别与状态解耦**：`dialog_messages` 表登记该对话框的**每一个分片**消息（长问题会被拆成多条，按钮在最后一片），查找只按消息定位、不看是否仍开放。已作答或已超时的对话框，其回复仍被识别并如实告知状态（"已处理过""已超时，会话已回到终端"），**绝不漏回普通会话注入**。

这条边界是被一个真实漏洞逼出来的：此前文字回复审批消息会走普通回复路由被**注入会话**，用户以为已作答，实际审批仍在空等超时。

## 安全规则（不可妥协）

- **超时绝不自动允许**。但"不允许"具体是什么，取决于 hook 下游还有没有人：交互式会话返回空对象（等价 `ask`），退回终端弹窗，与今天一致；**无头 turn 必须直接拒绝**——它跑在 `bypassPermissions` 下且背后没有终端，那里返回空对象等于放行。四期加的这条同样是"宁可卡住也不能替用户点同意"，只是换了个方向实现。
- **away 只门控交互式网关**：人在电脑前时终端会话一切照旧，终端弹框。无头 turn 不看 away——它是 Telegram 发起的、无论用户坐在哪里都没有终端，审批永远走 Telegram（四期二审修正，初版误将 away 检查共用导致 away 关闭时无头审批整体失效）。
- **拒绝优先**：任何解析不出的答案、过期的回调、来源不符的 chat/user，一律按未作答处理。
- **一次一答**：回调用后即失效（`telegram_callback_routes.used_at`）。
- **文字永不构成授权**：权限只能由按钮给出；对审批消息的文字回复被拒收且不注入会话。
- **超时即作废**：待审记录带 `expires_at`；hook 放弃后把记录标记为 `expired`，之后再点按钮一律拒收并如实提示"已超时，会话已回到终端"，绝不谎报成功（尤其"本会话都允许"——hook 已退出，其副作用根本无法生效）。
- **超时与作答是原子转换**：`expire_or_take_decision` 用条件更新竞争同一行——赢了就标记过期，输了就把**已落库的决定读回来并遵守**。否则会出现"Telegram 显示已允许、会话却悄悄退回终端"的两不像状态。
- **审批请求必须带内容**：不能只说「需要权限」，要带工具名与具体入参（此前已实现 `pending_tool_use` 摘要，复用）。

## 实现要点

- 新增隐藏子命令 `tinyctb approval-gate`，注册为 `PermissionRequest` hook，`timeout` = 配置的审批等待 + 15s 余量（默认 300+15）。
- 过滤只剩两层：away 开启、且 `permission_mode != bypassPermissions`；`config.claude.approvalTools` 留空即"凡要问的都问"，填了则为可选收窄。
- 流程：hook 写入 `pending_approvals` 记录 + spool 一条事件 → daemon 推送带 inline keyboard 的消息 → 用户点按 → daemon 处理 `callback_query` 落库决定 → hook 轮询到决定后返回。
- 回调基础设施**已存在但未启用**：`telegram_callback_routes` 表、`extract_telegram_callback_route`、`TelegramCallbackAction::{Approve,Deny}`、`telegram_answer_callback_query` 都是移植期留下的，本期启用即可。
- 二三期：`tinyctb question-gate` 注册为 `PreToolUse` hook 且 `matcher: "AskUserQuestion"`（只对该工具触发）。单选 → inline keyboard；自由文本 / 多选 → Reply 该消息（逗号分隔）。答案经 **`allow` + `updatedInput.answers`** 送回（工具正式契约），`answers` 的 key 用**原始问题文本**（不 trim，否则与 `questions[].question` 不一致，工具会认为没作答）。

## 待验证 / 开放问题

- ~~`permissionDecisionReason` 能否承载「用户选了 B」~~ 已验证可用，但**已弃用**：改走 `PreToolUse` 的 `allow + updatedInput.answers` 正式契约（`PermissionRequest` 其实**也会**为 `AskUserQuestion` 触发——2026-09-11 实测；但它承载不了答案、只是多余的权限提示，0.2.15 起由 `run_approval_gate` 直接 `allow`；`updatedInput` 只在 `PreToolUse` 上可用）。
- ~~`PermissionRequest` 的真机验证~~ **已完成（2026-08-11 23:00）**：用 pty 起一个 `--permission-mode default` 的交互式会话执行未授权命令，`PermissionRequest` 如期触发 → 22:59:58 建待审 → 23:00:02 投递到 Telegram → 23:00:06 点「允许」后阻塞的 hook 放行 → 23:00:46 再点一次正确回报「已经处理过了」。**注意**：普通交互会话若带大量 `permissions.allow` 规则（本机 settings.local.json 有 395 条）几乎不会弹框，验证必须用干净的 default 模式会话。
- hook 长时间阻塞（数分钟）对会话其他部分（心跳、UI）的影响。
- 同一会话并发多个工具调用时，多条审批请求的对应关系（复用 `event_uid` 那套精确配对）。
- ~~三期排序的交互形态~~ **已定**：一条文本回复（`3,1,2`），与选项按钮并存。

## 四期：无头 turn 的审批（2026-08-12）

### 起因：远程审批恰好没覆盖最需要它的那些 turn

一期做的审批网关挂在 `PermissionRequest` 上，只对**用户自己开的交互式会话**生效。而你人在外面、从 Telegram 发起的那些 turn——最看不住的那批——反而一路裸奔。三个实测事实锁死了这个结论（脚本在 `~/.tinyctb/exp/headless-flow/`）：

| 实验 | 结果 |
|---|---|
| `claude -p --resume X` 连续三轮 | **不分叉**：session_id 始终不变、单个 transcript、3 个 user turn 依次累积。无头链路本身是连续的，TG 多轮和用户后来在终端打开的是同一条会话 |
| 无头里调 `AskUserQuestion` | **工具根本不存在**（两种 permission mode 都一样，模型 ToolSearch 明确返回 "No matching deferred tools found"）。三期做的问答网关对无头 turn 一次也不会触发 |
| 无头 + `--permission-mode default` | 需要授权的调用被**沙箱直接拒掉**（marker 文件没生成），无头模式没有弹窗可弹。这就是 bridge turn 用 `bypassPermissions` 的原因 |
| 无头 + `PreToolUse` hook | **触发**，payload 完整：`tool_name` / `tool_input` / `tool_use_id` / `permission_mode` / `cwd` / `session_id` |

最后一行是出路：`PreToolUse` 的 `permissionDecision: allow|deny` 和问答网关用的是同一套机制。

### 设计

**两个网关按 `bypassPermissions` 精确二分**，互不重叠也不留缝：交互式会话（非 bypass）归 `PermissionRequest`，无头 turn（bypass）归 `PreToolUse`。交互式会话即使切进 bypass 模式也不会弹权限框，所以一期网关本来就听不到它的调用——二分没有丢东西。有测试钉住这个划分（`the_two_gates_do_not_overlap`）。

**★超时语义在两侧相反，这是本期最要紧的一条**。一期的规则是"超时=不表态，回落到终端弹窗"。无头 turn 背后**没有终端**，而且跑在 `bypassPermissions` 下——在那里"不表态"等于"放行"。所以无头侧**超时必须直接拒绝**，并在拒绝理由里告诉模型"没有执行、不要重试、把你打算做的事告诉用户然后停下"。

**复审后收紧的三条（2026-08-12 二审）**：

1. **away 只门控交互式网关**。Telegram 的 `/new` 和 Reply 在 away 关闭时照样起无头 turn——那个 turn 无论用户坐在哪里都没有终端，所以无头网关完全不看 away。初版把 away 检查放在两个网关共用的最前面，等于 away 关闭时无头审批整个失效，`headlessApprovalTools` 里的工具直接执行。
2. **错误也遵循同样的不对称**：交互式网关内部出错降级为"不表态"（终端弹窗兜底，安全）；无头网关一旦确认是运行中的 bridge turn，任何内部错误（配置读不了、SQLite 失败、事件入队失败）都**直接拒绝**并把错误写进拒绝理由——那里没有任何东西兜底，"不表态"就是执行。初版所有错误都降级成 `{}`，等于 fail open。
3. **准入判定=「该会话有 status='running' 的 bridge turn」，且在建审批/推送之前最先做**。只看表成员资格会让"上周跑过一次 Telegram turn"的会话被永远纠缠；判定放在最后则会给用户自己的 `--dangerously-skip-permissions` 终端会话建审批、白等整个超时，还谎称"超时回落弹窗"（bypass 模式根本没有弹窗，超时后照样执行）。现在这类会话在任何有副作用的步骤之前就被无声放过——不建审批、不推送、不等待、也不暴露在 fail-closed 错误处理之下。首工具竞态由**spawn 前注册**根治：`bridge_turns` 行在进程存在之前就已落库（CLI 的 `tinyctb new`/`reply` 也一并走这条路，此前它们从不注册）。

**工具过滤是唯一挡住热路径的东西**。`PreToolUse` 对**每一次**工具调用触发，所以 `headlessApprovalTools` 的空列表含义与 `approvalTools` **正好相反**：后者空=全都问（`PermissionRequest` 只在真需要决定时才触发，没什么可滤的），前者空=**关闭功能**（否则连 Read/Grep 都要你掏手机）。默认值 `["Bash","Write","Edit","MultiEdit","NotebookEdit"]`——只有会改东西的工具。`WebFetch` 想加可以加，默认不加是因为它太常见。

实测开销：无头网关的快路径**不读 away 标记也不开数据库**——非 bypass 调用在模式分流处退出，bypass 但无令牌（用户自己的终端会话）在环境变量检查处退出。**200 次调用共 107ms，约 0.53ms/次**（三审后复测）。away 标记文件只属于交互式网关的快路径。

**⚠️ 已知缺口（二审后已收窄）**：matcher 在 `hooks install` 时写死进 `settings.json`，改了 `headlessApprovalTools` 不重装则新工具不会被拦。现在 `hooks status` 按 marker 分别核对四个 hook**并比对 matcher 与当前配置**，不一致即报 `installed: false`——`/away` 复用这个判定，会自动重装。剩余缺口只在"改完配置后既不跑 status 也不碰 /away"的窗口期。matcher 取"内置默认 ∪ 配置"的并集，保证内置那批无论配置怎么改都仍然被拦。

### 真机冒烟（2026-08-12 18:0x）

隔离在 `TINYCTB_STATE_DIR` 里跑（子进程导出该变量，hook 继承），不碰生产库也不发 Telegram 消息。判据是 **marker 文件是否存在**，不是模型自述——本项目早先有一次实验就是因为信了模型自述而完全跑偏。

| 场景 | 命令是否执行 | 落库 decision | 模型收到 |
|---|---|---|---|
| A 无人答复（15s 超时） | **NO** | expired | "not run — the Telegram approval timed out"，未重试 |
| B 点 ✅ 允许 | **YES** | allow | Done |
| C 点 ❌ 拒绝 | **NO** | deny | "denied … was not created" |

推送事件带 `headless: true`、三个按钮、正文是真实命令。Telegram 提示语按此分流：无头那条明说"背后没有终端可回落——超时不答复=拒绝，任务就停在这里"，交互式那条说"超时后回落到终端弹窗"。不说清楚的话，忽略这条消息看起来是免费的。

### 仍未解决

- 无头 turn **没法中途提问**（工具不存在）。需要拿主意时它只能猜，或者把问题留到 turn 结尾由用户回复继续。给模型加一条"别猜、把选择题放到结尾"的提示是候选方案，尚未做。
- ~~matcher 陈旧问题没有自动检测~~ **已解决（二审）**：`hooks status` 按 marker 分别核对并比对 matcher 与当前配置，不一致报 `installed: false`，`/away` 会自动重装。

### 三审修正（2026-08-12，Sol 复审 1P1+2P2）

**[P1] spawn 后身份补写失败会让活进程脱离审批边界**。初版 `let _ =` 丢弃 UPDATE 错误：pid 留空 → daemon 10 秒宽限后判 turn 失败 → turn 不再是 running → 该进程后续所有工具调用被网关放过，且 daemon 无法按 PID 回收。现在补写**重试 3 次并核对恰好更新 1 行**（针对 daemon 自身连接造成的瞬时 SQLITE_BUSY）；仍失败则**杀掉刚起的进程组、把 turn 结算为 failed、向调用方报错**——宁可这个 turn 没跑成，也不能留一个无人监管的活进程。注入式回归测试：`spawn_that_cannot_be_recorded_is_terminated`。

**[P2] 准入第一层改为环境令牌，数据库退居第二层**。bridge 起的每个无头进程带 `TINYCTB_BRIDGE_TURN=<turn_id>` 环境变量（hook 是 claude 的子进程，天然继承）。网关先看令牌：**无令牌 = 非 bridge 进程，立即放过，全程不碰数据库**——状态目录烧成灰也波及不到用户自己的 bypass 终端会话（测试：把 TINYCTB_STATE_DIR 指向一个普通文件仍返回 `{}`）。有令牌才开库核对该 turn 是否仍 running：已结算（如超时被杀）的 turn 的余党调用**直接拒绝**，不再新建指向已关窗口的审批。令牌只存在于该 turn 的进程树里，"上周的 Telegram turn 纠缠今天的终端会话"从结构上不可能。⚠️ 部署瞬间已在跑的存量 turn（旧二进制起的、无令牌）不受门控，属一次性窗口。

**[P2] 未知 decision 按网关分流**：交互式 `{}` 回落终端弹窗；无头侧 `{}` 即执行，故改为明确 deny 并写明原因。

三处修复均按惯例回退验证过测试确实变红。

### 四审残余（2026-08-12）

身份补写 UPDATE 限定 `AND status='running'`（daemon 抢先结算的 turn 不可被误报成功）；结算错误经 `settle_failed_turn` 重试并折进错误链，连结算都失败时的遗留契约=行保持 running/pid NULL 由 daemon 宽限期响亮兜底；两种交错各有注入测试。

### 五审修正（2026-08-12，反向竞态）

四审那条竞态还有**镜像**：daemon 用快照判死。交错=daemon 读到 `pid NULL` → 身份补写成功（调用方拿到"已启动"）→ daemon 拿旧快照判"无 pid=死亡"→ 推失败通知+无条件写 failed → 令牌指向 failed 行、后续受控调用全被拒、结果永不投递。修复=**先认领后宣判**：失败转换改为条件 UPDATE（CAS），"无 pid"这一判据在写入时重新断言（`status='running' AND pid IS NULL`）；0 行=判据已失效，**既不写 failed 也不发 thread_error**，turn 照常运行。超时判死不依赖快照证据，不带 pid 条件。测试 `identity_write_landing_after_the_snapshot_averts_the_failure_verdict` 钉住该交错并断言无 thread_error；回退成无条件写立即变红。

### 六审修正（2026-08-13，判死副作用的崩溃一致性）

CAS 之后副作用的**顺序**还有两处崩溃窗口：①claim 提交后、kill 前 daemon 退出→turn 已是 expired、被所有后续 `list_running_bridge_turns` 扫描隐藏，**活进程永远无人回收**；②kill 后通知入队失败→turn 已终态、下一轮不重试，**用户永远收不到 thread_error**。修复=重排：**kill 在任何提交之前**（此时崩溃则 turn 仍 running，下一轮重判超时并重复幂等的 kill）；**claim 与失败通知在同一事务提交**（入队失败则 claim 一起回滚，整个判决下一轮重试；不可能出现"已结算却没人被告知"）。测试 `broken_outbox_rolls_back_the_claim_and_the_kill_is_repeatable` 用 SQLite ABORT trigger 模拟 outbox 故障，断言 claim 回滚、kill 已先落地、修复后重试恰好投递一次；回退成旧顺序立即变红。155 测试全绿。

## 键盘在场检测（2026-08-15，用户否决"长窗口阻塞终端"后定案）

用户要求：**终端无论 away 与否都必须能直接响应**；同时 TG 推送和 /threads 重发在窗口未过期时有效。两侧同时可答在 Claude Code 现有接口下做不到（对话框无法远程作答、uds 无对话框控制消息、注入用户消息不解除模态框——一期调研已证），但可以做到**谁在场谁优先、切换以毫秒计**：

**信号 = 最近一次按键**（daemon 常驻 `xinput test-xi2 --root`，**只挑 RawKeyPress**，时间戳写入 `input-activity.json`；网关读该文件判 15 秒窗口）。★2026-08-17 换掉了原来的「桌面输入空闲时间」（Mutter IdleMonitor via D-Bus）：它把键盘与指针混在一起计时，而 Logitech G502 传感器漂移每秒产生 ~600 次零位移 Motion，把 idle 长时间压在 1–48ms，导致每条审批在第一次轮询（100ms）就被判「人在键盘前」转交终端——按钮在手机上只活 0.1 秒，`/threads` 事后查无活请求。gdbus introspect 确认该服务只暴露 `/Core`，**没有按设备路径**，无法拆分键鼠，故必须在事件层过滤；`/dev/input` 因用户不在 `input` 组不可读，XInput2 是零权限变更的路径。随之删除的旧机制：gdbus/xprintidle 探测、`presence-probe.json` 共享状态、flock 租约、双重检查、指数退避、时钟回拨校验及其 9 条专属测试。**判据方向**：读不到记录（无 X 会话／监听器未起／纯 SSH／时间戳来自未来）一律判**不在场**，提示留在手机上；away 是用户声明，看不见的守卫不该推翻它。写端节流用单调时钟（`Instant`），避免墙钟回拨后拒绝记录真实按键。⚠️更早还用过 pts atime，被 Sol 用真机 pty 实验证伪（raw 模式读取键入 atime 纹丝不动）——间接证据当成了机制验证。`/back` 仍是确定性释放路径。

规则（approval 与 question 两个交互式网关一致，无头网关不涉及）：

0. **会话级 auto-allow 先于一切在场检查**（已授权的绝不因人在键盘前复弹终端框）；
1. **away 开启即推送**：桌面在做什么完全不参与「发不发」的决定——2026-08-17 实测鼠标传感器漂移每秒发 ~600 次幽灵 motion，把桌面 idle 压在 0 附近，旧的「判在场就不建行、不推送」会让手机**完全收不到消息**且用户无从察觉。推送先落库再由桌面活动决定让路，最坏情况从「消息不存在」变成「消息很快被终端接管」。
2. **键盘空闲 → 远程窗口照常**（推按钮、/threads 可重发、最长可配 24h——clamp 已放宽到 86400）；
3. **等待中用户回来（**按键**苏醒 或 `/back`；指针移动不算）→ 原子结算转终端**：已落库的 tap 仍然生效（`expire_or_take_decision` 竞争），否则标 expired、返回不表态、终端弹框；之后再点 TG 按钮收到诚实的"已超时，会话已回到终端"。释放延迟=轮询粒度 100ms（监听器实时写时间戳，无探测节流）。★注意终端侧的**可答性**并不依赖这个释放：2026-08-17 pty 实测确认 Claude Code 在 hook 阻塞期间就已渲染自己的权限对话框（对话框出现在 hook 起点而非返回点），释放只决定远程窗口何时关闭。**`/back` 在释放判定里最先检查**，不依赖监听器是否在跑——不依赖任何桌面接口，SSH 场景也确定有效，延迟即轮询粒度（100ms）。

由此长远程窗口变得无代价：在电脑前对话框始终在终端可答（hook 阻塞期间即已渲染），手机那份随桌面活动交回；不在时窗口一直开着。本机 `approvalTimeoutSeconds` 已配 3600。

测试五条：away 开启即推送（在场也建行也推送，随后才让路）、auto-allow 优先于在场、用户返回即释放（行 expired+曾推送）、`/back` 即释放（无桌面信号）、tap 抢先仍生效；回退各变红。测试覆盖读端真实生产路径（真 JSON 文件、未来时间戳、坏数据一律 fail-away）与写端行处理（RawMotion 不写／RawKeyPress 写／单调限流／EOF 结束／多候选并行监听（各自持有并回收子进程、各自重连、按候选增删；发布经共享锁全局限流并原子 rename，任一按键即落盘）），不再只靠环境接缝。

配套（同轮 Sol 复审的另三条）：终端等待会话 union 进 /threads 候选池（真实库 50/51 边界测试）；等待层 tier-1 要求 **presence == Window**（幽灵会话的陈旧 prompt 行、以及本就无窗口的 Background 会话都不得置顶）；状态行措辞中性化"终端有对话框在等你（需要在终端处理）"（不再断言"远程窗口已过"——prompt 可能因让路而从未有过远程窗口）。

## 投递语义（2026-08-17 定）

**at-least-once（可能重复），不是 at-most-once。** 发送与 transport log 落库无法原子（一个跨网络、一个本地），daemon 若崩在两者之间，就进入不可判定窗口：Telegram 已接受但本地无记录，恢复时会重发，用户可能看到同一请求两次。

选它的理由：反过来（先记日志再发）用可见的重复换静默丢失，而这个 outbox 存在的全部意义就是需要用户作答的请求不能凭空消失。重复是显眼且可处理的（一答一行，点第二条会收到已经处理过了）；丢失则和会话卡死长得一模一样。

## 后台 fork 的原生对话框：attach 双向作答（2026-09-02 定，`src/fork_dialog.rs`）

> **2026-09-03 重构（钩子权威记账，取代下方旧「先投递后记账/刮屏判 Injected」细节）**：真机验暴露了旧设计的因果缺陷——PTY 刮屏无法证明是哪一面关的框，误判会不可恢复地记错答案。现改为：**注入侧只「送键」不记账**（`InjectOutcome::Delivered` = 拿到选择器 chrome 后、**写任何键前**再查 DB「此题仍 `Open`」——`still_authorized`，这是 chrome 给不了的**问题实例身份**，防已在飞的旧注入把旧选项打进新题；通过才写数字+回车。写数字后**不再拦**：数字/回车中途 I/O 出错也算已送达、返回**不可重试**的 `Delivered`，免得重试重复打字。`Unreachable` 只在写键**前**发生（无 chrome，或题已不 `Open`），故"零键"始终成立。`attach` 用 8 位短 id、`execve` 补 `TERM`——见下方真机两真凶）；**最终答案一律由 `PostToolUse(AskUserQuestion)` 钩子从 `tool_response.answers` 读出 fork 真正取的选项、权威记账**（`state::record_native_answers_from_hook`，按 thread+question 精确匹配、只填仍开放的当前行、不覆盖历史已答行；`tool_response.answers` 的 key 逐字等于行存的 `question`，2026-09-03 手机注入与本地键盘作答均已实证）。**建新问题时结算掉同 fork 的旧开放 native 行**（`state::settle_stale_native_questions`），保证每个 fork 至多一个开放 native 行，杜绝旧按钮把旧选项打进新问题。`Delivered` 返回独立的 `ApprovalAnswer::Delivered`（callback 审计 `recorded:false`，按钮消费由 `retryable=false` 控制）。已删：刮屏判 Injected、回车后「框消失」判据、`repaint_and_capture`/`flush_pending`、`state::record_native_answer`。**真机挖出的两个投递真凶（dev harness 遮住数月，只有真机端到端能暴露，2026-09-03 修）**：① **`claude attach` 只认 8 位短 id（`claude agents` 的 `id`），daemon 却传完整 session UUID** → attach 报 `No job matching '<uuid>'` 立退 → 每次注入恒 `Unreachable`。改 `with_attach_pty`／`attach_window_argv` 都用 `short_id()`。② **daemon 的 systemd 环境无 `TERM`** → `claude attach` 渲不出选择器（无 `Enter to select` chrome）。`with_attach_pty` 改为父进程构造带 `TERM=xterm-256color` 的 envp、子进程 `execve`（异步信号安全，envp 在 fork 前建好）。全链路真机实证：手机点→送键→fork 答→钩子从 `tool_response.answers` 记真答案→fork 继续（v0.2.12）。下方 2026-09-02 段落为旧设计留档，细节以本注为准。

**问题的由来**：后台 fork（`claude --bg` / daemon fork）在一块没人看的隐藏 pty 上渲染 AskUserQuestion。此前 windowless 单选走「扣住工具调用 + 手机填 `updatedInput`」：手机能答，但本地看不见、答不了。用户要「双向、谁先抢答算谁的」，且要用**原生对话框**（自造的横幅/zenity 都被否）。

**承重事实**：`claude attach <id>` 不只只读——用 pty 起 attach 客户端、向其 master 写字节，键真的送达 fork 的对话框。attach 是纯查看客户端，**零模型 token**；fork 本就带上下文在等，答完继续同一回合，与手机答开销相同。**全程不停、不发信号给任何 TUI**——只开新窗口、像人一样敲键，这是它安全、而 SIGSTOP 接管致命（bash 作业控制抢前台 + SIGTTIN 冻死，见 `terminal-takeover-impossible` 记忆）的根本区别。

**如何选中一项（2026-09-03 真机实证，推翻早前的源码猜测）**：真交互 AskUserQuestion 框给选项**编号**（`1. RED`、`2. GREEN`…），**打选项的 1-based 数字即选中**——实测对 index 2 注入数字"3"+回车，fork 答出第 3 项 `BLUE`。方向键也能导航（真框底部提示 `Enter to select · ↑/↓ to navigate · Esc to cancel`），但**打数字是绝对选择**（不依赖别的 attach 客户端把高亮停在哪），正是两面抢答需要的。故手机注入 = **打 `index+1` 数字 + 回车**（`option_digits`）。（早前从二进制 `FLt` 组件读到的 `Select with numbers [1-N]` 是**另一个非交互渲染**、不是真交互框——7 轮静态审都没发现，正是真机验挖出来的。）数字与回车**分两步写、之间再确认活框**：回车是唯一会"提交"的键，只在框仍在时才发；本地此刻抢答、框已消失时，那颗孤零数字未提交地落在主框（低危、不误发），不发回车、判 `Unreachable`。回车后再强制重绘看当前屏：`Injected` 要**正证据＝新帧非空且 chrome 没了**（fork 关框后重绘了下一状态）。`claude attach` 镜像仍在跑的会话，成功后 fork 继续工作、attach 持续产出，故正常成功是**非空帧**；**空帧＝attach 没产出＝它死了（崩溃）**、**框还在＝没吃下**——两者都判 `Unreachable`。空帧的两向歧义按**不对称危害**定案（见⑤）：空帧判 Injected 会记下 fork 可能没收到的答案、**不可恢复**（行已终结、钩子的 `answer IS NULL` 再也修不了、问题被静默丢弃）；判 Unreachable 只是把"真成功但不可见"报成可重试、**可恢复**（钩子仍会在 fork 真答时结算行、/threads 重推）——取可恢复的一侧。

**在场判定（三审 P1-1）**：不刮问题/选项文字（进历史回显判不准）；也不信连接窗口里累计的原始流。而是 `repaint_and_capture`＝**先 `flush_pending` 清掉已排队的旧字节、再强制重绘（`TIOCSWINSZ`→SIGWINCH）、只看这一帧**（清了帧边界，三审 #2）。签名用选择器专属 chrome `Enter to select`（`SELECTOR_CHROME`，真框底部提示 `Enter to select · ↑/↓ to navigate · Esc to cancel`；答后折叠成 `User answered…` 就没了、不进 scrollback、英文固定；2026-09-03 真机实证——早前的 `Select with numbers` 永远匹配不上真框）。

**新流程（仅 windowless 单选、非空选项；多选与 free-text 走扣住+填/comma；审批留 v0.2.10）**：
- **gate 放手**：windowless 单选非空选项分支建行、推手机按钮、`pop_attach_window` 弹 `claude attach` 窗口、`no_opinion` 让原生框渲染、建行后写 `native_attach`。空选项（free-text）不放手，落回扣住+填（Sol P1-4）。**权限层直接放行 AskUserQuestion（0.2.15，2026-09-11）**：`PermissionRequest matcher='*'` 的 `approval-gate` 原本对 AskUserQuestion 也走完整审批路径、再推一条「允许 AskUserQuestion？」到手机——与 question-gate 推的选项按钮**重复**，点完选项它已过期（「这个按钮已经处理过了」；DB `pending_approvals` 实测 **116 行**冗余）。现 `run_approval_gate` 读到 `tool_name==AskUserQuestion` 即 `GateKind::Interactive.allow`，在 away／terminal-watch／DB 之前短路：手机只收**一条**（选项），pty 也不再先弹 yes/always/no、选择器直接渲染。安全依据：该工具只「问用户」、无副作用可门控，真正的选择由 question-gate／answered-gate 记账。**多问题调用（0.2.16，2026-09-12）**：一次 AskUserQuestion 带 ≥2 题时，gate 原本 `questions.len()!=1` 静默让路——不建行、不推手机、连 attach 窗都不弹（早退在弹窗前），远端用户对后台 fork 就是**静默卡死**（真机：mahjong fork 两次各两页的提问从未到手机）。**真机机制（三次抛弃 fork 探针、transcript 铁证）**：多题原生框是**一个框+顶部标签栏** `← ☐题1 ☐题2 ✔Submit →`（☐未答／☒已答，当前页有底色 `48;2;177;185;249`）；任意页**打数字＝选中并自动跳下一页（不需回车，不跳过已答页）**；`←` 在首页**不绕回**；全部答完落到 Submit 页，回车一次性返回全部答案（工具语义原子，无半提交）。**现在（用户定）**：每题**单独一条**「【第 k/N 问】」消息推手机、按 `question_id` **任意顺序**答、答完最后一题**自动提交**；驱不了的题（多选需空格勾选、自由文本无选项）推「请在电脑答这一题」的通知、交互终端会话的多题也推通知——**绝不静默**。实现：`release_question_batch` 一个事务建 N 行（仅可注入题建行，共享 `batch_id`=tool_use_id、`batch_seq`=页序、`batch_total`）、推 N 条、弹一次窗、`no_opinion` 放行；`settle_stale_native_questions` 改按**批次键** `COALESCE(batch_id,question_id)` 只结算别的批次（同批 N 行并存）；手机点某题→`inject_option(..., Some(MultiTab{seq,total}))`：`←`×total 归位（靠不绕回）→`→`×seq→强制重绘（TIOCSWINSZ 抖列数）读标签栏 `tab_bar_state`（只解析从 `←` 到 Submit 的**这一条**栏）→ **必须**是 N 页且当前页 `☐`（若人在键盘已答成 `☒`，打数字会落到下一页答错题→零数字、`Unreachable` 可重试）→ 只打数字；重绘后若 `☐` 为 0 → `→`×total 到 Submit 回车提交。送达后 `mark_question_delivered`（`delivered_at`）→ 该行状态 `Delivered`，重点不再打字；真答案仍由 PostToolUse 从 `tool_response.answers` 按题文一次填满 N 行。**Sol 一审收紧（2026-09-12）**：①标签栏加**当前页序号**（高亮前的 ☐/☒ 个数）——注入前须 `current_index==seq` 且该页 `☐`，`☐` 单独只证"某个未答页"，人在键盘切页时会打错题；②导航耗时后、打数字前**再查一次** `still_authorized`（fork 可能已换到同页数的新批次）；③自动提交前 `→`×total 后**重绘确认当前页是 Submit** 才回车，绝不盲按；栏读不到重试 3 次仍不可读则**不提交、仍 Delivered**（数字已打进去），并加兜底——任一次点按进来先读栏，若已全 ☒（卡在 Submit）直接提交；④`/threads` 的 `open_prompts` 排除已送达行（`delivered_at IS NULL`）、同批按 `batch_seq` 取最小未答页（逐题推进，每线程一题的折叠结构不变）；⑤notify-only 事件标 `notifyOnly`：提示语改「这一题手机答不了，请到电脑答」且**不注册回复路由**（回复不会当 prompt 打进 fork）；⑥N 条推送入队挪进同一事务（与建行/路由/结算原子）；⑦answered 钩子在"无 native 行"早退前也 `close_attach_windows`（全不可注入批次也弹了窗）。**Sol 二审收紧（2026-09-12）**：⑧**延后提交要有路可达**——新 `InjectOutcome::SubmitPending`（数字已打、提交未确认：栏读不到或没走到 Submit）：调用方标 `delivered_at` 但**按钮保活**（`retryable=true`）；已送达的批次行再被点 → `inject_submit_only`（attach→读栏→全 ☒ 且恰 N 页→`submit_batch`，**绝不重打数字**）；`/threads` 对"整批已送达、无人提交"的批次重推最小页作为提交入口；⑨`submit_batch` **绑定批次**——要求 `answered==total && unanswered==0 && current_index==total && 含 Submit`，且**回车前再查授权**，不满足→SubmitPending 不按回车；⑩位置 TOCTOU 收窄——授权复查后**再读一次栏**核对位置与 `☐` 才打数字（残余＝重绘读→打字的毫秒级窗口，与单题路径同级）；⑪**活框签名加 `Submit answers`**——停在 Submit 页时页脚不再有 `Enter to select`（探针实测），否则 `wait_for_chrome` 会把停在 Submit 的框判成不存在、所有恢复路径都走不到。**Sol 三审收紧（2026-09-12）**：A/B/C 确认 fully fixed（自动提交这一形态收口）；新 major＝⑪那个 `Submit answers` 签名不能**全局**生效——单题路径不读标签栏、chrome 一到就打数字+回车，transcript 回显里的"Submit answers"会让它误判活框。⑫改为：`dialog_present` 仍只认 `Enter to select`；`submit_tab_present` 单列，`wait_for_chrome(…, also_submit_tab)` 只在**会校验标签栏再打字**的路径（多题注入、只提交恢复）为真；⑬加测 `a_revoked_batch_gets_no_enter_even_on_its_own_submit_tab`（合法同尺寸全 ☒ Submit 栏、授权在回车前被撤→只走到 Submit 不回车、SubmitPending）真正压到 `submit_batch` 的最终绑定；⑭`/threads` 同批子查询按 `(thread_id, batch_id)` 限定（`tool_use_id` 跨会话重放/碰撞不再互相污染）。**Sol 四审（2026-09-12）**：三项生产修复全部确认；仅余测试覆盖 nit——⑮撤权测试改成两次运行（`head -c 6` 证走到 Submit；`head -c 7` 捕获为空证未发回车），stub 契约＝`head -c N` 只在收满时落盘。**真机端到端挖出的真 bug（2026-09-12，用户亲点）**：四次点按全部 `not the unanswered tab on screen (None) -> Unreachable`——导航后**读不到标签栏**。根因：`force_repaint` 的两次 `TIOCSWINSZ`（199→200）**背靠背无停顿**，TUI 异步处理 SIGWINCH、去读尺寸时已回到 200＝"没变化"→不重绘→空帧；能读到栏的探针都在两次尺寸之间 sleep 了 150ms。stub 自行连续重印、单测抓不到（又一次 [dev harness 遮蔽]）。⑯修：`force_repaint` 在 199 **停 150ms** 再回 200；`read_bar` 跨重试累积字节、每次等 `key_gap×6`；`navigate_to_tab` 把箭头增量也累积（每按一次箭头栏都重画，解析器取最后一条）。用户在手机注入失败后于电脑窗口只答了第 2 题并提交→fork 记 `(未作答) 雨`、DB 第 1 题被匿名结算——这部分是 Claude Code 允许跳题提交的既定语义，非 tinyctb 缺陷。**真机 bug ②（同日第 5 次 e2e）**：重绘修好后栏能读（`answered 0, unanswered 2`），但 `current: None`——高亮色码找不到。根因：探针跑在有 `COLORTERM=truecolor` 的 shell 里，看到真彩 `48;2;177;185;249`；daemon 的 systemd 环境**无 COLORTERM**（也无 TERM，TERM 早已补），TUI 退回 256 色、高亮成 `48;5;n`，精确字节匹配失败→7 次点按全 Unreachable。与 0.2.12 缺 TERM 同根：**探针环境 ≠ daemon 环境**。⑰修：解析器只匹配 SGR 前缀 `ESC[48;`（真彩/256 色皆可，跳到该 SGR 的 `m` 再跳后续 SGR）；`with_attach_pty` 补 `COLORTERM=truecolor`（与 TERM 同法）；单测加 256 色栏夹具。**顺带暴露隐患③**：0.2.13 焦点助手把弹出窗抢到前台，用户此时在电脑打字/按回车，按键被劫持进对话框、按默认项提交整批（第 4 次 e2e：推送后 5 秒"包子/晴"、PC 键盘活动同刻）——待用户定策略（如键盘 N 秒内有活动则不抢焦点）。**用户定（2026-09-12）：保持现状（总是抢焦点），隐患③按已接受记档**——弹窗即前台方便回电脑直接答；代价是人正在电脑打字时按键可能被打进对话框，属已知取舍，不改。**真机端到端闭环（2026-09-12 第 7 次，用户只在手机点）**：两题各由手机注入（daemon：`tab 1/2 typed -> Delivered`、`all 2 tabs answered -> submitted -> Delivered`），DB 两行 `delivered_at` 置位、答案 豆浆/雨，fork 的 `tool_response.answers` 与之逐字一致、fork 继续回合——⑯⑰两修在 daemon 真实环境下验通。（第 6 次无人点按超时，无结论。）

- **交互终端会话双向作答（0.2.17，2026-09-17）**：交互（Window）会话 away 下原本走 **hold 路径**——钩子扣住 AskUserQuestion、手机填答、终端只画横幅，**PC 键盘答不了**（违反「PC 永远能答」）。后台 fork 走 `claude attach` 注入，但普通交互会话的 pty 归 gnome-terminal、attach 不了。**0.2.17 对「Window 会话 + 已记住终端窗口 + 单选非空选项 + away」这一种**：`run_question_gate` **释放**（`no_opinion`）让 Claude Code 的**真原生选择框**渲染在终端里（PC 键盘直接答），手机点按经 **XTEST** 打进那个终端窗口（`_NET_ACTIVE_WINDOW` source=2 聚焦 + XTEST 数字⏎）。谁先答谁算，`PostToolUse(AskUserQuestion)` 钩子从 `tool_response.answers` 权威记账、两面通用。**窗口定位＝「记住」不是「查」**（标题 OSC 被 TUI 锁死、BEL 被吞、gnome-terminal D-Bus 列不出 screen↔window，三条内省全灭；真机 probe1-7 验证）：`run_prompt_context`（UserPromptSubmit）在**本机键盘刚有动作**（`input-activity.json` 新鲜 ≤5s）且是 Window 会话时（每次都抓、变了才写；手机注入的提问不抓），抓 `_NET_ACTIVE_WINDOW`（那一刻焦点窗口必是本会话终端）存 `session_terminal_windows(thread_id→x_window_id)`，复用。**最小爆炸半径**：只有这一种情况改；无记住窗口的交互会话（仍 hold）、后台 fork（仍 attach）、多选、空选项、多问题批次全部不变。**数据面**：新表 `session_terminal_windows`、`pending_questions.inject_window`（Some=XTEST 进该窗，None=fork attach/held）；`mark_question_xtest_window` 置 `native_attach=1`+`inject_window`（手机侧经 `question_is_native_attach` 路由，`inject_window` 区分 XTEST vs attach）；两个内嵌 python 助手 `capture_window.py`/`xtest_answer.py`（materialize 原子发布，与 focus 助手同法）。**Sol 一审收紧（2026-09-17）**：①[高]授权检查非「按键前」——改**两相**：`xtest_answer.py focus`（慢，等聚焦≤1.5s）→ Rust **聚焦后、打字前再查一次 `still_authorized`**（人在键盘已答则零键）→ `xtest_answer.py type`（快，打字前再校验窗口仍在且仍是终端），把「键落进已折叠选择框」的窗口收窄到打字相（与 attach 路径同级）；②[高]未来时间戳 fail-safe——`local_input_fresh` 改 `last<=now && now-last<=within`（`saturating_sub` 会把未来戳判成刚输入）；③[高]XTEST 分支也 `settle_stale_native_questions`——防上一题 PostToolUse 结算失败留 Open 行、旧按钮把旧选项打进新题；④[中]每次本地提交热路径——`remember_session_window` 先**只读探测** `session_terminal_window_readonly`，只在未记过且捕获成功时才开可写连接（避与 daemon 争写锁）；⑤[低]终端判定 `b"term"` 大小写不敏感（gnome-terminal/xterm/terminator…；kitty/Alacritty/konsole 无 term 则退回 hold），注释改准。**用户拍板的两点（2026-09-17，非 bug 记档）**：(a) [中]**释放后手机对这类题只能点列出的选项、不能再输自由文本（Other）**——用户定「先发只点选项版，PC 键盘保留自由输入」；away 时手机对这类题少一项，属已接受取舍（手机 XTEST 键入 Other 文本留作后续）。(b) [高]**多 tab 固有限**——记住的是 X **窗口**不是 tab，若同一 gnome-terminal 顶层窗口有多个 tab、作答时当前 tab 已换成别的 shell，XTEST 会打进错 tab；X 层无 window→tab→pts 映射、无法关死。用户会话皆独立窗口不用 tab，风险低，按固有限记档（同 pty/argv 类）。**闸门**：fmt/clippy 退0、python 编译、全量 3×442。**待真机端到端**（一次性会话，非本会话）：手机点→XTEST 落进真选择框→工具返回注入选项。 **Sol 二审收紧（2026-09-17，全为新问题，一审 6 条已确认修好）**：⑦[高]XTEST 打字打到「当前焦点」不针对窗口，两进程拆分反而在 focus 退出与 type 启动之间留焦点漂移缝——`xtest_answer.py type` 改为**同进程内**先核 `active()==target`、不对则 source=2 重聚焦等≤0.8s、仍不对 return 4，确认后才打字；⑧[高]记住的窗口没牢固绑定会话——改**每次本机新输入都捕获、变了才原子写**（`INSERT OR REPLACE`），会话 `--resume` 到别的终端会自动重绑；DISPLAY/X-authority 与非标准键盘布局作为**单显示器+US 布局假设**记档（同多 tab 类固有限，本机单屏 :1）；⑨[中]首键发出后失败原报 Unreachable（可重试→会重复打），改为**首键一发即算 Delivered**（python 打字过程 catch 后若 started 仍 return 0，与 attach 同）；⑩[中]`!windowless` 含 Unverified，收紧为 `session_window == SessionWindow::Window`（只 XTEST 进实测到的终端）；⑪[中]数字键无修饰键（非标准布局会错）——键前校验 `keysym_to_keycode!=0` 否则 return 2 干净失败，US 布局假设记档；⑫[低]写入非原子——去掉 SELECT-then-INSERT，统一 `INSERT OR REPLACE`；⑬[低]测试桩没压第二次授权检查——桩改为 `delivered if still_authorized()`，加 Cell 计数闭包测试（check#1 true、check#2 false → Unreachable 零键、两次都跑）。闸门 fmt/clippy 退0、python 编译、全量 442。 **Sol 三审收紧（2026-09-17）**：#10/#11/#13 确认修好；余下同形态（注入安全+窗口绑定）连三轮触发「问用户」护栏，**用户定：再加固一轮+残余记档后真机 e2e**。⑭[高]序列中途焦点/授权漂移——`do_type` 改为**每个键前**都重核 `active()==target`，回车前尤其：焦点不对绝不发回车（数字留在选择框未提交、无害）；**不加键盘 grab**（主动 `XGrabKeyboard` 会把含 XTEST 合成在内的按键交给 grab 方客户端而非终端，等于自己吞键——推理否决，未上真机）；⑮[高]刷新会污染正确绑定——`remember_session_window` 对**手机驱动的提问不捕获**：daemon 注入手机回复时 `record_live_injection` 记 `injected_at`，钩子随后触发，故 `recent_live_injection_readonly(…, 15s)` 为真即跳过（全局按键新鲜度证明不了活动窗口是本会话）；⑯[中]`started` 置位太晚——改在**首个 KeyPress 交给 server 后立即**置位（sync/release 之前），其后任何异常都 return 0；⑰[中]送键成功→`mark_question_delivered` 失败的缝——不再 `?` 传播（否则按钮复活、二次点按重复打字），记日志并仍报不可重试；⑱[低]「最新捕获胜出」改由 SQL 保证——单调 UPSERT `ON CONFLICT DO UPDATE … WHERE excluded.captured_at >= captured_at`，迟到的旧捕获不能覆盖新的。新测：`the_read_only_capture_probes_see_stored_windows_and_recent_injections`、`an_unverified_session_with_a_remembered_window_still_holds`（回退成 `!windowless` 真变红）、单调 upsert 断言。**记档的残余（X11 层固有，单机单用户小概率误投递，非攻击面）**：(i) 焦点核对与按键之间的毫秒级缝；(ii) PC 在手机点按的聚焦等待内抢答→数字+回车打进已折叠选择框、成为一行会话会处理的杂散输入（PostToolUse 记账不受影响）；(iii) 本机提交后立刻切到另一个终端被采样到→记错窗口，下次本机提交自愈；(iv) 多 tab、单 DISPLAY、US 布局（后两者是部署范围限制而非固有，本机 `:1`/US 成立）。闸门 fmt/clippy 退0、python 编译、全量 3×444。 **Sol 四审收紧（2026-09-17）**：上轮修复多数确认（首键即送达、keycode 预校验、Unverified 守卫、单调 upsert、不加 grab 的判断）；余 2 高 1 中皆代码可收紧、非 X11 固有，用户定「修这三条再交」。⑲[高]打字阶段没再查授权——助手拆成三个小动作 `focus`/`digits`/`enter`，**按键动作绝不等待、绝不重聚焦**（窗口此刻不是活动窗口即 exit 4、零键）；Rust 用 `XtestGates` 逐步围栏：`still_open`→focus→`still_open`→`claim`→digits→一拍→`still_ours`→Return；一拍之内 PC 抢答则**回车扣下不发**、仍报 Delivered。⑳[高]「手机提交不捕获」无先后保证——弃用事后才写的 `live_injections`，改 `mark_phone_prompt`：daemon 在把手机消息**写入会话 socket 之前**落标记（`settings` 键 `phone_prompt_at:<thread>`），钩子不可能先于标记运行；只读探测**读不出也按抑制**（未来戳同）；仅「读得到且无/过期」才允许捕获。㉑[中]送键成功但 stamping 失败→`/threads` 重推可再打——改**打字前原子认领** `claim_question_delivery`（`UPDATE … WHERE delivered_at IS NULL AND answer IS NULL`，changed==1 才算赢；两次点按只有一个赢、写失败=没认领=零键），零键失败才 `release_question_delivery`。新测：`xtest_inject_is_fenced_by_the_row_at_every_step`（逐步序列断言：整序、预撤权零步、聚焦中撤权仅 focus、失认领零键、一拍内抢答扣回车、digits 失败释放认领、enter 失败仍 Delivered）、`a_delivery_claim_is_won_once_and_released_only_while_unanswered`、探针测试改 phone-prompt 语义。 **Sol 五审（2026-09-17）**：注入顺序、认领/释放配对、逐键焦点核对、fail-safe 抑制全部确认；余 1 高 1 低。㉒[高]15 秒时间窗覆盖不了会话排队延迟——改**因果判定**：手机注入的消息自带 `telegram：` 前缀，钩子直接看本次 payload 的 `prompt`（`prompt_is_phone_driven`），排多久都认得；时间窗标记留作第二道。㉓[低]主说明「只捕获一次」与实现矛盾——改准。 **真机 bug（2026-10-08，用户亲点，v0.2.18 修）**：手机连点三次（A、B、A）全部「没能把答案送进电脑上的对话框」、DB 行 `outcome=Unknown`，PC 键盘随后作答正常记账。根因＝`xtest_answer.py focus` 发 `_NET_ACTIVE_WINDOW` 时 `l[1]=CurrentTime(0)`：**Mutter 只为带真时间戳的激活请求切换工作区**，时间戳为 0 时跨工作区的窗口不聚焦、只被标 `_NET_WM_STATE_DEMANDS_ATTENTION`（真机：会话终端在工作区 1、用户在工作区 2；三次失败后该窗确有 DEMANDS_ATTENTION），助手等 1.5s 退 4 → `Unreachable`。同工作区时 0 可用，故 09-17 e2e 与 09-28 那次真机都过了。与 Mutter `meta_window_activate_full` 的 `allow_workspace_switch = (timestamp != 0)` 一致。修：聚焦前在私有未映射窗口上改一次属性、`sync()` 一次往返后从队列读出 PropertyNotify 的**真服务器时间戳**填 `l[1]`（取不到退回 CurrentTime，同工作区仍可用）；等待从 1.5s 放宽到 3s、改用单调时钟（工作区切换有动画，实测高负载下焦点 >2s 才落地）。总耗时＝一次 X 往返＋最多 3s 轮询。真机验证：同一跨工作区目标，CurrentTime 3s 不聚焦且被标 DEMANDS_ATTENTION；真时间戳 0.05s 聚焦、工作区随之切换。`focus_attach.py` 不改——它聚焦的是刚弹在当前工作区的新窗，CurrentTime 反而绕开 last_user_time 检查、更稳。**副作用记档**：手机作答会把桌面切到该会话所在工作区（XTEST 只能打进焦点窗，无可避免）。**残余（Astra 复审记档）**：Rust 在启动聚焦步之前查一次「题仍开放」，若 PC 恰在这次检查与聚焦请求之间（约一次 python 启动的时长）抢答，聚焦请求已发出，桌面仍会跳到该会话的工作区，之后的「题仍开放」复查只能拦住按键、撤不回这次跳转。与既有残余 (ii) 同类，属单机单用户小概率。 **真机 bug（2026-10-09，v0.2.19 修）——手机代按打进了别的会话**：麻将会话（窗口 65053492、pts/1）提问，用户手机点「发车」，「1⏎」却打进了揭棋窗口里的 **Codex CLI**（窗口 65138240，Codex 收到一条用户消息「1」并当成「先推进第一项」），麻将选择框未被触及、PC 回车才继续。根因在 0.2.17 的「记住窗口」：08:34:17 一条 `<task-notification>`（Monitor 到期）经 UserPromptSubmit 进入麻将会话，此刻用户正在 Codex 窗口打字，键盘新鲜度是**全局**信号、提示也不带 `telegram：` 前缀，于是把**当时的活动窗口（Codex）**记成了麻将会话的窗口；0.2.18 修好跨工作区聚焦后，这个错绑定第一次真的打出了键。**修法（用户定「打字前核实窗口」+「旧记窗口只当开关」）**：①记住的窗口降级为**开关**（该会话跑在 X 终端里、可释放），**绝不再当打字目标**；②提问时 gate 在题行上记下提问会话的 claude pid（`CLAUDE_CODE_MESSAGING_SOCKET` 文件名）、其 starttime ticks 与 pts（`/proc/<pid>/fd/0..2`），新列 `inject_pid`/`inject_pid_start`/`inject_tty`；记不到 pts 就不释放（走 held，手机直填）；③点按时 daemon 先核 pid+starttime+pts 未变（会话退出后 pts 可能已归别的终端），再经**该会话自己的 pts** 写 `CSI 22;2 t`（标题入栈）+ `OSC 2;<一次性随机记号>`（`O_NONBLOCK|O_NOCTTY`、300ms 预算），助手 `find <记号>` 找**唯一**标题等于记号的终端窗口（≤1.5s；0 个或多个都失败）——只有该会话的窗口、且该会话的 tab 在前台时才可能显示它；随后 focus/digits/enter 每一步、**每个键前**都要求该窗口仍显示记号（键还要求是活动窗口），任何一步不符零键（exit 6）；无论成败，入栈成功后 Drop 守卫都写 `CSI 23;2 t` 弹回原标题；④捕获卫生：以 `<字母` 开头的提示（Claude Code 自己的后台事件）不再触发记窗口；⑤0.2.19 之前建的已释放行（有窗口无 pts）点按一律「请在电脑窗口作答」、不打字；⑥XTEST 路径补日志面包屑（不含选项）。**真机验证**：VTE 约 50ms 应用 OSC 标题、入栈/出栈恢复原标题；AskUserQuestion 选择框显示期间 Claude Code 不改标题（静态 `✳ …`），开始工作后才每秒转圈；`find` 0.12s 找到本窗、缺记号 1.6s 退 4、错记号 focus 退 6、用本会话记号对 Codex/麻将窗口按键均零键。顺带关掉 0.2.17 记档的残余 (iii)（记错窗口）与多 tab 固有限 (iv) 的打错 tab 一半——别的 tab 在前台时记号不显示，点按改为「请在电脑窗口作答」。**副作用**：手机作答的 1～4 秒内该窗口标题显示 `tinyctb-<16位十六进制>`，之后恢复。**与 0.2.17 探路结论的出入**：09-17 探路记「OSC 写本会话 pts 被 Claude TUI 锁死无效」；10-09 在本会话（pts/8）复测，标题 0.02～0.05s 即变成记号、空闲时保持 1.5s 不被改写、焦点移出窗口时也不变；当年失效的原因未能复现（可能是 Claude Code 版本差异或当时会话在工作、标题每秒转圈）。**焦点移回窗口时 TUI 会不会重设标题未能测到**（桌面正被用户使用，探针中止）；即便会，逐键核记号也使点按**零键失败**（提示到电脑作答），不会打错地方——由真机端到端确认。**Astra 四轮（2026-10-09，r4 通过）**：r1 两项阻断——①pid/pts 复用竞态：改为**只开一次 pts 并全程持有该描述符**，`SessionPts::still_session` 在前后两次 starttime 之间核「同一进程的 fd0..2 之一与我方描述符 `st_rdev` 相同」，聚焦后、认领前与回车前各复核一次，入栈/记号/出栈都走同一描述符（pts 号被回收给别的终端也写不到）；②助手子进程无界：`run_helper_bounded` 轮询 + 每步硬期限（find 4s、focus 6s、按键 3s）；另两项应修：入栈完成即挂出栈守卫、出栈失败记日志；测试可注入进程观测（pid 被回收、换 pts、另一 pid、标记后离开 pts）。r2 两项阻断——①按键结果分不清全打/半打：助手退出码 0 只表示**全部键按下并松开**，新增 **7＝可能已出键但序列未完成**（`started` 在 fake_input 之前置位、`finally` 松开按住的键），Rust `StepResult::Partial`；数字步 Partial/超时一律 **Delivered、保留认领、扣回车**，被杀的按键步之后再跑有界的 `release <digits>` 松键（防自动连发）；②超时后阻塞 `wait()`：`kill_and_reap` 杀后最多等 500ms，否则交后台线程收尸；应修：记号写一半（停在 OSC 串里）时出栈前先发 **CAN（0x18）**（真机验证 CAN 后解析器回到常态）、focus 等待中每次轮询与返回前都复核记号。r3 一项：入栈写一半也发 CAN。r4 通过（余一处文档措辞已改）。闸门 fmt/clippy 退0、python 编译、全量 450。
- **多题逐题推送、部分提交、多选勾选（0.2.20，2026-11-23）**：用户要求「同一目标的多道独立单选题，手机上拆成 n 条单选、可挨个或只答部分」，随后追加「停顿超一分钟没答完补提醒」与「多选题改成勾选框+提交按钮」。**交互会话的多题批次**（同 0.2.17 的开关：Window 会话＋记住终端窗口＋会话 pts）不再只推「请在终端里作答」：每个单选/多选页都建行（`mark_question_xtest_window`＋`set_question_batch`），手机点按**只暂存**（新列 `staged_answer`），因为看不到终端画面、不能逐页盲打；**全部暂存即自动提交**，或点批次下方的「📨 提交已答的」——一次过打完整串键：`←`×N 回到第一页（实测夹在首页不循环）、每页单选打一个数字（选中并自动跳页）、多选打每个勾选项的数字（切换勾选、不跳页）再 `→`、未答页 `→` 跳过（键盘已答的页原样保留）、最后一个 `→` 夹在 Submit 页，**复查行后**再回车（`fork_dialog::inject_batch_via_xtest`，助手新步骤 `keys`，每键前核焦点与标题记号，键间 0.25s）。打字前**整批原子认领**（`claim_batch_delivery` 一条语句把全部行置 `delivered_at`+`submitted_at`；零键失败才释放）。**实测（2026-11-23，关掉 hook 的一次性会话驱动真对话框）**：只答部分时 Submit 页显示「⚠ Some questions are unanswered」、默认选中「1. Submit answers」，回车只交已答的题，工具结果里只有它们；按 0.3s/键实放整串（从第 3 页起步）结果为 color=Red、pet=Dog，fruit 留空；多选页数字切换 `[✔]`、`→` 跳页，单独一道多选题也带「Submit」页。**后台 fork**：每页照旧点按即注入；新增「提交已答的」走 `inject_submit_partial`（读到正好 N 页、至少一页已答、高亮在 Submit 才回车）；多选页走 `inject_checked_tab`（导航到该页、要求仍是 `☐`、逐个打切换数字）。**多选题**：手机按钮是勾选框（`⬜ A 奶酪`/`✅ A 奶酪`，一行一个，路由动作 `toggle_option` 永不消费），点一下改 `checked_json` 并 `editMessageReplyMarkup` 原地重绘；最下面「📨 提交这题」（`submit_checked`）：held 题按原生格式 `a, b` 记答案交给阻塞钩子，交互批次页暂存，fork 批次页注入；回复字母 `A,C` 仍可用。**未答完提醒**：每次手机作答记 `tapped_at`；daemon 每个同步周期 `enqueue_batch_reminders`——已答过至少一题、最后一次作答满 60s、还有可由手机作答的页没答、未提交、未过期、且这次停顿还没提醒过（`reminded_at < MAX(tapped_at)`）——推一条「⏰ 这组 N 道题还没答完：已答 k 题，还差第 x 问…」并附自己的「提交已答的」按钮；再作答即重新计时。提交消息与提醒都不带 `questionId`、不建回复路由（回复它不算答案）。**记档的限**：①交互批次多选页的切换是相对的——若 PC 键盘已在该页勾过，再按手机勾选会被翻转（看不到屏幕，按「未动过」处理；fork 路径读得到页签，要求 `☐` 才打）；②某页若被 PC 停在「Type something」文本框里，`←`/`→` 可能被输入框吃掉；③与 0.2.17/0.2.19 相同的 X11 残余（焦点核对与按键之间的毫秒缝、单 DISPLAY、US 布局）。 **Astra 一审（不通过，5 条）修法**：①[高]「提交这题」/「提交已答的」按钮永不消费（`keepRoute`），每次点按按行/批次状态判定——之前交互批次首次提交后改勾选会清掉暂存、按钮却已失效；②[高]`keys` 步骤期限按键数算（3s＋0.45s/键，4 页×8 勾≈41 键≈21.5s），打字被截断/超时/回车被拒一律回 `SubmitPending`，手机提示「没能完整打进并提交，请到电脑上检查」，不再谎报已提交（认领保留、绝不重打）；③[高]打字过程中 PC 键盘插手：助手 `keys` 用 XInput2 监听根窗口 raw key press，来源设备不是 XTEST（「Virtual core XTEST keyboard」）即视为有人在按键盘，下一键前停（未出键 exit 4、已出键 exit 7），末键后再看一次；批次回车改走 `enter-watched`（同样监听）；回车前若行已被结算（PC 已提交）直接 Delivered 不按（真机验证：自身 XTEST 键不触发围栏；实体键触发因用户不在机前未能实测）；④[中]勾选 toast 写出**完整**当前勾选集（键盘重绘失败时 toast 仍是真相），重绘失败重试一次；⑤[中]多选回复逐个 token 解析（字母大小写、标签、混用）。 **Astra 二审（不通过；r1 的 #1/#2/#4/#5 确认修好，#3 未修完＋3 条新）修法**：⑥[高]键盘围栏有空窗——改成**单进程** `xtest_answer.py batch`：先订阅 XInput2 raw key press（订阅后**不清空**队列——此后排队的都是真按键），验窗口与记号后输出 `ready`；Rust 收到 `ready` 才复查行与 pts 并**认领**，回 `go`；助手每键前、末键后查实体键，打完输出 `typed`；Rust 复查行（已被结算→不按、Delivered）后回 `enter`；助手按回车前再查一次。监听从认领前连续覆盖到回车后；无 XInput2 时拒绝（不盲打）。退出码：0 已回车；4/6/3/1 未出键；10 Rust 中止（未出键）；7 可能部分出键；11 打完我方中止回车；12 打完回车被拒。Rust 侧逐步有期限（ready 4s、typed 2s＋0.45s/键、退出 3s），超时杀进程。旧的 `keys`/`enter-watched` 两步删除。协议用假 X 单测六条路径（全程、go 前中止、回车前中止、首键前/中途/回车前有人按键）全对；⑦[高]fork 多选切换数字写到一半失败——新结果 `InjectOutcome::PartlyTyped`：行标 delivered（绝不重打）、整组 `submitted_at`（手机不再提交、不再提醒），提示「可能只勾上了部分选项，请到电脑核对并提交整组」；⑧[中]fork 部分提交后未答页旧按钮仍判 Open——`inject_native_attach_answer`、`submit_checked` 入口和 fork 注入授权闭包都先查 `batch_submitted`，统一回「这组题已经提交（或已交给电脑处理）」；⑨[中]提醒资格检查与入队不在同一事务——`enqueue_batch_reminders` 整体包进 `BEGIN IMMEDIATE`，在锁内查 due、建路由、入队、写 `reminded_at`。 **Astra 三审（不通过；键盘围栏方案明确认可、不再阻断）修法**：⑩[高]fork 多选只在首个切换前检查——`inject_tab_digits` 在每个后续切换数字前重新 `still_authorized()` 并重读页签（总页数、`current_index == seq`），任一不符即停、回 `PartlyTyped`（逐项核对勾选框画面未做：起点要求该页 `☐`＝全未勾，每个切换都落在已核实的本页）；⑪[高]fork 多选缺持久认领——`claim_checked_tab` 在**首个切换前**一个事务里把本行 `delivered_at` 与整组 `submitted_at` 都写成同一 stamp（交给电脑），崩溃或写库失败时一律保守为已交接；注入期间授权改为 `checked_claim_live`（本行 `delivered_at == stamp` 且未作答）；全部切换完成 `finish_checked_tab` 只撤自己 stamp 的交接标记，零键失败 `release_checked_tab` 整体撤销，`PartlyTyped` 保持交接；⑫[中]`submit_checked` 对批次行**先**查 `batch_submitted`，再看勾选集与行状态。 **Sol 四审（改用 gpt-6.2-sol/medium，附用户审核要求模板；不通过，3 阻塞 1 应改）修法**：⑬[阻塞]交互批次含只能在电脑答的页（无行）时，手机页全部暂存就自动提交——改为只有**每一页都有行**才自动提交，否则提示「手机能答的都答完了；第 x 问只能在电脑上答——电脑答完再提交，或点「提交已答的」先交」；⑭[阻塞]多选勾选拼成标签串再解析回索引会错配（选项 `A`/`B`/`A, B`）——「提交这题」把勾选**索引**直接传给暂存（`stage_interactive_batch_answer` 新参数 `ticked`），只有文本回复才解析；⑮[阻塞]fork 多选两次切换之间 PC 同页改勾选——每个切换后强制重绘读屏，按真机重绘字节（`N.` 后 `[✔]`＝勾、`[`+光标跳+`]`＝空，Claude Code 2.1.330 实测）解析勾选集 `checked_boxes`，必须**恰好等于**已打的集合，否则立即停、`PartlyTyped`；首个切换前也要求屏上全空（读不到即不打）；⑯[应改]`finish_checked_tab` 两条更新并入一个事务。新测试桩 `staged_attach_stub` 按收到的字节数切换画面。 **Sol 五审（不通过，3 阻塞 1 应改）修法**：⑰[阻塞]交互会话多选页是相对切换、读不到屏，电脑先勾过 A 再手机交 A、B 会勾反——批次含多选勾选时，只有**出题以来电脑键盘可证明没动过**才打（`approvals::local_input_since`，读 daemon 的 `input-activity.json`，因其每秒最多写一次故起点减 1s；读不到记录按「动过」），在暂存触发时与认领前（助手已在监听）各查一次，否则整组不打、提示「出题后电脑键盘有过操作…这组请在电脑上完成」；⑱[阻塞]fork 多选首键前「查页→再单独重绘读勾选」之间可切页——改为**一次**重绘同时读页签与勾选（`read_tab_screen`），读完立即打首键；每个切换后也同一次读里同时核页位置与勾选集；⑲[阻塞]勾选解析会被标签/描述里的字面 `2.[]` 覆盖——改为在原始字节上按真机形态匹配：`N.` 后必须至少一个 CSI 再 `[`，空框必须是括号间的 CSI，纯文本无法冒充；⑳[应改]含电脑题的批次在手机题答完后永不提醒——提醒新增 `pc_only`（无行的页），手机题全答完但有电脑题时也提醒并点名。用户在「同类问题连三轮」护栏下选择继续送审（2026-11-23）。 **Sol 六审（不通过）＋用户定「电脑优先抢答」**：六审再卡多选（监听停了留下旧记录／订阅前按键未落盘可能误放行；fork 末题最后核对后到回车之间 PC 取消一项仍会提交），护栏下问用户，用户定：「要求是2边皆可回答…放一个抢答标志，电脑优先。当电脑勾选过，该标志就显示电脑已抢答，手机端无效。此时手机端任何答复都无效。」落地：㉑需要替手机「切换」勾选的多选（交互批次、fork 批次）在打之前与打的全程都要证明电脑键盘没动：daemon 键盘监听新增**心跳** `input-listener.json`（每 2s，含 `aliveAtMs`/`upSinceMs`），`local_input_since` 只有在心跳新鲜（≤6s）且监听自出题前就在线时才给结论，否则「未知」；交互批次在暂存触发时与认领前（助手已订阅、再等 300ms 让订阅前的按键落盘）各查一次；fork 多选新增助手 `watch` 模式（XInput2 常驻围栏，`check`→`clean/pressed`，粘滞），认领前起、等 300ms 后查记录＋围栏，注入期间每次授权（首键前、每个切换之间、末题提交回车前）都问围栏——PC 在最后核对到回车之间任何按键都会拦下回车。任一处判定「动过或无法证明没动」→ 整组 `pc_won_at`（同时交接 `submitted_at`），推一条「🖥️ 电脑已抢答」通知，此后该组手机任何点按/文本回复一律回「电脑已抢答这组题，手机上的答复无效」。单选不受此规则（直接选中、非相对切换）。㉒[阻塞]打完字后数据库复查报错被当成「电脑已提交」——`on_typed` 失败一律扣回车回 SubmitPending，调用方只有在行上**确认**有答案时才说「电脑上已经提交」；㉓[应改]交互批次单选按钮点一次被消费、改不回——交互批次题的答案按钮 `keepRoute`（每次点按按行状态判定）。 **Sol 七审（不通过，5 阻塞 1 应改）＋用户定「电脑优先扩大到所有题」**：七审指出单选在电脑先进「Type something」等情形下同样会盲打错，以及若干围栏缺口；护栏下问用户，用户选「扩大到所有题」：多题批次推送后电脑键盘有任何按键（或无法证明没有），整组判电脑已抢答，手机对该组的作答、勾选、提交全部无效（交互与 fork、单选与多选一律）。落地：㉔`pc_priority_check` 在**每个**批次相关回调最前（以及文本回复入口）检查——已抢答或出题以来键盘动过/无法证明→声明抢答、回「电脑已抢答」，该回复 `keepRoute`（同一按钮再点仍是这句，七审应改）；㉕交互批次每次打字前都查（不再只限含多选），认领前在助手围栏起后等 300ms 再查；助手看到实体键改用专用退出码 13（首键前）/14（中途或末键后）/15（回车前），Rust `BatchPass::PcKey` → 回调 `on_pc_key` → 声明抢答并持久化（七审 #1）；㉖fork 所有注入路径（单选页 `inject_option`、多选页、`提交已答的` 的 `inject_submit_partial`、提交恢复 `inject_submit_only`）统一走 `pc_fenced`：先起 `KeyWatch` 围栏、等 300ms、查键盘记录，注入期间授权都并上 `!watch.pressed()`；`inject_tab_digits` 在读屏**之后**、首个数字**之前**再授权一次（七审 #2：读屏的重绘等待内按下的键也会被拦）；fork 部分提交前同样经 `pc_priority_check`（七审 #3：勾完后电脑改动再从手机提交→判电脑抢答）；㉗`input-activity.json` 只有「不存在」才视为没按过键，其他读错误一律「未知」（七审 #4）。 **Sol 八审（不通过，4 阻塞 2 应改；同类第三轮，问用户后定「修 5 条、空档记档」）修法**：㉘监听心跳改为**真实在线**：每 1s 一跳，只在读键线程正在等读或刚处理完（≤1s）时写，流一断开就在同一把锁下**删掉**心跳文件（飞行中的心跳不会再写回），新鲜度改 3s；`upSinceMs` 断线重连即重置；㉙fork 注入结束后**主动**再问一次围栏（不是读缓存）——最后一次重绘期间按的键也算电脑抢答；㉚交互助手起不来 XInput2 围栏（`ready` 前 exit 1）→ `BatchPass::NoFence` → 判电脑抢答（不再只回「可重试」）；㉛提醒前先查键盘：出题后动过 → 声明电脑抢答（附通知），不再推「还没答完」；`pc_priority_check` 跳过已结算/已由手机提交的批次（不会事后误判抢答）；㉜批次题的所有答案按钮 `keepRoute`（之后被抢答仍能提示）；抢答标记与通知在同一个 SAVEPOINT 里落库（可嵌在提醒的 IMMEDIATE 事务内），通知 eventKey 按批次去重。**已知残余（用户 2026-11-23 认可记档）**：围栏查询与真正发键之间仍有一次 X 往返（交互：助手 `person()` 后到 `fake_input`；fork：`clear()` 后到 pty 写）的毫秒级空档，X11 没有原子的「查完即打」；已把实体键检查挪到每次发键前的最后一步，把空档压到最小。 **Sol 九审（不通过，4 阻塞 1 应改；问用户后定「修完再送」）修法**：㉝心跳增加 `idle`（读键线程正在等读时所写），每 250ms 一跳、新鲜度 2s；新的 `keyboard_untouched_behind_fence`：围栏起来后，等到一条**围栏之后、读键线程空闲时**写的心跳（≤3s）——此时 X 在围栏前交付给监听的按键都已读完落盘——再读键盘记录（交互认领前、fork `pc_fenced` 与 `submit_checked` 均改用）；㉞fork 走向 Submit 页与导航到目标页的**每个方向键**前都先授权（含围栏），不再只在回车前；㉟批次回车一旦写出（`ENTER_SENT`/`take_enter_sent`），手机答案即已提交，之后的电脑按键不再判抢答；㊱交互助手 `ready` 前超时 → `NoFence`；`go` 之后除我方中止（11）与零键拒绝外的一切结束（超时、错误、焦点/记号丢失、回车被拒）一律 `PcKey{typed}` → 判电脑抢答（不再落入「可重试」或只留 `submitted_at`）；㊲daemon 每个同步周期对**所有**等待中的批次（含手机一题没碰的）查键盘，动过/无法证明 → 声明抢答并推通知（用户只在电脑上作答的常见流程也能收到「电脑已抢答」）。**用户 2026-11-24 补充送审范围**：「让sol不要审黑客类安全类bug，这不是写操作系统」——之后送审只审正常使用中真实发生的逻辑错误。 **Sol 十审（加了范围说明后；1 阻塞 2 应改，全为真实使用场景）修法**：㊳fork 由手机触发自动提交（最后一页答完、Enter 写出）后，批次立即标为手机已提交（`mark_batch_submitted`），之后的巡检与按键不再判电脑抢答；注入期间批次带 `phone_typing_at` 标记，巡检与 PostToolUse 都不把它算作等待中；㊴PostToolUse 记账时，若该会话有仍在等手机的批次（未提交、未在手机打字中、未抢答），即是电脑答完了——当场声明电脑抢答并推通知（不依赖巡检刚好看到键盘）；㊵交互批次有只能在电脑答的页时，提示改为说清楚：手机暂存的答案还没打进电脑，电脑上一动键盘就改由电脑作答、手机答案不会进去；要保留手机答案请点「提交已答的」，或在电脑上整组重答。 **Sol 十一审（1 阻塞 2 应改）修法**：㊶键盘记录改为**精确时间**——节流（每秒最多写一次）期间到来的按键不再丢弃，而是记为待写，由下一次心跳（≤250ms）补写其真实时间；有待写按键时心跳不标 `idle`。于是所有「出题以来键盘动过没有」的比较都去掉了原来的 1 秒余量（之前用户在电脑上回车发出提示、Claude 一秒内出题，那下回车会被当成电脑抢答，整组手机按钮作废）；㊷daemon 启动时（持单实例锁、不可能有注入在跑）清掉所有残留的 `phone_typing_at`——fork 注入中途重启 daemon 留下的标记不会再让巡检与 PostToolUse 永远跳过该批次；㊸交互批次唯一暂存的是多选页、之后改勾选被撤暂存时，「已答」变 0，不再推「已答 0 题」的提醒（及没有可提交内容的按钮），下次再暂存即重新计时。 **Sol 十二审（1 阻塞 2 应改）修法**：㊹（曾改为从推送到手机时起算，十三审指出推送前电脑在对话框里的勾选会被手机盲打翻转、且推送时间本身不准；用户 2026-11-24 定**改回出题时刻**：Claude 一出题电脑终端即弹出对话框，此后电脑上的按键都可能落进对话框，故从出题起算；配合 ㊶ 的精确计时，出题前发提示的那下回车不算）；㊺过期批次不判电脑抢答——点旧按钮直接回「手机答复窗口已关闭」，不会写 `pc_won_at` 也不补推通知；㊻多选题用文字回复（`A,C`）改了勾选后，原消息的勾选框随之重绘（`redraw_question_checkboxes`），屏上 ⬜/✅ 与下一次点按的切换方向一致。 **Sol 十三审（3 阻塞 1 应改；同类第三轮，问用户后定「改回出题时刻」）**：两条阻塞（推送时间取值、只能电脑答的首条推送不记时间）随推送起点一并撤销；推送前电脑勾过、手机再盲打会翻转的阻塞也因起点改回出题时刻而消失（出题后任何按键即电脑抢答）；㊼PostToolUse 补发「电脑已抢答」只针对未过期的批次（`waiting_batches_for_thread` 加期限条件），手机窗口已关闭的批次不写 `pc_won_at`、不发通知。 **Sol 十四审（1 阻塞 1 应改）修法**：㊽多选题文字回复里的**单个字母一律按按钮字母**解析（勾选框显示 `⬜ A <标签>`、提示也让回字母），即便另一个选项的标签恰好是 `A`；不是字母（或超出选项数）才按标签匹配；㊾到期写入的 `QUESTION_EXPIRED` 标记不算「已作答」，过期批次点旧的「提交已答的」回「手机答复窗口已关闭」，不再误报「这组题已经提交过了」（打字中断后的「电脑已提交」判定同样不会被过期标记误触发）。 **Sol 十五审（1 阻塞 1 应改）修法**：㊿多选文字回复若**全部由有效按钮字母组成**（`A`、`a, c`），一律按字母解析，优先于整串标签匹配（标签为 `A`、`B`、`A, B` 时回 `A, B` 勾前两项）；「提交这题」先查题目是否已过期/已作答，再提示「还没勾选」——过期题不会再被引导去点选项。 **Sol 十六审（2 阻塞）修法**：(51) fork 读屏找标签栏起点改为「`←` 后（跳过空格与控制序列）紧跟页签标记 `☐`/`☒`」的最后一处（`last_bar_arrow`），选项文字或描述里的 `←`（如「← 返回上一级」）不再被当成标签栏，勾选框解析与页签解析都用它；(52) 两个同名选项（描述不同）时，勾选按钮按它在本条消息里的**位置**确定是第几项（重绘也按位置对应），点第二个按钮切换的是第二项。 **Sol 十七审（1 阻塞 1 应改）**：(53) 手机提交跨过截止时间——聚焦、重绘、逐键输入可能耗时数秒，注入过程中各「是否仍归我们」复查（交互批次的认领前/回车前、fork 各注入路径的授权、fork 多选的认领存活）改用随墙钟前进的 `live_clock(now)`，截止时间在注入途中到达即扣下回车；(54) 单选同名选项（描述不同）：Claude 收到的答案只有标签文本（原生 `answers` 是 问题→标签 的映射），两项提交给 Claude 的内容完全相同，无可观察差别，未改。 **Sol 十八审：通过**（同名单选一条撤回），附 2 条应改已修：fork 多选「提交这题」的 `live_clock` 在函数入口即起算（围栏等待与认领也计入截止判断）；过期的 fork 多选题收到文字回复时先判过期，不再改写勾选、不重绘。
- **多选题改用 Telegram 投票、提交后原消息变状态、点按更快（0.2.21，2026-11-24）**：v0.2.20 上线后真机试用，用户反馈两点：①多选勾选框每点一下要等一会才能点下一个；②点「提交这题」看不出反应（后台其实收到了，提示只是顶部一闪而过的小字，原消息不变）。用户要求「多选题选择按钮本地变状态，然后点提交按钮统一提交」——Telegram 的内联按钮做不到本地状态（每次点按都要经服务器到 bot 再重画），用户选定改用**原生多选投票**：①多选题（2–10 个选项）在题目文字消息下面紧跟一条 `sendPoll`（`is_anonymous=false`、`allows_multiple_answers=true`，选项带字母前缀 `A 奶酪`，题干≤300 字、选项≤100 字截断——投票按**序号**回来，截断不改含义），勾选在手机本地即时变化，点「投票」一次送达；`getUpdates` 订阅 `poll_answer`，按 `poll_id` 找回题目（新列 `poll_id`/`poll_message_id`），投票等同「提交这题」（交互批次暂存/全部答完一次打完、fork 注入、held 记答案），先过电脑优先检查；**撤回投票**只对仍只是暂存的交互批次页有效（撤暂存），其余回「已提交，撤回不改变已交的答案」；投票没有 toast，结果以一条短消息回复；投票消息也登记为题目消息（回复它 `A,C` 同样作答）；`sendPoll` 失败不让整条投递失败（文字已发出，仍可回复字母），选项数不在 2–10 时退回 v0.2.20 勾选框。②**题目结束即关闭手机消息**：daemon 每次唤醒扫已结算（已作答、电脑抢答、过期）且未关闭的题（新列 `finalized_at`，只看 24 小时内的题），把它们的按钮键盘收成一行结局（`✅ 已作答：…`／`🖥️ 电脑已抢答`／`🖥️ 已在电脑上作答`／`⌛ 手机答复窗口已关闭`，按钮仍可点、回状态提示），投票 `stopPoll`；单选题点中后该消息上被选项的圆点换成 ✅（`chosen_option_keyboard`）。③**点按更快**：回调先 `answerCallbackQuery`（结束按钮转圈）再重绘键盘；手机最近 60 秒有动作时 daemon 唤醒间隔降到 Telegram 通道下限 250ms（原 500ms）。 **Sol 一审（2 阻塞 3 应改）修法**：①撤回投票只撤投票自己写入的勾选（新列 `poll_vote_json` 记最后一次投票的勾选集）：投票后又用文字回复改过答案，撤回旧投票时回「后来用文字回复改过答案，撤回不影响它」；②`sendPoll` 不再重试（超时可能已发出，重发会让第一条投票对不上题），对不上题的投票回一条「没能对应到题目，请回复字母作答」，不再静默忽略；③单选 ✅ 重绘只在题目仍在手机上进行时做（未关闭、未被电脑抢答、行仍 Open/Delivered），点已收起的结局按钮不会把选项恢复出来；④关闭消息遇到临时失败（网络、429、5xx）不记 `finalized_at`，记 `finalize_tried_at` 并 30 秒后重试，只有 400/403（消息没了、未改变、投票已关）才算完成；⑤同名单选项按**被点按钮的位置**打 ✅。 **Sol 二审（1 阻塞 2 应改）修法**：⑥任何写勾选的路径（`set_checked_options`，文字回复、勾选框、暂存）都会清掉投票记录，投票路径在提交后、且当前勾选仍等于投票集时才重新记——文字回复即使改回与投票相同的集合，撤回旧投票也不会清掉它；⑦关闭消息的巡检每次唤醒限时（单次调用 2s、整体 2s 预算，超出的留到下次），网络不稳时不拖住新消息收发；⑧投票提示改为「整组还没送进电脑前可以撤回投票重选；已提交的撤回无效」。 **Sol 三审（2 阻塞 1 应改）修法**：⑨批次的「提交已答的」和提醒消息虽挂在锚点题上，但属于整组：单题关闭时不动它们，等**整组每一行**都结算后（最后一道结算的题关闭时）才收成「✅ 这组题已结束」/「🖥️ 电脑已抢答」；⑩交互批次单选题用文字回复改了暂存答案时，原消息的 ✅ 也随之移到新选项（文字回复路径同样重绘）；⑪关闭巡检的每次 API 调用都只拿到本次唤醒剩余预算（≤2s），预算用完立即停，当前题与其余题留到下次。 **Sol 四审（2 阻塞 1 应改）修法**：⑫关闭进度按**每条消息**记（新表 `closed_question_messages`），预算用完下次从未关闭的那条继续，不再从头来；遇到临时失败整个巡检暂停 30 秒（`question_finalize_paused_until`），断网时不每次唤醒都空耗预算；⑬批次的「提交已答的」/提醒消息改为独立扫描（`settled_batch_messages`：整组每行都结算、消息未关闭即收起），不再依赖某一行自己有消息——最后一题靠文字回复答完（投票没发出去）也能收起；⑭fork 多选投票若一个键都没送进去（行仍 Open），撤回时回「之前的投票没能送进电脑，答案没有提交，重新投票即可」。 **Sol 五审（1 阻塞 1 应改）修法**：⑮同一题前一条消息临时失败、后一条因预算用完没做时，同样记失败并暂停 30 秒（不再跳过退避）；⑯已交给电脑但会话还没记下答案时（含 fork 多选只勾上部分），撤回回「这组题已经交给电脑上的对话框，撤回投票改不了它；如果电脑上还没提交，请到电脑上核对后提交」，只有答案已记下才说「已提交」。 **Sol 六审：通过**，附 2 条应改已修：fork 批次某页勾选已送进对话框、整组仍在手机上时，撤回回「这题的勾选已经送进电脑上的对话框，撤回改不了它；其余题照常作答，或点「提交已答的」」；收起键盘时结局按钮优先挂在未用过的路由上，点它得到状态提示而非「已经处理过了」。
- **本地**：窗口里原生答（打数字或方向键皆可）。**弹窗焦点（v0.2.13，2026-09-04）**：后台 daemon map 出的窗口被 Mutter 焦点偷取防护拒焦（用户报「框闪一下没焦点」）。故 `pop_attach_window` 经内嵌的 python-xlib 助手 `src/focus_attach.py`（`include_str!` 随二进制走、用时落 `~/.cache/tinyctb/focus_attach.py`）弹窗：助手先快照 `_NET_CLIENT_LIST`、再弹 `gnome-terminal`、diff 出新窗口（gnome-terminal 是 client-server，标题/`_NET_WM_PID` 都不可靠，diff 与终端无关且可靠）、对它发 `_NET_ACTIVE_WINDOW` **source=2（pager）**请求聚焦——EWMH 定义 2＝pager/直接用户动作，Mutter 通常据此把窗带到前台（含越过焦点偷取防护），但标准明允 WM 拒绝，故是最佳努力非硬保证（注：上游 `wmctrl -a` 实发 source=0 再 `XMapRaised`，此处 source=2 是等效的直连聚焦请求）。落盘用 temp+rename **原子发布**（防并发弹窗时另一进程读到半截脚本，四审）。助手**永远先弹窗**（X／python-xlib 缺失也照弹，焦点是最佳努力、绝不承重；terminal 首弹失败留 `_SPAWNED=False` 供 `__main__` 尾重试一次）；仅 `python3` 完全起不来时 Rust 回退直接弹窗。本机 `wmctrl`/`xdotool` 未装，python3-xlib（0.33）已装；2026-09-04 真机实证 source=2 拿焦点、且按 daemon 同一调用形状自测弹窗即前台。**自动关窗**由 `run_question_answered_gate` 钩子记账后调 `close_attach_windows`：匹配 `<claude> attach <short>` **三段** argv（严格三段——`/proc/cmdline` 只去尾部终止空项、保留内部空项），`argv[0]` 命中 **daemon 解析出的 claude 全路径**（原生/符号链接，`execve` 保 argv[0]）**或** **basename 恰为 `claude`**——涵盖 `exec /real/claude "$@"`（argv[0]=真 claude 绝对路径）与常见的 PATH 形式 `exec claude "$@"`（argv[0]=裸 `claude` 无斜杠）；要求整段 basename 等于 `claude` 故排除 `/tmp/notclaude`／裸 `notclaude`（非 `ends_with("claude")`——那会误匹配，四审）。SIGTERM 掉 attach 客户端,绝不碰 fork（fork 跑 bg-pty-host 长 argv）。**且仅关 daemon 自弹的窗（0.2.14 关键修）**：弹窗命令包成 `env TINYCTB_ATTACH_POPPED=1 <claude> attach <short>`（`env` 直接 exec claude,故 argv 不变仍匹配,但进程 environ 带该标记),`close_attach_windows` 再读 `/proc/<pid>/environ` 校验标记（`environ_has_marker` 精确逐条匹配非子串）——**用户自己开的 `claude attach <short>` 没标记、绝不被关**。这修的是 0.2.13 的真机回归:用户在终端窗口里**用键盘作答**后,auto-close 把他正用的那个窗也 SIGTERM 了（会话没死、但窗口被从脚下抽走）。environ 读不到就跳过（fail-safe:证不了是我们弹的就不杀）。附带也会命中**同 fork 自己的 `with_attach_pty` 无头注入客户端**——但那条走 `resolve_program` 不带标记,故 0.2.14 起也不再被误关。**已知限（Sol 四审记档，属并发/罕见，按"别钻牛角尖"记为 pty/WM 固有限）**：① 手机在 attach 客户端进程**尚未出生**的极短窗内答题，钩子那次 `/proc` 一次性扫描关不到它，随后弹的窗漏关（自动关是 0.2.13 新增能力、0.2.12 本就不关；人从看推送到点按通常 >1s、attach 早在，实际少见）；② 并发弹多窗时 helper 的新窗识别（sole-new-window／`max(XID)`）可能把 source=2 焦点发给无关窗；③ `/proc`→SIGTERM 间 PID 复用 TOCTOU、短 id 跨 fork 碰撞（1/2³²）理论上可误杀另一 client；④ **argv[0] 认领的固有边界（泛化陈述，故完备）**：自动关窗靠 `argv[0] == 解析出的 claude 路径` 或 `basename(argv[0]) == claude` 认领弹窗;**凡 wrapper 让进程运行时 argv[0] 既不等于解析路径、basename 也不是 `claude`,其弹窗就关不掉(漏关、不误关)**——含 `exec` 到改名/带版本号/`readlink -f` 解析出的真二进制(如 `claude.real`)、shebang 型不 `exec`(内核前插解释器→argv 非三段)、`exec -a <name>` 改写 argv0。常见形态(原生/符号链接、`exec /real/claude`、裸 `exec claude`)均已覆盖;上述皆非常规部署,五审记档为 pty 关窗固有边界。⑤ **标记门的两个边界(0.2.14 Sol 复审 MINOR 记档)**:(a) 会**清洗/重建环境**的 `CLAUDE_BIN` wrapper 会丢掉 `TINYCTB_ATTACH_POPPED` 标记→真正的 daemon 弹窗**漏关**(安全侧=漏关不误杀,与④同性质);(b) 用户 shell 若**主动导出**完全相同的 `TINYCTB_ATTACH_POPPED=1`,其手开 attach 会被误判为 daemon 弹窗而关掉——需用户刻意导出我们这个专有标记,非自然发生。
- **手机（先投递后记账，Sol P1-3；只认正证据，三审收敛为两态）**：daemon（`inject_native_attach_answer`）先 `pending_question_status` 查在开/已答/过期（含 deadline），仍开放；再**自己查 label→选项号**（区分"不是选项"＝坏答案不可重试，与投递失败＝可重试，三审 #6），再 `inject_option`：
  - `Injected`（回车前活框在、回车后**非空帧且框消失**，见上）→ 用 `record_native_answer` 记真答案、toast「已作答」。**该记账能覆盖钩子的 `\0elsewhere` 软结算**——若 fork 答完后 PostToolUse 在这空档先把行软结算了，手机的真选项仍写进去，审计/`/threads` 显示真答案而非 sentinel（四审 P2）。
  - `Unreachable`（回车前 chrome 不在／回车后框**还在或空帧**／attach 无帧／pty 错误——**pty 分不清"本地已答/还在连/报错"，一律不认作本地已答**，三审 #1）→ 不记不结算、行留开、toast「没能连上…未记录，可重试」、回 `retryable:true` 让按钮不被消费可再点。
- **本地已答由 PostToolUse 钩子权威结算（三审 #1/#5）**：装 `PostToolUse(AskUserQuestion)` 钩子 `question-answered-gate`，AskUserQuestion 一答完（本地或注入）就触发，把该 session **所有开放 native 行**关掉（`settle_open_native_questions_for_session` 写 `\0elsewhere`——fork 同时只卡一个问题，故其余开放 native 行是漏了答题事件的旧残留，一并关掉，四审 P2）。于是本地先答、手机从没探过也不再被 `/threads` 重推；手机成功注入的真答案由上面的 `record_native_answer` 覆盖之。**唯一未真机验的假设＝PostToolUse 对 attach 答题会不会触发**（建期 API 限速验不了）；若不触发则此钩子惰性，行退回按窗口过期。钩子**只读探优先**：`create_state_db` 初始化本身会写（WAL 切换、迁移），故先用**只读连接** `session_has_open_native_question_readonly` 查有没有 native 行，有才开可写连接走 settle——真正避免常态 no-op 上 SQLite 写锁；只读打开失败即降级 no-op（六审 P3）。install/uninstall/status 对称，status 用 `marker_installed_with_matcher` **校验 matcher 是 `AskUserQuestion`**（四审 P3）。
- **终端会话（非后台 fork）不变**：`native_attach` 假，走 v0.2.10 扣住+填。

**注入进程安全（Sol P2-1）**：`with_attach_pty` fork **前**分配 CString/argv，子进程只 `execv`+`_exit`。`write_all` 处理部分写/EINTR，`waitpid` 处理 EINTR。**程序名走 `CLAUDE_BIN` 权威解析（`resolve_program`/`pop_attach_window`）；无效 `CLAUDE_BIN` 即报错、绝不回退裸 claude**（三审 #7）——注入的 attach 与本地弹的窗口都用 daemon 同一个 claude。

**已知固有极限（pty 注入本质，非可封死的 bug，已如实记档）**：① check→回车之间的微竞态无原子保证，只能收窄（三审 #3）；② 不写整套 ANSI 终端模拟器就没有 100% 可靠的"当前屏"，`flush_pending`+重绘是最佳努力（三审 #2 内核）；③ `Injected` 靠"回车后框消失"作正证据，非应用层 ack（三审 #4），最终由 PostToolUse 钩子兜底核对；④ **数字键上游是否接受＝2026-09-03 真机已验（对 index 2 注入"3"→fork 答出 BLUE）**，不再是未验项；仍未真机验的只剩 PostToolUse 对 attach 答题是否触发、双 surface 并发时序（建期 API 限速）；⑤ **回车后空帧的处置**：`claude attach` 镜像仍在跑的会话，成功后 fork 继续工作、attach 持续产出，故空帧＝attach 没产出＝它死了（崩溃）→判 `Unreachable`（"attach 答完即退"的前提已否决）。即便退一步当歧义看，也按**不对称危害**取可恢复侧＝`Unreachable`：判 Injected 会记下 fork 可能没收到的答案、不可恢复；判 Unreachable 只是可重试、钩子仍会在真答时结算行（四→六审反复围绕此点，六审 P2 证 Injected 侧不可恢复）。

**测试**：`fork_dialog` 8（数字序列、窗口 argv、chrome 判定、present→`Injected`、非选择器屏→`Unreachable`、无帧→`Unreachable`、**答后空帧→`Unreachable`（不可恢复侧的对立）**、**答后框还在→`Unreachable`**）；daemon 6（注入并事后记账、非选择器屏→`Unreachable` 行留开、attach 失败→`Unreachable`+`retryable`、终端会话不注入、**PostToolUse 钩子关全部开放行含旧残留**、**真答案覆盖软结算**）；hooks 幂等含 PostToolUse 组（removed +5）。stub＝**后台连续 printf 模拟活屏重绘 + `head -c` 收键 + 答后画 `after`**（`after` 空＝模拟 attach 死/答后没产出→空帧、`after`=chrome＝框卡住不消失）；死 stub 模拟无帧。（**测试细节以上方 2026-09-03 重构注为准**：`fork_dialog` present（chrome 在 + `still_authorized`=true）→`Delivered`、**题已不 `Open`（`still_authorized`=false）→`Unreachable` 且零键**、非选择器屏/无帧→`Unreachable`；state 侧 settle-stale 结算旧开放行 + 钩子只填开放行不覆盖历史；已无"数字后框关/回车后框消失"这类判据。）全量 422/422 全绿、clippy `-D warnings` 退 0、fmt 净。
