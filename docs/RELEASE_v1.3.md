# BBVoxi 1.3 版本发布说明

> **Release: BBVoxi 1.3 — 三级输入兜底 + 项目地址 + 全量审查**
> 发布日期：2026-09-24 ｜ 状态：稳定版（Stable） ｜ 上一版本：[1.2](./RELEASE_v1.2.md) ｜ 版本索引：[版本历史](./版本历史.md)

本文档记录 BBVoxi **1.3** 的交付物，包括安装、功能、技术要点、测试结果与已知限制。

本版做三件事：
1. **新增三级输入兜底**（核心新功能）：文字打不进去时不再"就此放弃"，自动改用剪贴板粘贴，连粘贴也不行就把结果放进剪贴板 —— 主人按 `Ctrl+V` 就能取回；
2. **新增「项目地址」按钮**：设置窗底部可一键打开本项目主页；
3. **全量代码审查**：修掉 1 个会**静默丢字**的严重缺陷、3 个中等缺陷和若干小问题。

单元测试 **98 条全部通过**（上一版 82 条）。

---

## 1. 包信息

| 项 | 值 |
|---|---|
| 文件名 | `BBVoxi-1.3.exe` |
| 路径 | `dist/BBVoxi-1.3.exe`（仓库根的 `dist/` 目录，已被 `.gitignore` 忽略） |
| 大小 | **8,301,056 字节（8,107 KB / 7.92 MB）** |
| SHA-256 | `8F817BE039C6E990FD88CF44A59D223FE9FCFA03439020FBEBFC3490E60E9FC3` |
| 构建时间 | 2026-09-24 17:04:25（UTC+8） |
| 编译目标 | `x86_64-pc-windows-msvc`, `release` |
| 单文件部署 | ✅ 无需运行时，所有依赖静态链接 |
| 版本号约定 | **对外发布用 `v1.3`**（两位版本号）：exe / tag / Release 标题均为 `1.3`。Cargo.toml 内因 SemVer 字段限制为 `1.3.0`，不影响对外命名 |

> 如需校验下载完整性：
> `Get-FileHash dist\BBVoxi-1.3.exe -Algorithm SHA256`
> 期望值：`8F817BE039C6E990FD88CF44A59D223FE9FCFA03439020FBEBFC3490E60E9FC3`

---

## 2. 一句话定位

**Windows 端按住即说、边说边出字、松开就输入的语音输入法。**
本版核心是解决主人反馈的痛点：**字有时打不进去或打错地方** —— 现在打不进去也会自动兜底，
结果永远不会凭空消失。

---

## 3. 系统要求

| 项 | 要求 |
|---|---|
| 操作系统 | Windows 10 1809 及以上 / Windows 11 |
| 架构 | x86_64（64-bit） |
| 麦克风 | 任意可被系统识别的输入设备（16 kHz 及以上优选；低于 16 kHz 亦可用，自动插值升采样） |
| 网络 | 三家 ASR 服务商之一可访问（详见第 5 节） |
| 权限 | 普通用户权限即可（管理员权限的**目标窗口**无法输入，见第 11 节） |
| 运行时 | **无**（单文件 PE 静态链接） |

---

## 4. 安装与首次运行

1. 把 `BBVoxi-1.3.exe` 放到任意目录（推荐 `%LOCALAPPDATA%\BBVoxi\`）
2. 双击运行：右下角出现 BBVoxi 托盘图标（已有旧版在运行则直接唤醒并弹出设置窗）
3. 设置窗里选服务商、填 API Key（腾讯云填 App ID / Secret ID / Secret Key 三元组）
4. 点「测试识别（5 秒）」说一句话确认能出字（**结果只显示在窗口里**，不会往外打字）
5. 点「保存」，之后关闭窗口 = 最小化到托盘
6. 在任意程序的光标处按住快捷键说话，松开即输入

- **卸载**：直接删除 exe；配置与日志保留在 `%APPDATA%\BBVoxi\`
- 可选「开机自启」写在设置窗的「通用」卡片里
- 设置窗底部有**「项目地址」**按钮：点击后用系统默认浏览器打开本项目主页

---

## 5. 支持的 ASR 服务商

三家可切换，接口地址写死，无需在 UI 手填（与 1.2 一致）：

| 简称 | 服务商 | 鉴权方式 | 流式 |
|---|---|---|---|
| **千问** | 阿里云百炼（qwen-audio-3.0-asr-flash-streaming） | `Authorization: Bearer <key>` | ✅ |
| **豆包** | 火山引擎（volc.seedasr.sauc.duration） | `X-Api-Key` / `X-Api-Resource-Id` | ✅ |
| **腾讯云** | 腾讯云 ASR（Hy-ASR-3.0-preview / 16k_zh_en） | URL 签名（AppID + SecretID + SecretKey） | ✅ |

三家统一音频规格：**16 kHz / 16 bit / mono / PCM**（客户端自动重采样，含升采样）。

---

## 6. 完整功能清单

### 6.1 本版新增（重点）

- [x] **三级输入兜底**：逐字注入被目标程序拒掉时，不再"就此放弃"
  - ① **字符注入**（首选）：`SendInput` 逐字打，不碰剪贴板
  - ② **剪贴板粘贴**：写入结果 → 注入 `Ctrl+V`（`KEYEVENTF_UNICODE` 与 `Ctrl+V` 是两条不同通道，远程桌面 / 部分 Electron / Java / 老 MFC 往往认后者）
  - ③ **只放剪贴板**：一个字都不输入，把结果放进剪贴板并提示按 `Ctrl+V`
- [x] **剪贴板内容还原**：粘贴成功后**延迟还原主人原来的剪贴板内容**（先核对剪贴板序号，期间主人复制了新东西就不还原）；没粘成则不还原（那是主人唯一的取回通道）
- [x] **「识别结果总留一份到剪贴板」开关**（默认关）：成功输入也留一份，覆盖"注入报了成功、字却被目标程序悄悄丢掉"这一档（客户端观测不到的情况）
- [x] **「项目地址」按钮**：设置窗底部，用系统默认浏览器打开 `https://github.com/HaiSeaman/BBVoxi`

### 6.2 本版修复（全量审查）

- [x] **【严重】注入失败后结果被静默丢弃**：逐字注入失败过之后，收尾 `finish` 返回"什么都没做"的假成功 —— 结果既不进目标程序也不进剪贴板，主人的话直接消失。现在失败必须如实报错，由会话层把整段结果放进剪贴板
- [x] **切走窗口后一次性放弃、永不复位**：以前切出去看一眼再切回来，整段识别结果就没了。现在"切走只是暂停，切回来就恢复"（`sent` 记的正是原窗口里的字，继续差分是安全的）
- [x] **改键捕捉的 `paused` 锁存**：设置窗收起后 `ui()` 不再运行，若捕捉状态留在 true，全局快捷键会**彻底失效**且毫无提示。现在捕捉跟着界面一起过期（400ms 心跳兜底，覆盖所有隐藏路径）
- [x] **剪贴板 `EmptyClipboard` 顺序错误**：必须在写入数据分配完成之后才清空剪贴板，否则写入可能被清空误伤
- [x] **配置损坏的报错无处可去**：`eprintln!` 在无控制台的 release 下会消失，改为写日志
- [x] **开机自启句柄泄漏**：`current_exe()` 失败时注册表键不关闭，改为"先备好要写的值再开注册表键"
- [x] 死代码清理：`qwen.rs` 的 `finished` 字段、`asr/mod.rs` 的 `provider_name()`、`session.rs` 的 `error_at` 字段
- [x] 腾讯云 `slice_type` 缺字段按 0 处理（不再拿 -1 这种值域外哨兵当默认，与豆包踩过的坑一致）

### 6.3 语音输入主线（延续）

- [x] 全局快捷键录音（默认 `Ctrl+1`，按住说、松开输入，可改成任意组合）
- [x] 吞按键：按住过程不向前景窗口投递该组合键
- [x] 边说话边打字（实时输入）：中间结果实时写入光标处，识别被修正时自动回退重打
- [x] 测试模式（5 秒）验证凭据与麦克风，结果只回显在窗口里
- [x] 托盘常驻、关窗即最小化、双击 exe 唤醒后台实例
- [x] 开机自启（注册表 Run 项）

### 6.4 稳定性与可观测性（延续）

- [x] panic 带文件/行号写日志；会话 `catch_unwind` 包裹，单次异常不带走程序
- [x] 日志本地时间戳、滚动（单份 2 MB + 2 份历史）
- [x] 音频缓冲满只丢帧不中断
- [x] 剪贴板被占用时重试 5 次（每次 20ms）再报错

---

## 7. 技术亮点

### 7.1 两条通道：为什么粘贴能救逐字注入

`SendInput` + `KEYEVENTF_UNICODE`（模拟键盘逐字输入）和 `Ctrl+V`（剪贴板粘贴）在 Windows 里走的是**完全不同的两条路径**。有些程序（远程桌面、部分 Electron/Java 应用、老 MFC 控件）会忽略前者但认后者。所以"逐字注入被拒"不是终点 —— 换一条通道再试一次，是成熟产品（morvox / OpenWhispr / Whisperstream）共同认可的兜底思路。

### 7.2 剪贴板兜底的三个自我保护细节

1. **先快照、后写入、再粘贴、最后延迟还原**：主人原来的内容只在**第一次**粘贴之前快照一次（每次都重新快照，第二次会拍到我们自己写进去的碎片，还原出来比不还原还糟）；
2. **还原前核对剪贴板序号**：粘贴成功后延迟 300ms，期间主人复制了新东西就不还原（`GetClipboardSequenceNumber`）；
3. **UIPI 直接跳过粘贴**：管理员权限窗口拦下的是**整个** `SendInput`，`Ctrl+V` 同样被拦 —— 看到 `AccessDenied` 就不白试，直接进第 ③ 级。

### 7.3 修复"静默丢字"：失败必须"喊出声"

`LiveTyper::finish` 原来走 `plan(blocked())`，而 `blocked()` 把"已失败"也算作禁止注入 —— 于是返回一个"什么都不做"的 `Ok`。调用方 `session` 靠 `Err` 才把整段结果放进剪贴板，这行 `Err` 一假，结果两头落空。

本版在 `finish` 开头显式检查失败状态并**抛出**：

```rust
if let Some(reason) = &self.failure {
    return Err(anyhow!("{reason}"));
}
```

配套回归测试 `finish_reports_a_latched_failure_so_the_caller_can_use_the_clipboard`
专门守这条链，并做了变异检查（临时把判断改成恒假 → 测试立刻红 → 证明测试有效）。

### 7.4 改键捕捉与界面心跳

eframe 0.35 里窗口隐藏后 `ui()` 不再被调用（`logic()` 照常 10Hz）。改键捕捉期间钩子是**暂停**的（`paused=true`），一旦窗口收起就"捕捉既结束不了、`paused` 也永远留在 true"——全局快捷键**永久失效**且无提示。解法：`ui()` 每次绘制时更新 `painted_at` 心跳，`logic()` 里检查心跳超过 400ms（4 倍帧间隔余量）就自动结束捕捉，覆盖所有已知和未来的隐藏路径。

### 7.5 项目地址：为什么不用 egui 的 `open_url`

eframe 0.35 的 `egui::Context::open_url` / `Hyperlink` 只是投递 `OutputCommand::OpenUrl`，**只有 web runner 消费它** —— 在 Windows 原生窗口上是空操作。改用 `ShellExecuteW`（`Win32_UI_Shell` 特性），返回值 >32 才算成功。

### 7.6 架构图（本版新增第⑤层）

```
┌────── 用户按下快捷键 ──────┐
│ hotkey.rs  WH_KEYBOARD_LL  │──吞掉组合键、发 Cmd::Start
└─────────────┬──────────────┘
              ▼
┌──── session.rs（一次会话）────────────────────────────┐
│ ① audio.rs     麦克风采集 → 重采样 16k → 100ms 帧      │
│ ② asr/         WebSocket 推流 + 解析三家协议           │
│ ③ typer.rs     中间结果实时差分写入 / 注入失败时切通道  │  ← 本版新增兜底切换
│ ④ injector.rs  SendInput 逐字注入 / 退格 / Ctrl+V     │  ← 本版新增粘贴
│ ⑤ clipboard.rs 剪贴板读写 / 快照 / 延迟还原           │  ← 本版新增模块
└─────────────┬────────────────────────────────────────┘
              ▼
        前台程序的光标处
```

---

## 8. 模块清单（15 个，新增 1 个）

| 模块 | 职责 | 本版改动 |
|---|---|---|
| `src/main.rs` | 入口：panic 钩子、单实例互斥 + 跨实例唤醒、托盘 | **改键捕捉随界面过期（`reap_capture_if_ui_gone`）** |
| `src/config.rs` | 配置模型与持久化、接口地址 | **+ 两个新开关；损坏回退改记日志** |
| `src/hotkey.rs` | 全局键盘钩子、吞键、快捷键解析 | 内部取值修正 |
| `src/audio.rs` | cpal 采集、重采样、100ms 分帧 | — |
| `src/injector.rs` | `SendInput` 注入、退格、修饰键重置 | **+ `Ctrl+V` 粘贴、`AccessDenied` 错误类型、扫描码映射** |
| `src/typer.rs` | 实时输入差分、收尾对账 | **+ 三级兜底切换、剪贴板快照/还原、失败必须报错** |
| `src/clipboard.rs` | 剪贴板纯文本读写 | **本版新增** |
| `src/asr/mod.rs` | WebSocket 框架、结果累积 | 删死字段/死函数 |
| `src/asr/qwen.rs` | 千问协议 | 删 `finished` 死字段 |
| `src/asr/doubao.rs` | 豆包协议 | — |
| `src/asr/tencent.rs` | 腾讯云协议 | `slice_type` 缺字段按 0 处理 |
| `src/session.rs` | 会话编排 | **+ 兜底接线、幽灵会话、`watch` 竞态、失败时剪贴板交接** |
| `src/ui.rs` | egui 设置窗 | **+ 项目地址按钮、改键捕捉心跳、`paused` 修复** |
| `src/autostart.rs` | 开机自启 | 句柄泄漏修复 |
| `src/log.rs` | 滚动日志、本地时间戳 | — |

---

## 9. 路径与产物

| 用途 | 路径 |
|---|---|
| 发布 exe | `dist/BBVoxi-1.3.exe` |
| 用户配置 | `%APPDATA%\BBVoxi\config.json` |
| 识别日志 | `%APPDATA%\BBVoxi\logs\bbvoxi.log` |
| 开机自启 | 注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\BBVoxi` |

---

## 10. 测试矩阵

| 项 | 结果 |
|---|---|
| `cargo test --all` | ✅ **98 passed / 0 failed / 3 ignored**（总 101 条；被忽略的是需要真 Key 的联网探针 / 真剪贴板往返 / 真浏览器烟测） |
| 上一版对比 | 82 → 98（本轮新增/改写 16 条） |
| `cargo build --release` | ✅ 通过，单文件 8,301,056 字节 |
| `cargo clippy --all-targets` | ✅ 未新增告警（仍为 14 条历史风格提示） |
| 变异检查 | ✅ 把"严重"修复临时改坏 → 回归测试立刻红 → 恢复全绿，证明测试不是摆设 |

本版新增/改写的回归测试（按修复项）：

| 修复 | 测试 |
|---|---|
| 严重：失败必须报错 | `finish_reports_a_latched_failure_so_the_caller_can_use_the_clipboard` |
| 切走暂停、切回恢复 | `returning_to_the_original_window_resumes_typing` |
| 剪贴板快照只做一次 | `original_clipboard_is_snapshotted_only_once` |
| 兜底默认关（防单测污染） | `fallback_is_off_by_default` |
| 粘贴事件序列 | `paste_is_ctrl_down_v_down_v_up_ctrl_up`、`paste_carries_real_scan_codes`、`paste_ends_with_modifier_reset`、`paste_events_carry_our_tag` |
| 改键捕捉过期 | `capture_expires_only_when_capturing_and_past_the_limit`（4 断言） |
| 旧配置兼容新开关 | `clipboard_options_default_for_old_configs` |
| 腾讯缺字段 | `missing_slice_type_stays_in_the_value_domain` |
| 剪贴板纯函数 | `utf16_is_nul_terminated`、`utf16_keeps_surrogate_pairs_intact`、`empty_text_still_has_a_terminator`、`utf16_length_matches_allocated_bytes` |

> 本版未做实机图形/端到端自动化测试（项目约定只做代码与逻辑验证）。
> 发布前建议人工冒烟四项：① 在某不认逐字注入的程序（如部分 Electron 应用）里验证自动转粘贴；② 录音中途切走再切回，验证恢复输入；③ 收起设置窗后再试快捷键，验证未失效；④ 点「项目地址」确认浏览器打开。

---

## 11. 已知问题与限制

| 限制 | 现状 |
|---|---|
| 仅 64 位 Windows | 未覆盖 Win7 / 32 位 |
| 仅中文识别 | 语言不暴露 |
| 管理员权限窗口输入失败 | Windows UIPI 限制，给出明确提示；`Ctrl+V` 同样被拦 —— **但结果仍会复制到剪贴板**，粘到普通权限窗口即可 |
| **焦点不在输入框时注入/粘贴都无效** | 两者都要靠前台焦点接收，只能靠「识别结果总留一份到剪贴板」兜住 |
| 录音中途切走窗口 → 本次不自动输入 | 防打错地方的设计；切回来自动恢复；一直没回来就把结果放进剪贴板并提示 `Ctrl+V` |
| 兜底粘贴短暂占用剪贴板 | 粘贴成功后延迟约 300ms 还原；那一次会进 Windows 剪贴板历史（`Win+V`） |
| 剪贴板只保得住文本 | 主人原来复制的是图片/文件时无法还原（延迟渲染），日志会写明"不还原" |
| 多麦克风选择 UI | 自动选系统默认设备 |
| 腾讯 `Hy-ASR-3.0-preview` 单次上限 60 秒 | 服务端限制（千问/豆包无此限） |
| 豆包「高精度」模式无中间结果 | 该模式下实时输入看不到逐字上屏，属协议特性 |
| 资源占用 | 工作集约 114~170 MB（egui 基线） |
| `cargo fmt --check` 不通过 | 仓库历史代码与当前 rustfmt 版本存在格式差异（非本版引入） |

---

## 12. 升级指南（任意旧版 → 1.3）

1. 退出正在运行的 BBVoxi（右键托盘 → 退出）
2. 用 `BBVoxi-1.3.exe` 覆盖旧 exe（无需卸载、无需重填配置）
3. 配置保持兼容：`%APPDATA%\BBVoxi\config.json` 旧字段自动忽略、缺失字段回退默认（两个新开关都有默认值）
4. 双击运行即可

> **行为变化提醒**：本版新增的两个开关默认值是「输入被拒时用剪贴板粘贴兜底 = 开」「识别结果总留一份到剪贴板 = 关」。后者若需要，请在设置里显式打开。兜底粘贴会让剪贴板被短暂占用后自动还原，那一次会进 `Win+V` 历史。

---

## 13. 安全与隐私声明

与 1.0 一致，无变化：不收集任何用户信息、无统计无遥测；仅将按住期间的语音流经 WSS 发往**你自己选择**的 ASR 服务商；API Key 明文保存在本地 `%APPDATA%\BBVoxi\config.json`（改进方向保留 DPAPI 加密）；第三方服务的数据策略以其官方说明为准。

---

## 14. 反馈渠道

项目主页 `Issues` 区。报 Bug 请附：`%APPDATA%\BBVoxi\logs\bbvoxi.log` 最近 200 行 + 系统版本 + 所用服务商 + 复现步骤（尤其是"打不进去"时是哪种程序、日志里有没有 `逐字注入被拒` / `已复制到剪贴板`）。

---

## 15. 版权与许可

MIT License，详见仓库根 `LICENSE`。

---

## 附：1.3 发布检查表

- [x] 版本号：`Cargo.toml` → `1.3.0`（Cargo 要求 SemVer 三段式）；**对外发布 exe / tag / 标题统一用 `v1.3`**
- [x] `cargo test --all`：98 passed / 0 failed / 3 ignored
- [x] `cargo build --release` 成功（`Compiling bbvoxi v1.3.0`）
- [x] SHA-256 记录于本文档 §1，且与 `dist/BBVoxi-1.3.exe` 实际值一致
- [x] 文档齐全：本文档 + `docs/全量审查与修复报告-v1.3.md` + `docs/GITHUB发布文案-v1.3.md` + `README.md` + `docs/版本历史.md` 同步更新

---

## 附：GitHub Release 简介

可复制粘贴的 Release 标题与正文见 **`docs/GITHUB发布文案-v1.3.md`**。
