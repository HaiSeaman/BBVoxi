# BBVoxi · GitHub 发布文案（复制粘贴用）

> 用法：找到你要的那一块，**整块复制**（不要带上最外层的 ``` 三个反引号），粘贴到 GitHub 对应位置即可。
> 所有内容都已核对过项目实际功能，没有夸大。

---

## 一、About · 英文（推荐用这条）

位置：GitHub 仓库首页右上角 **About** 齿轮 → **Description**

```text
Push-to-talk voice typing for Windows: hold a hotkey, speak, release - the text lands right at your cursor like a normal input method. Native Rust, single 8 MB exe, no install. Three streaming ASR providers (Alibaba Qwen / Volcengine Doubao / Tencent Cloud) with live incremental typing and auto-correction.
```

字符数：约 300（GitHub 上限 350，安全）

---

## 二、About · 中文（备选，或给中文用户看）

位置：同上 Description 框

```text
Windows 按住说话的语音输入法：按住快捷键说话、松开即把文字打进光标处，像输入法一样自然。Rust 原生实现，单文件 exe 免安装。支持阿里云千问 / 火山豆包 / 腾讯云三家流式识别，边说边出字并能自动纠正。
```

字符数：约 105

---

## 三、Website（可选，留空也行）

位置：About 面板的 **Website** 字段

```text
（留空即可。等你有了演示视频或主页再填。）
```

---

## 四、Topics 标签（英文，16 个）

位置：About 面板 → **Topics**（点齿轮后可添加，每个标签单独输入一次、回车确认）

```text
windows
rust
voice-input
speech-to-text
asr
speech-recognition
input-method
dictation
push-to-talk
hotkey
websocket
streaming
egui
qwen
doubao
tencent-cloud
```

> 说明：Topics 只能用小写字母、数字和连字符（**不支持中文**）。上面 16 个都合法。
> 嫌多的话，最少留这 6 个：`windows` `rust` `voice-input` `speech-to-text` `asr` `push-to-talk`

---

## 五、Release 标题与正文（英文）

位置：GitHub → **Releases** → **Draft a new release**
Tag：选 `v1.0.0` ｜ Release title 填下面第一条 ｜ 正文填下面那一整块

```text
BBVoxi 1.0.0 - First Stable Release
```

```markdown
**Hold a hotkey, speak, release - the text lands at your cursor.**

BBVoxi is a tray-resident Windows voice input method written in Rust. No extra window, no clipboard tricks: it types the recognized text straight into whatever app you are using.

## Highlights

- **Push-to-talk, unlimited length** - default `Ctrl + 1`, rebindable to any `Ctrl / Alt / Shift` + letter, digit, `F1`-`F12` or space
- **Live incremental typing** - partial results are typed as you speak, then reverted and retyped when the recognizer corrects itself (longest-common-prefix diff)
- **Cursor injection without the clipboard** - `SendInput` + `KEYEVENTF_UNICODE`, so whatever you were copying stays intact
- **Three streaming ASR providers, switchable** - Alibaba Cloud Qwen (DashScope), Volcengine Doubao, Tencent Cloud ASR
- **Test mode never types outside the app** - sandbox your credentials and mic before going live
- **Tray-first, close means minimize** - autostart optional, cleanly revertible from the settings window
- **Single file, no runtime** - statically linked PE, ~8 MB, no VC++ redistributable, no WebView2

## Install

1. Download `BBVoxi-1.0.0.exe` below.
2. Double-click it. A tray icon appears; the settings window opens on first run.
3. Pick a provider and paste your own API key (Tencent Cloud needs the App ID / Secret ID / Secret Key triple).
4. Click "Test (5s)", say a sentence, then **Save**.

## Requirements

- Windows 10 1809+ / Windows 11, x86_64
- A microphone (any device the system can see; audio is resampled to 16 kHz mono internally)
- Internet access to your chosen ASR provider
- Language: Chinese recognition only in 1.0.0

## Notes

- Admin-elevated windows cannot receive injected input (Windows UIPI). Use a normal-privilege window.
- Tencent's `Hy-ASR-3.0-preview` engine accepts at most 60 s of audio per session; Qwen and Doubao have no such limit.
- Your API keys are stored locally at `%APPDATA%\BBVoxi\config.json`. **No analytics, no telemetry, no upload except the audio you record, sent only to the provider you selected.**

## Verify

```powershell
Get-FileHash .\BBVoxi-1.0.0.exe -Algorithm SHA256
```

> SHA-256: (把你的实际值填在这里，就是 `docs\RELEASE_v1.0.0.md` 第 18 行那个)

Full notes: see `docs/RELEASE_v1.0.0.md`.

**License:** MIT
```

---

## 六、Release 标题与正文（中文版 · 精简版）

> **内容更完整的详细版在另一份文件**：[`GITHUB发布文案-1.0.0详细版.md`](./GITHUB发布文案-1.0.0详细版.md)
> 想发一份信息量充足的正式 Release（含七轮迭代的真实 Bug 分析、完整功能清单、隐私说明、已知限制），**用那一份**；下面这份是精简版。

```text
BBVoxi 1.0.0 —— 首发正式版
```

```markdown
**按住快捷键说话，松开即输入到光标处。**

BBVoxi 是一个常驻系统托盘的 Windows 语音输入法，用 Rust 原生实现。没有多余窗口，不碰剪贴板：识别出的文字直接打进你正在用的程序里。

## 功能亮点

- **按住说话，不限时长** —— 默认 `Ctrl + 1`，可改成任意 `Ctrl / Alt / Shift` + 字母、数字、`F1`~`F12` 或空格
- **边说边出字** —— 识别中间结果实时写入光标处；识别被修正时自动回退重打（最长公共前缀差分）
- **不碰剪贴板** —— 走 `SendInput` + `KEYEVENTF_UNICODE` 逐字符注入，你正在复制的内容不会被打断
- **三家流式识别可切换** —— 阿里云百炼（千问）、火山引擎（豆包）、腾讯云 ASR
- **测试模式永不外打** —— 先验证凭据与麦克风，结果只显示在窗口里
- **托盘常驻，关窗即最小化** —— 开机自启可选，随时能干净关闭
- **单文件免安装** —— 静态链接 PE，约 8 MB，不需要 VC++ 运行库，不需要 WebView2

## 安装

1. 下载下方的 `BBVoxi-1.0.0.exe`。
2. 双击运行：托盘出现图标，首次运行会自动弹出设置窗。
3. 选择服务商并填入**你自己的** API Key（腾讯云需填 App ID / Secret ID / Secret Key 三元组）。
4. 点「测试识别（5 秒）」说一句话，确认能出字后点**保存**。

## 系统要求

- Windows 10 1809 及以上 / Windows 11，x86_64
- 麦克风（任意系统能识别的输入设备；程序内部会重采样到 16 kHz 单声道）
- 能访问你所选服务商的网络
- 1.0.0 仅支持中文识别

## 已知限制

- 以管理员权限运行的窗口无法接收注入文字（Windows UIPI 限制），请用普通权限窗口。
- 腾讯 `Hy-ASR-3.0-preview` 引擎单次最多 60 秒音频；千问与豆包无此时限。
- API Key 本地明文保存在 `%APPDATA%\BBVoxi\config.json`。**无统计、无遥测；除了你录制的语音（只发给你自己选择的服务商），不上传任何东西。**

## 校验下载完整性

```powershell
Get-FileHash .\BBVoxi-1.0.0.exe -Algorithm SHA256
```

> SHA-256：（填入你的实际值，即 `docs\RELEASE_v1.0.0.md` 第 18 行记录的那一串）

完整发布说明见 `docs/RELEASE_v1.0.0.md`。

**许可证：** MIT
```

---

## 七、发布前 30 秒自检清单

| # | 检查项 | 在哪看 |
|---|---|---|
| 1 | About 的 Description 已填（英文那条） | 仓库首页右上 About 齿轮 |
| 2 | Topics 已加（至少 5 个英文标签） | 同上 |
| 3 | README 顶部能正常显示界面截图 | 仓库首页 |
| 4 | 许可证显示为 MIT | 仓库首页右侧 |
| 5 | Release 的 Tag 选的是 `v1.0.0` | Releases → Draft a new release |
| 6 | Release 上传了 `dist\BBVoxi-1.0.0.exe` | 同上，Attach binaries |
| 7 | Release 正文里的 SHA-256 已填真实值 | 同上 |
| 8 | 仓库里**没有** exe / target / config.json | 仓库文件列表 |
