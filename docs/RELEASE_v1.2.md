# BBVoxi 1.2 版本发布说明

> **Release: BBVoxi 1.2 — 全量审查与健壮性修复**
> 发布日期：2026-09-18 ｜ 状态：稳定版（Stable） ｜ 上一版本：[1.1](./RELEASE_v1.1.md) ｜ 版本索引：[版本历史](./版本历史.md)

本文档记录 BBVoxi **1.2** 的交付物，包括安装、功能、技术要点、测试结果与已知限制。
本版是**一次全量代码审查后的健壮性修复版**：不新增功能，重点是把上一版"防止把字/退格打到别的程序里"的修复**做完整**，并修掉音频采样率与 ASR 结束判定上的三个隐患。

单元测试 **82 条全部通过**（上一版 56 条）。

---

## 1. 包信息

| 项 | 值 |
|---|---|
| 文件名 | `BBVoxi-1.2.exe` |
| 路径 | `dist/BBVoxi-1.2.exe`（仓库根的 `dist/` 目录，已被 `.gitignore` 忽略） |
| 大小 | **8,285,696 字节（8,092 KB / 7.90 MB）** |
| SHA-256 | `E50C987EBD3EDA35CC84E93937394BDEF42A35F557357AD9CD86E9F99D592A05` |
| 构建时间 | 2026-09-18 20:23:09（UTC+8） |
| 编译目标 | `x86_64-pc-windows-msvc`, `release` |
| 单文件部署 | ✅ 无需运行时，所有依赖静态链接 |
| 版本号约定 | **对外发布用 `v1.2`**（两位版本号）：exe / tag / Release 标题均为 `1.2`。Cargo.toml 内因 SemVer 字段限制为 `1.2.0`，不影响对外命名 |

> 如需校验下载完整性：
> `Get-FileHash dist\BBVoxi-1.2.exe -Algorithm SHA256`
> 期望值：`E50C987EBD3EDA35CC84E93937394BDEF42A35F557357AD9CD86E9F99D592A05`

---

## 2. 一句话定位

**Windows 端按住即说、边说边出字、松开就输入的语音输入法。**
本版不改交互与界面，专注一件事：**让"绝不打错地方"这条底线真正变成事实**，并补齐音频与协议层的三个健壮性缺口。

---

## 3. 系统要求

| 项 | 要求 |
|---|---|
| 操作系统 | Windows 10 1809 及以上 / Windows 11 |
| 架构 | x86_64（64-bit） |
| 麦克风 | 任意可被系统识别的输入设备（16 kHz 及以上优选；**低于 16 kHz 亦可用，本版起自动插值升采样**） |
| 网络 | 三家 ASR 服务商之一可访问（详见第 5 节） |
| 权限 | 普通用户权限即可 |
| 运行时 | **无**（单文件 PE 静态链接） |

---

## 4. 安装与首次运行

1. 把 `BBVoxi-1.2.exe` 放到任意目录（推荐 `%LOCALAPPDATA%\BBVoxi\`）
2. 双击运行：右下角出现 BBVoxi 托盘图标（已有旧版在运行则直接唤醒并弹出设置窗）
3. 设置窗里选服务商、填 API Key（腾讯云填 App ID / Secret ID / Secret Key 三元组）
4. 点「测试识别（5 秒）」说一句话确认能出字（**结果只显示在窗口里**，不会往外打字）
5. 点「保存」，之后关闭窗口 = 最小化到托盘
6. 在任意程序的光标处按住快捷键说话，松开即输入

- **卸载**：直接删除 exe；配置与日志保留在 `%APPDATA%\BBVoxi\`
- 可选「开机自启」写在设置窗的「通用」卡片里

---

## 5. 支持的 ASR 服务商

三家可切换，接口地址写死，无需在 UI 手填（与 1.1 一致）：

| 简称 | 服务商 | 鉴权方式 | 流式 |
|---|---|---|---|
| **千问** | 阿里云百炼（qwen-audio-3.0-asr-flash-streaming） | `Authorization: Bearer <key>` | ✅ |
| **豆包** | 火山引擎（volc.seedasr.sauc.duration） | `X-Api-Key` / `X-Api-Resource-Id` | ✅ |
| **腾讯云** | 腾讯云 ASR（Hy-ASR-3.0-preview / 16k_zh_en） | URL 签名（AppID + SecretID + SecretKey） | ✅ |

三家统一音频规格：**16 kHz / 16 bit / mono / PCM**（客户端自动重采样，含升采样）。

| 设置 | 千问 | 豆包 | 腾讯云 |
|---|---|---|---|
| 自动添加标点 | ✅ `semantic_punctuation_enabled` | ✅ `enable_punc` | ❌ 服务端决定（UI 禁用并注明） |
| 口语顺滑 | ❌ 暂不支持（UI 禁用并注明） | ✅ `enable_ddc` | ✅ `filter_modal` |

---

## 6. 完整功能清单

### 6.1 本版修复（重点）

- [x] **测试识别不再往外打字**：严格遵守「测试结果只显示在窗口里」的承诺；测试结束设置窗**不会**再被收起，主人能一直看着结果
- [x] **收尾阶段重新校验目标窗口**：松手后等待最终结果期间切走窗口，也不再补打/退格到别的程序（此前只覆盖了录音过程中的实时输入）
- [x] **切窗口后的行为统一**：无论「边说话边打字」开或关，中途切走都**一律放弃自动输入**（此前关着实时输入时会把整段文字打进新窗口）
- [x] **腾讯云 / 豆包松手后立即收尾**：服务端把"结束标记"和"最后一句文本"放在同一个包时，不再白等到 8 秒超时
- [x] **低于 16 kHz 的麦克风自动升采样**：蓝牙耳机免提模式（常见 8 kHz）不再被慢放一倍送出去
- [x] 修正「测试识别」界面计时与 5 秒上限的起跑线不一致
- [x] 修正腾讯云签名 `nonce` 的取值区间与注释不符

### 6.2 语音输入主线（延续）

- [x] 全局快捷键录音（默认 `Ctrl+1`，按住说、松开输入，可改成任意 `Ctrl / Alt / Shift / Win` + 字母、数字、`F1`~`F24` 或空格）
- [x] 吞按键：按住过程不向前景窗口投递该组合键（不会触发浏览器切标签）
- [x] 边说话边打字（实时输入）：中间结果实时写入光标处，识别被修正时自动回退重打
- [x] 光标处注入：`SendInput` + `KEYEVENTF_UNICODE` 逐字符打字，**不碰剪贴板**
- [x] 修饰键清场：注入前发 Ctrl/Alt/Shift/Win 左右键 KEYUP，避免 `Ctrl+Backspace` 删整词
- [x] 测试模式（5 秒）验证凭据与麦克风
- [x] 托盘常驻、关窗即最小化、双击 exe 唤醒后台实例
- [x] 开机自启（注册表 Run 项）

### 6.3 稳定性与可观测性（延续）

- [x] panic 带文件/行号写日志；会话 `catch_unwind` 包裹，单次异常不带走程序
- [x] 日志本地时间戳、滚动（单份 2 MB + 2 份历史）
- [x] 音频缓冲满只丢帧不中断；单次注入 ≥80ms 写警告日志
- [x] 麦克风启动日志写明重采样方式（`线性插值升采样` / `盒式滤波降采样` / `原样通过`）

---

## 7. 技术亮点（本版修复的原理）

### 7.1 把"校验"从调用点搬进类型签名（修复 ②）

上一版给"打字前校验前台窗口"补了一个独立函数，调用点有两处。结果收尾的 `finish()` 那条路径被漏掉了 —— 同一个坑出现第二次。

本版的解法不是"再补一次调用"，而是**改签名**：

```rust
pub fn sync(&mut self, desired: &str, now: Option<isize>) -> Result<()>   // now = 此刻的前台窗口
pub fn finish(&mut self, desired: &str, now: Option<isize>) -> Result<()>
```

`LiveTyper` 自己持有"开始录音时的前台窗口"，两个方法进入后先 `guard(now)`。**调用方不传当前窗口就编译不过** —— 忘记校验从"可能发生"变成"不可能发生"。

窗口句柄拿不到时不拦截（不能因为读不到句柄就停掉主人的输入）；`muted`（测试模式）连收尾都不注入。

### 7.2 结束标记与正文同包的处理（修复 ③）

腾讯和豆包都会把「最后一包」和「最后一句文本」放进同一个响应包。原来的解析是"二选一"：命中文本分支就不再返回"结束"。本版让两者可以同时上报：

```rust
Parsed::Text { kind, text, finished: bool }   // finished = 这一包同时是最后一包
```

累积逻辑从 `AsrClient::handle` 抽成独立的 `Transcript`（`final_text` / `partial_text` / `done` / `last_error`），这样**不需要真的 WebSocket 就能单测"最后一包是否结束会话"** —— 这个缺陷恰好就藏在这一层。

### 7.3 升采样：为什么不能沿用分箱取均值（修复 ④）

`Decimator` 的算法是"把输入样本按 `floor(idx/step)` 分箱取均值"，它**只能**把多个输入样本合成一个输出样本。当输入率低于 16 kHz（`step < 1`）时，每个输入样本各占一箱，输出速率等于输入速率 —— 8 kHz 的音频被贴上 16 kHz 的标签送出去，等于慢放一倍。

本版新增 `Interpolator`：第 m 个输出样本取输入时间轴 `m × 输入率/输出率` 处的线性插值，与 `Decimator` 组成 `Resampler`，按采样率自动选路：

```
输入率 ≥ 16 kHz → Decimator（盒式滤波降采样，均值抗混叠）
输入率 < 16 kHz → Interpolator（线性插值升采样）
```

选线性插值而非"每个样本重复两遍"：后者是零阶保持，会在频谱里产生镜像，听感发毛、识别也更差。

### 7.4 架构图（本版无变化，标注改动点）

```
┌────── 用户按下快捷键 ──────┐
│ hotkey.rs  WH_KEYBOARD_LL  │──吞掉组合键、发 Cmd::Start
└─────────────┬──────────────┘
              ▼
┌──── session.rs（一次会话）─────────────────────────────┐
│ ① audio.rs     麦克风采集 → 重采样 16k → 100ms 帧      │  ← 本版新增升采样
│ ② asr/         WebSocket 推流 + 解析三家协议           │  ← 本版新增 finished 标记
│ ③ typer.rs     中间结果实时差分写入                     │  ← 本版新增 muted / 编译期强制校验
│ ④ injector.rs  SendInput 逐字注入 / 退格               │
└─────────────┬─────────────────────────────────────────┘
              ▼
        前台程序的光标处
```

---

## 8. 模块清单（14 个，未增减）

| 模块 | 职责 | 本版改动 |
|---|---|---|
| `src/main.rs` | 入口：panic 钩子、单实例互斥 + 跨实例唤醒、中文字体、托盘 | — |
| `src/config.rs` | 配置模型与持久化、接口地址 | — |
| `src/hotkey.rs` | 全局键盘钩子：修饰键状态、吞键、快捷键解析 | — |
| `src/audio.rs` | cpal 采集、重采样、100ms 分帧 | **+ 线性插值升采样（`Resampler` / `Interpolator`）** |
| `src/injector.rs` | `SendInput` 注入、清修饰键、代理对拆分 | — |
| `src/typer.rs` | 实时输入差分、收尾对账 | **+ `muted` 测试态、`abandon` 统一语义、`sync/finish` 强制窗口校验** |
| `src/asr/mod.rs` | WebSocket 框架、握手、结果累积 | **+ `Parsed::Text.finished`、抽出可单测的 `Transcript`** |
| `src/asr/qwen.rs` | 千问协议 | 适配 `finished` 字段 |
| `src/asr/doubao.rs` | 豆包协议（二进制头 / gzip / 二次识别） | **最后一包带文本时同报结束** |
| `src/asr/tencent.rs` | 腾讯云协议（URL 签名 / `slice_type`） | **`final=1` 与文本同包时同报结束**；`nonce` 取值修正 |
| `src/session.rs` | 会话编排 | **测试模式短路、收尾前二次校验、计时起跑线对齐** |
| `src/ui.rs` | egui 设置窗 | — |
| `src/autostart.rs` | 开机自启（注册表 Run 项） | — |
| `src/log.rs` | 滚动日志、本地时间戳 | — |

---

## 9. 路径与产物

| 用途 | 路径 |
|---|---|
| 发布 exe | `dist/BBVoxi-1.2.exe` |
| 用户配置 | `%APPDATA%\BBVoxi\config.json` |
| 识别日志 | `%APPDATA%\BBVoxi\logs\bbvoxi.log` |
| 开机自启 | 注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\BBVoxi` |

---

## 10. 测试矩阵

| 项 | 结果 |
|---|---|
| `cargo test --all` | ✅ **82 passed / 0 failed / 1 ignored**（总 83 条；被忽略的是需要真 Key 的联网探针） |
| 上一版对比 | 56 → 82（本轮新增/改写 16 条） |
| `cargo build --release` | ✅ 通过，单文件 8,285,696 字节 |
| `cargo clippy --all-targets` | ✅ **未新增任何告警**（与修复前同为 14/15 条，全是历史风格提示） |
| 变异检查 | ✅ 把 7 处修复逐个改坏，**8 条测试对应失败**，改回后全绿 —— 证明测试不是摆设 |

本版新增/改写的回归测试：

| 修复 | 测试 |
|---|---|
| ① | `muted_session_never_types` |
| ② | `finish_rechecks_the_target_window`、`sync_rechecks_the_target_window`、`guard_stops_typing_after_the_window_changes`、`guard_keeps_typing_in_the_same_window`、`guard_does_not_block_when_handles_are_unknown` |
| ③ | `final_packet_with_text_also_ends_the_session`、`last_packet_with_text_also_finishes`、`transcript_marks_done_when_a_text_packet_also_finishes`、`transcript_keeps_going_without_the_finish_flag`、`transcript_records_server_errors` |
| ④ | `upsamples_low_rate_input_to_target_rate`、`upsampling_interpolates_between_neighbours`、`decimation_path_is_unchanged` |
| ⑤ | `switching_windows_abandons_even_when_live_typing_was_off`、`disabled_by_default_still_plans_its_final_edit` |

> 本版未做实机图形/端到端自动化测试（项目约定只做代码与逻辑验证）。
> 发布前建议人工冒烟四项：① 测试识别不往窗口外打字且设置窗不消失；② 录音中途切窗口后不自动输入；③ 腾讯云松手后立即出字（不空等）；④ 蓝牙耳机下日志出现"线性插值升采样"。

---

## 11. 已知问题与限制

| 限制 | 现状 |
|---|---|
| 仅 64 位 Windows | 未覆盖 Win7 / 32 位 |
| 仅中文识别 | 语言不暴露 |
| 管理员权限窗口输入失败 | Windows UIPI 限制，给出明确提示 |
| **中途切走窗口 → 本次不自动输入** | 本版起两种设置统一为"放弃输入"（这是防打错地方的设计）。结果仍保留在设置窗「最近识别」与日志中 |
| 多麦克风选择 UI | 自动选系统默认设备 |
| 腾讯 `Hy-ASR-3.0-preview` 单次上限 60 秒 | 服务端限制（千问/豆包无此限） |
| 豆包「高精度」模式无中间结果 | 该模式下实时输入看不到逐字上屏，属协议特性 |
| 资源占用 | 工作集约 114~170 MB（egui 基线） |
| `cargo fmt --check` 不通过 | 仓库历史代码与当前 rustfmt 版本存在格式差异（非本版引入）；执行一次 `cargo fmt` 即可统一 |

---

## 12. 升级指南（任意旧版 → 1.2）

1. 退出正在运行的 BBVoxi（右键托盘 → 退出）
2. 用 `BBVoxi-1.2.exe` 覆盖旧 exe（无需卸载、无需重填配置）
3. 配置保持兼容：`%APPDATA%\BBVoxi\config.json` 旧字段自动忽略、缺失字段回退默认
4. 双击运行即可

> **行为变化提醒**：若你平时关掉了「边说话边打字」，且习惯在录音中途切换窗口 —— 本版起这种情况**不会**再自动到新窗口输入（避免打错地方）。

---

## 13. 安全与隐私声明

与 1.0 一致，无变化：不收集任何用户信息、无统计无遥测；仅将按住期间的语音流经 WSS 发往**你自己选择**的 ASR 服务商；API Key 明文保存在本地 `%APPDATA%\BBVoxi\config.json`（改进方向保留 DPAPI 加密）；第三方服务的数据策略以其官方说明为准。

---

## 14. 反馈渠道

项目主页 `Issues` 区。报 Bug 请附：`%APPDATA%\BBVoxi\logs\bbvoxi.log` 最近 200 行 + 系统版本 + 所用服务商 + 复现步骤（尤其是"是否在录音过程中切换过窗口"）。

---

## 15. 版权与许可

MIT License，详见仓库根 `LICENSE`。

---

## 附：1.2 发布检查表

- [x] 版本号：`Cargo.toml` → `1.2.0`（Cargo 要求 SemVer 三段式）；**对外发布 exe / tag / 标题统一用 `v1.2`**
- [x] `cargo test --all`：82 passed / 0 failed
- [x] `cargo build --release` 成功（`Compiling bbvoxi v1.2.0`）
- [x] SHA-256 记录于本文档 §1，且与 `dist/BBVoxi-1.2.exe` 实际值一致
- [x] 文档齐全：本文档 + `docs/全量审查与修复报告-v1.2.md` + `docs/GITHUB发布文案-v1.2.md` + `README.md` + `docs/版本历史.md` 同步更新

---

## 附：GitHub Release 简介

可复制粘贴的 Release 标题与正文见 **`docs/GITHUB发布文案-v1.2.md`**（或本文档上一版本 §末的同类结构）。
