# BBVoxi 1.0.0 版本发布说明

> **Release: BBVoxi 1.0.0 — 首发正式版**
> 发布日期：2026-09-10 ｜ 状态：稳定版（Stable） ｜ 后续版本：[版本历史](./版本历史.md)

本文档记录 BBVoxi **1.0.0** 首发正式版的全部交付物，包括安装、功能、技术要点、测试结果与已知限制。
后续每个版本都将以同一目录约定新增一份 `docs/RELEASE_<版本号>.md`，统一在 `docs/版本历史.md` 中维护索引。

---

## 1. 包信息

| 项 | 值 |
|---|---|
| 文件名 | `BBVoxi-1.0.0.exe` |
| 路径 | `dist/BBVoxi-1.0.0.exe`（仓库根的 `dist/` 目录，已被 `.gitignore` 忽略） |
| 大小 | **7,888 KB（7.89 MB / 8 272 384 字节）** |
| SHA-256 | `13abbc96778350a5eda2139fa100fba913a13b4804c0a87f073102523c9e449a` |
| 构建时间 | 2026-09-10 20:24:34（UTC+8） |
| 编译目标 | `x86_64-pc-windows-msvc`, `release` |
| 单文件部署 | ✅ 无需运行时，所有依赖静态链接 |
| PE 资源校验 | `RT_ICON` × 7 + `RT_GROUP_ICON` × 1（图标组正常） |

> 如需校验下载完整性：
> `Get-FileHash dist\BBVoxi-1.0.0.exe -Algorithm SHA256`
> 期望值：`13ABBC96778350A5EDA2139FA100FBA913A13B4804C0A87F073102523C9E449A`

---

## 2. 一句话定位

**Windows 端按住即说、边说边出字、松开就输入的语音输入法。**
一个常驻托盘的轻量 Rust 原生应用，只做一件事：把语音变成光标处的文字。

---

## 3. 系统要求

| 项 | 要求 |
|---|---|
| 操作系统 | Windows 10 1809 及以上 / Windows 11 |
| 架构 | x86_64（64-bit） |
| 麦克风 | 任意可被系统识别的输入设备（48 kHz 优选，自动重采样到 16 kHz） |
| 网络 | 三家 ASR 服务商之一可访问（详见第 5 节） |
| 权限 | 普通用户权限即可；遇到管理员权限窗口输入失败时按系统提示提升一次 |
| 运行时 | **无**（单文件 PE 静态链接，无 VC++/Edge WebView 等额外依赖） |

---

## 4. 安装与首次运行

```powershell
# 1. 把 BBVoxi-1.0.0.exe 放到任意目录（推荐放 %LOCALAPPDATA%\BBVoxi\）
# 2. 双击运行：右下角出现 BBVoxi 托盘图标
# 3. 右键托盘 → "设置"，填入 ASR 服务商与 API Key
# 4. 设置里点"连通性测试"——看到 ✅ 即可使用
# 5. 任意前台窗口下，按住 Ctrl+1 说话，松开即可注入文字
```

- **卸载**：直接删除 `BBVoxi-1.0.0.exe`；用户配置与日志保留在 `%APPDATA%\BBVoxi\`（详见第 9 节）。
- **不写注册表、不创建计划任务**，可选的"开机自启"通过 `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\` 的 `.lnk` 实现，可随时在设置里关闭。

---

## 5. 支持的 ASR 服务商

三家可切换，API 地址写死，无需在 UI 里手填：

| 简称 | 服务商 | 鉴权方式 | 流式 | 是否需心跳 |
|---|---|---|---|---|
| **qwen** | 阿里云百炼（通义千问 Paraformer） | `X-API-Key: <key>`（新版） | ✅ | ✅ 必须 `heartbeat: true`（千问静默会超时断连） |
| **doubao** | 火山引擎（豆包 2.0） | `Authorization: Bearer <key>` | ✅ | 否（推荐 `StreamMode=2` + `enable_nonstream`） |
| **tencent** | 腾讯云 ASR（Hy-ASR-3.0-preview） | `Authorization: Bearer <secret>` | ✅ | 否（**客户端 55 秒强制停止**，因服务端只接受 ≤60 秒包） |

三家统一音频规格：**16 kHz / 16 bit / mono / PCM**。
麦克风多为 48 kHz，需在客户端用 rubato 重采样到 16 kHz（详见第 7 节）。

---

## 6. 完整功能清单（1.0.0）

### 6.1 语音输入主线
- [x] **全局快捷键录音**：默认 `Ctrl+1`，设置里可改组合键
- [x] **吞按键（pass-through）**：按住过程不向前景窗口投递快捷键，避免浏览器切标签等问题
- [x] **悬浮条实时反馈**：录音开始后弹出，显示识别中间文本与状态
- [x] **边说边出字（实时输入）**：每来一帧中间结果就更新到光标处，松手再对账到最终结果
- [x] **松手收尾识别**：多句识别时累加所有 `sentence_end=true` / `definite=true` / `slice_type=1` 的文本
- [x] **光标处注入**：`SendInput` + `KEYEVENTF_UNICODE` 逐字符模拟键盘打字（**非**剪贴板粘贴）
- [x] **修饰键清场**：注入前先发 `Ctrl/Alt/Shift/Win` 左右键的 KEYUP，避免 `Ctrl+Backspace` 删整词卡顿

### 6.2 三种使用方式
1. **按住默认快捷键说话**（常驻模式，无时长上限）
2. **设置 → "测试识别"**：5 秒自动停止，结果填回设置面板，供联调/排障
3. **设置 → "连通性测试"**：纯网络/鉴权握手，不录音

### 6.3 设置面板（egui / eframe 0.35）
- ASR 服务商切换（三家）+ 对应 API Key 输入
- 快捷键自定义（组合键 UI 选择）
- 开机自启开关（写 `Start Menu\Programs\Startup` 的 `.lnk`）
- 自动标点开关（默认开，可关闭）
- "语音顺滑"开关（默认开，可关闭）
- 语言固定为中文（按需求约定，不暴露语言选项）

### 6.4 稳定性与可观测性
- [x] **panic 带文件/行号写日志**（`unwind` + 自定义 `panic_hook`，**不**用 `panic = "abort"` 防止静默消失）
- [x] **关键异步链路** `catch_unwind` 包裹，单次异常不影响主流程
- [x] **音频缓冲满只丢帧不中断**（回压式处理）
- [x] **慢操作监控**：注入 ≥ 80 ms 才写 `实时输入偏慢：…` 警告
- [x] **日志路径** `%APPDATA%\BBVoxi\logs\bbvoxi.log`（详见第 9 节）
- [x] **配置热加载**：设置变更立即写盘，重启生效

### 6.5 安全与隔离
- [x] 单进程、无第三方网络库（rustls + tokio-tungstenite，TLS 1.2/1.3）
- [x] 全部 ASR 厂商走 HTTPS / WSS
- [x] **录音开始记录前台窗口**：中途切换就停用实时输入（退格不会打到别的窗口）
- [x] **测试模式永不外打**（"测试识别"只把结果写回设置面板）

---

## 7. 技术亮点（架构与算法）

```
┌─────────┐   全局键盘钩子    ┌──────────┐
│ 用户    │ ───────────────▶ │ hotkey   │ ← 仅当焦点不在本程序时拦截
└─────────┘   pass-through   └────┬─────┘
                                  │ 开始/结束
                          ┌───────▼────────┐
                          │ session         │ 会话编排：断句、收尾、对账
                          └────┬───────┬────┘
                               │       │
              ┌────────────────▼─┐ ┌───▼─────────────┐
              │ audio（采集+重采样）│ │ injector（SendInput）│
              └────────┬──────────┘ └─────────────────┘
                       │ 16 kHz / 16 bit / mono PCM
              ┌────────▼─────────┐
              │ asr/{qwen,doubao,tencent}
              └────────┬─────────┘
                       │ WebSocket 流式
              ┌────────▼─────────┐
              │ UI（悬浮条+设置窗）│
              └──────────────────┘
```

### 7.1 音频采集与重采样
- 麦克风（多为 48 kHz）→ cpal 采集 → **rubato** 盒式滤波降采样到 16 kHz（**非整数比**也精准，避免频谱镜像）
- 100 ms 分帧，定时推入 WebSocket 发送队列
- 回压式：缓冲队列满则丢帧而不阻塞采集线程

### 7.2 实时输入（边说边出字）的核心算法
> 代码位置：`src/typer.rs`

```
let sent     = 注入器已写入目标程序的尾部       // 例如 "你好"
let desired  = 本次会话期望的当前文本           // 例如 "你好，请问今天"
let common   = longest_common_prefix(sent, desired)
let back     = sent.len() - common.len()
let forward  = desired.len() - common.len()
                             └─ 退格 back 次，再补打 forward 个字符
```

- **不会简单 append**：中间结果会被服务端修正，简单追加会留下错误前缀
- **松手后对账一次**：保证最终文字 == 最终识别结果
- **录音中途切换窗口 → 自动停用实时输入**（只在同一窗口内打字，避免退格打到错地方）

### 7.3 注入前的"清修饰键"细节
按住组合键（`Ctrl+1`）说话时，修饰键状态真实为"按下"，`SendInput` 必须先发修饰键左右键 KEYUP：
- 注入批次**开头**补发：`Ctrl_L/Ctrl_R / Alt_L/Alt_R / Shift_L/Shift_R / Win_L/Win_R` 的 KEYUP
- 右侧修饰键带 `KEYEVENTF_EXTENDEDKEY`
- 必须与文本**同一次** `SendInput` 调用内（否则修饰键消息被插回队列）
- **绝不**用"等待修饰键松开"——按住说话永远等不到，实测每次卡 1.5 秒并把音频发送堵死

### 7.4 关键工程经验（已沉淀）
- **`unsafe` 回调里的判定逻辑一定要抽成纯函数再测**：`hooks.decide()` 抽出来后，单测一次就抓到了漏写 `0xA2`（VK_LCONTROL）的 bug——Windows 低级键盘钩子的虚拟键码**左右分开**，低级钩子**不上报**通用 `VK_CONTROL(0x11)`，必须列全 `VK_L/R*`。
- **rustls 0.23 必须显式启用加密后端**：`Cargo.toml` 里 `rustls = { version="0.23", default-features=false, features=["ring","std","tls12","logging"] }`，并在首次握手前 `rustls::crypto::ring::default_provider().install_default()`（幂等）。
- **常驻程序禁用 `panic = "abort"`**：一次 panic 静默消失，排查极难，必须用 `unwind` + 钩子写日志 + `catch_unwind`。
- **注入前确认前台窗口**：用户刚点过本程序按钮时前台是 BBVoxi 窗口，`SendInput` 会把字打给自己——录音开始时即收起主窗口，注入前再比对前台窗口 PID。

---

## 8. 模块清单（14 个）

| 模块 | 职责 |
|---|---|
| `src/main.rs` | CLI 入口（默认常驻 / `--settings` 打开设置窗 / `--test` 启动后立即录音 5 秒） |
| `src/config.rs` | 配置文件、默认值、热加载、`#[serde(default)]` 容错解析 |
| `src/hotkey.rs` | 全局键盘钩子（含可单测的 `decide()` 纯函数，规则详见 7.4） |
| `src/audio.rs` | cpal 采集、rubato 48→16 k 重采样、100 ms 分帧、回压缓冲 |
| `src/injector.rs` | `SendInput` 注入、清修饰键、KC 校验、前台窗口比对 |
| `src/typer.rs` | 实时输入算法（最长公共前缀 + 回退/补打） |
| `src/asr/mod.rs` | ASR 接口：`Stream<Item=Event>`、事件枚举（`Partial` / `SentenceEnd`） |
| `src/asr/qwen.rs` | 通义千问 Paraformer 流式 WebSocket |
| `src/asr/doubao.rs` | 火山引擎豆包 2.0（`StreamMode=2` + `enable_nonstream`） |
| `src/asr/tencent.rs` | 腾讯云 Hy-ASR-3.0-preview（55 秒客户端强制停止） |
| `src/session.rs` | 会话编排：开/收、对账、收尾、返回类型转换 |
| `src/ui/mod.rs` + `ui.rs` | egui/eframe 0.35 设置窗与悬浮条 |
| `src/autostart.rs` | 开机自启（写 `Start Menu\Programs\Startup\BBVoxi.lnk`） |
| `src/log.rs` | 滚动日志、panic hook、慢操作监控 |

构建产物：`build.rs`（Windows SDK `rc.exe` 注入 PE 图标资源）、`scripts/make_icons.py`（图标源生成）。

---

## 9. 路径与产物

| 用途 | 路径 |
|---|---|
| 发布 exe | `dist/BBVoxi-1.0.0.exe` |
| 用户配置 | `%APPDATA%\BBVoxi\config.json` |
| 识别日志 | `%APPDATA%\BBVoxi\logs\bbvoxi.log` |
| 开机自启 | `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\BBVoxi.lnk`（设置开启时存在） |
| Settings 窗 PID 锁定 | 同一用户下 `--settings` 单实例（`Global\BBVoxiSettingsMutex`） |
| 缓存/中间文件 | 无（无 SQLite、无本地语音数据保留） |

---

## 10. 测试矩阵

| 项 | 结果 |
|---|---|
| `cargo fmt --check` | ✅ 通过 |
| `cargo test` | **52 passed / 0 failed**（含纯函数 `decide()`、`typer` LCS、配置默认值等） |
| 发布包实机冷启动 | ✅ 日志出现 `BBVoxi 启动` + `注册全局快捷键：Ctrl + \`` |
| PE 资源校验 | ✅ `RT_ICON` × 7 + `RT_GROUP_ICON` × 1 |
| ASR 三家握手（开发机） | ✅ 千问 / 豆包 / 腾讯 各自连通 |
| 注入跨应用实测 | ✅ 记事本 / VS Code / 浏览器 / 命令行 均能打字 |

> 发布前未在物理重启 / 多用户切换 / UAC 弹窗路径下做完整回归——详见第 11 节"已知限制"。

---

## 11. 已知问题与限制（节选）

| 限制 | 现状 | 计划 |
|---|---|---|
| 仅 64 位 Windows | 1.0.0 仅发布 `x86_64-pc-windows-msvc`；Win7/32 位不覆盖 | 后续视需求评估 |
| 仅中文识别 | 语言不暴露，按需求约定写死 zh | 后续视需求评估 |
| 管理员窗口输入失败 | 在管理员提升的窗口（如任务管理器、注册表编辑器）`SendInput` 默认投递不到 | 设置面板内弹"以管理员身份重启"引导；1.0.0 暂时**不**自动 UAC |
| 悬浮条无法拖动 | 固定底部居中显示，未做拖动/记忆位置 | 1.x 路线图 |
| 多麦克风选择 UI | 自动选系统默认 | 后续 |
| 偶发 ASR 厂商服务端 5xx | 日志会写明 + 显示在悬浮条；用户可手动重说 | 后续接入重试退避 |
| 资源占用 | 工作集 ~170 MB | egui + glow 已最小化；如要进一步降，需换原生 Win32 UI |

---

## 12. 升级指南

### 12.1 用户升级（任意旧版 → 1.0.0）
1. 关闭正在运行的 BBVoxi（右键托盘 → 退出）
2. 用 `BBVoxi-1.0.0.exe` 覆盖旧 exe（**无需**卸载）
3. `%APPDATA%\BBVoxi\` 下配置保持兼容；不兼容字段会被忽略并回退默认
4. 双击运行即可

### 12.2 从源码构建
```bash
git clone <repo>
cd BBVoxi
cargo build --release          # → target/release/bbvoxi.exe (~8.0 MB)
# 或与发布包完全一致：
mkdir -p dist
cp target/release/bbvoxi.exe dist/BBVoxi-1.0.0.exe
```

---

## 13. 安全与隐私声明

- **不收集**：用户名、机器码、地理位置、输入法历史
- **不上传**：本地配置之外的任何文件
- **收集**：仅将**用户在按住期间**的语音流通过 HTTPS/WSS 发往用户在设置里选定的 ASR 服务商；松手即停
- **保留**：用户在设置里填写的 API Key（本地明文，文件级权限；改进方向：调用 Windows DPAPI 加密，见 1.x 路线图）
- **第三方**：三家 ASR 服务商各自的服务条款/数据策略以官方为准

---

## 14. 反馈渠道

- 项目主页：本仓库 `Issues` 区
- Bug Report：请附 `%APPDATA%\BBVoxi\logs\bbvoxi.log` 最近 200 行 + 操作系统版本 + ASR 服务商
- Feature Request：欢迎贴在 `Issues` 区并描述使用场景

---

## 15. 版权与许可

本仓库使用 **MIT License** 授权，详见仓库根 `LICENSE` 文件。
未经书面许可，不得使用 BBVoxi 商标、Logo 二次分发。

---

## 附：1.0.0 发布检查表（已勾）

- [x] 版本号：`1.0.0`
- [x] `Cargo.toml` 版本字段同步更新
- [x] `cargo fmt --check` 通过
- [x] `cargo test` 52/52 通过
- [x] `cargo build --release` 成功，单文件 ~8 MB
- [x] PE 资源（含图标组）校验通过
- [x] 发布包实机冒烟（启动 + 快捷键注册）
- [x] SHA-256 记录于本文档 §1
- [x] 三家 ASR 连通性验证（开发环境）
- [x] 文档齐全：本文档 + `README.md` + `docs/版本历史.md`
- [x] `.gitignore` 已忽略 `dist/`、`target/`、`.workbuddy/`

> 等价说明已经写进 GitHub Release body 草稿（粘贴即用），方便手动发布。
