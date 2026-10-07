//! 自动更新：查 GitHub 最新发布 → 下载 exe → 核对指纹 → 替换自身 → 重启。
//!
//! 为什么是这套做法（而不是安装包 / 更新框架）：
//! 主人发布的就是**单个 exe**（GitHub releases 里就一个 `BBVoxi-x.y.z.exe`），
//! 所以不需要打包格式；而 Windows 不允许覆盖**正在运行**的 exe，于是
//! 「下载成一个临时文件 → 核对指纹 → 旧文件让位、新文件就位 → 重启」是最省事
//! 也最可靠的一条路（让位那一步交给成熟的 `self-replace` 做）。
//! 那个"临时文件"就放在程序自己所在目录（`BBVoxi-<版本>.new.exe`）：
//! 替换靠 `rename` 就位，**跨盘会失败**，所以不能丢到系统临时目录去；
//! 装好或失败之后都会立刻删掉，不留垃圾。
//!
//! 这里**只在你点按钮时**才联网：不做后台轮询、不写注册表、不碰系统目录。

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 主人的仓库（查最新发布用）
pub const REPO: &str = "HaiSeaman/BBVoxi";

/// 查"最新发布"的接口地址。
///
/// 由 `REPO` 拼出来，而不是再手写一份常量：两处各写一份的话，哪天改了仓库名
/// 只改一处，更新功能就会整体失效，而现象只是"检查更新失败"——最难查的那种。
fn api_latest() -> String {
    format!("https://api.github.com/repos/{REPO}/releases/latest")
}

/// 出错时给主人的"手动下载"去处（GitHub 会自动跳到最新版）
pub fn releases_page() -> String {
    format!("https://github.com/{REPO}/releases/latest")
}

/// 查版本 / 建连的超时。国内直连 GitHub 实测 1.5~4 秒，这里留足余量。
const TIMEOUT_CONNECT: Duration = Duration::from_secs(10);
const TIMEOUT_QUERY: Duration = Duration::from_secs(20);
/// 整个下载过程的墙钟上限（**每一次尝试**，代理一次、直连一次）。
///
/// 为什么是 300 秒：主人这台机器上实测两条路的速度分别是走代理 663KB/s（8.4MB 约
/// 13 秒）和直连 68KB/s（约 123 秒）—— 300 秒对最慢的那条也留了两倍余量。
/// 上限主要防的是"连接半死不活"：`try_agents` 会依次试代理与直连，两次都按这个
/// 上限算，最坏 10 分钟，界面全程有进度可看；再长就不该让主人干等了。
const TIMEOUT_DOWNLOAD: Duration = Duration::from_secs(300);
/// 单次下载的体积上限：对方真返回一个几十 GB 的东西时，别把主人的磁盘填满
const MAX_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024;
/// GitHub 的下载会重定向到 release-assets.githubusercontent.com，最多跟几次
const MAX_REDIRECTS: u32 = 10;
/// 重启后等多久确认"新进程确实活着"（见 `restart`）。
///
/// 500 毫秒足够暴露"exe 坏了/缺 DLL/一启动就 panic"这几类问题；再长就只是让
/// 主人白等（正常情况下新实例这会儿已经在建窗口了）。
const RESTART_SMOKE_WAIT: Duration = Duration::from_millis(500);

/// 一次线上发布里我们需要的那点信息
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// 去掉 `v` 前缀的版本号，如 `1.5.0`
    pub version: String,
    /// 发布说明（GitHub release 正文，原样带出来给主人看）
    pub notes: String,
    /// 资产文件名，如 `BBVoxi-1.5.0.exe`
    pub asset_name: String,
    /// 下载地址（GitHub 接口给的直链，跟着它走就行）
    pub download_url: String,
    /// GitHub 公布的 SHA-256 指纹（`sha256:...`）；老资产可能没有，那就是 `None`
    pub digest: Option<String>,
}

/// 版本号比较：`latest` 比 `current` 新才算有更新。
///
/// 为什么不用字符串比较：`"1.10.0" < "1.9.0"`，那样主人发 1.10.0 时更新会**不认**。
/// 按段取数字比；段数不同按 0 补齐（`1.4` == `1.4.0`）；带后缀的按语义化版本
/// 的规矩排在正式版**前面**（`1.5.0-rc` < `1.5.0`）—— 主人在用的都是纯数字版本，
/// 这条只是别让带后缀的 tag 判反。
/// 解析不出来（tag 写成了中文、乱码）一律当"没有更新"：宁可少提示一次，
/// 也不能拿一个读不懂的版本号去覆盖主人正在用的程序。
pub fn is_newer(current: &str, latest: &str) -> bool {
    let (Some(cur), Some(new)) = (parse_version(current), parse_version(latest)) else {
        return false;
    };
    new > cur
}

/// 解析成可比较的形式：`(各段数字, 是不是正式版)`。
///
/// 按语义化版本的规矩：
/// - 段数不固定（`1.4` == `1.4.0` == `1.4.0.0`）：先按段取数字，再把**末尾的 0 去掉**，
///   这样 `[1,4]` 和 `[1,4,0]` 天然相等，元组比较就是对的；
/// - `+xxx` 是构建元数据，**不参与**比较（`1.5.0+a` == `1.5.0`）；
/// - `-rc` / `-beta` 是预发布后缀，**比同号的正式版旧**（`1.5.0-rc` < `1.5.0`）。
///   第二项用"是不是正式版"而不是"有没有后缀"，就是为了让比较方向正确
///   —— 这里写反过一次，被 `version_comparison_handles_prefix_and_prerelease` 抓住。
///
/// 以前只取前三段（`take(3)`），于是 `1.5.0.1` 会被当成 `1.5.0` —— 如果主人哪天
/// 用四段版本号发版，更新会**永远说"已是最新"**，而界面上看不出任何异常。
fn parse_version(text: &str) -> Option<(Vec<u64>, bool)> {
    let text = text.trim();
    let text = text.strip_prefix('v').unwrap_or(text);
    if text.is_empty() {
        return None;
    }
    let text = text.split('+').next().unwrap_or(text);
    let (core, is_release) = match text.split_once('-') {
        Some((core, _)) => (core, false),
        None => (text, true),
    };
    let mut nums: Vec<u64> = Vec::new();
    for part in core.split('.') {
        // 段数不限，但每一段都必须是数字：`1.5.x` 这种读不懂的 tag 一律拒收，
        // 绝不用一个读不懂的版本号去覆盖主人正在用的程序
        nums.push(part.trim().parse().ok()?);
    }
    if nums.is_empty() {
        return None;
    }
    while nums.len() > 1 && nums.last() == Some(&0) {
        nums.pop();
    }
    Some((nums, is_release))
}

/// 从 GitHub 的 `releases/latest` 返回体里取出我们要的东西。
///
/// 抽成纯函数是为了能拿**真实返回结构**钉住它：字段名写错、资产挑错，
/// 都会变成"按钮点了没反应"这种最难查的毛病。
pub fn parse_release(json: &str) -> Result<Release> {
    let v: serde_json::Value = serde_json::from_str(json).context("GitHub 返回的不是合法 JSON")?;
    let tag = v["tag_name"].as_str().unwrap_or("").trim().to_string();
    if tag.is_empty() {
        bail!("GitHub 返回里没有版本号（tag_name）");
    }
    let version = tag.strip_prefix('v').unwrap_or(&tag).to_string();
    let notes = v["body"].as_str().unwrap_or("").to_string();

    // 挑资产：只要 .exe（主人上传的就是单个 exe）。挑不到就如实报错 ——
    // 不能让界面显示一个"点不动的更新"。
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let exe = assets
        .iter()
        .find(|a| {
            a["name"]
                .as_str()
                .is_some_and(|n| n.to_ascii_lowercase().ends_with(".exe"))
        })
        .context("这个版本里没有 .exe 资产（可能只传了源码包），请去发布页手动下载")?;

    let asset_name = exe["name"].as_str().unwrap_or("").to_string();
    let download_url = exe["browser_download_url"].as_str().unwrap_or("").to_string();
    if download_url.is_empty() {
        bail!("GitHub 没给出「{asset_name}」的下载地址");
    }
    Ok(Release {
        version,
        notes,
        asset_name,
        download_url,
        digest: exe["digest"].as_str().map(|s| s.to_string()),
    })
}

/// GitHub 的 `digest` 是 `"sha256:<十六进制>"`。只认 sha256、其余一律当"没有指纹"
/// （拿一个自己不认识的算法当校验依据，等于没有校验）。
pub fn parse_sha256(digest: &str) -> Option<String> {
    let (algo, hex) = digest.split_once(':')?;
    if !algo.eq_ignore_ascii_case("sha256") {
        return None;
    }
    let hex = hex.trim().to_ascii_lowercase();
    if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(hex)
}

/// 发布说明太长时给界面用的摘要（按**字符**截，别按字节切中文）
pub fn notes_summary(notes: &str, max_chars: usize) -> String {
    let text = notes.trim();
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// 算一个文件的 SHA-256（小写十六进制）
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("打不开 {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 预检：这个目录能不能写文件。
///
/// 为什么要在下载**之前**查：程序要是放在 `C:\Program Files` 这类受保护目录，
/// 等 8MB 下完才告诉你"换不上去"最气人。提前一句话说清，主人换目录就行。
pub fn precheck_dir(dir: &Path) -> Result<()> {
    let probe = dir.join(format!("bbvoxi-write-test-{}.tmp", std::process::id()));
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => bail!("程序所在目录不能写（{e}）：请把 BBVoxi 放到你自己能写的目录再更新"),
    }
}

// —— 以下是真正碰网络/磁盘的部分（阻塞调用，必须放在后台线程里跑）——

/// 建一个 HTTP 客户端。
///
/// `direct = true` 时**明确不走代理**；否则跟随 Windows 的系统代理设置
/// （靠 Cargo.toml 里开的 `win-system-proxy` 特性）。为什么要分两种：主人这台机器
/// 实测「走系统代理」比「直连」快 **10 倍**（663KB/s vs 68KB/s），所以优先走代理；
/// 但代理软件关掉之后系统设置可能还开着，那时直连反而是唯一能用的路。
fn build_agent(timeout: Duration, direct: bool) -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder()
        .timeout_connect(Some(TIMEOUT_CONNECT))
        .timeout_global(Some(timeout))
        .max_redirects(MAX_REDIRECTS)
        .user_agent(concat!("BBVoxi/", env!("CARGO_PKG_VERSION")));
    if direct {
        builder = builder.proxy(None);
    }
    builder.build().new_agent()
}

/// 依次尝试的客户端：先跟随系统代理，失败再直连。
///
/// 为什么要退一步：代理开着时快 10 倍，可**代理软件一关、系统设置还开着**的话，
/// 请求会直接连不上（连接被拒）。退一步直连虽然慢，但"慢"总好过"更新不了"。
fn agents(timeout: Duration) -> [(&'static str, ureq::Agent); 2] {
    [
        ("系统代理", build_agent(timeout, false)),
        ("直连", build_agent(timeout, true)),
    ]
}

/// 把网络错误翻成主人看得懂的话。
///
/// 为什么值得专门翻：直接抛 `ureq` 的英文错误，主人只会看到一串
/// "timeout: global" 之类的字，既不知道是断网还是被拦，也不知道下一步干嘛。
fn friendly_net_error(e: &ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(403) => "GitHub 拒绝了请求（403：多半是短时间问得太勤）".into(),
        ureq::Error::StatusCode(404) => "GitHub 上找不到这个仓库或还没有发布版本（404）".into(),
        ureq::Error::StatusCode(code) => format!("GitHub 返回了状态码 {code}"),
        ureq::Error::Timeout(detail) => format!("等待超时（{detail}；网络太慢或被拦）"),
        ureq::Error::HostNotFound => "连不上 github.com（域名解析不了，检查网络/DNS）".into(),
        ureq::Error::ConnectionFailed => "连不上 GitHub（可能被当前网络环境拦截）".into(),
        other => format!("{other}"),
    }
}

/// 依次用"系统代理 → 直连"两个客户端试一遍，第一个成功的就用它。
///
/// 为什么要收成一处：查版本和下载都要走这条"先代理后直连"的路，
/// 分开写迟早会只改一处（例如只给下载加了超时，查版本没有）。
fn try_agents<T>(
    what: &str,
    timeout: Duration,
    mut attempt: impl FnMut(&ureq::Agent) -> Result<T>,
) -> Result<T> {
    let mut last: Option<anyhow::Error> = None;
    for (how, agent) in agents(timeout) {
        match attempt(&agent) {
            Ok(v) => return Ok(v),
            Err(e) => {
                crate::log::log(format!("{what}失败（{how}）：{e:#}"));
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("{what}失败")))
}

/// 查线上最新发布（**阻塞**，调用方负责放到后台线程里）。
pub fn latest() -> Result<Release> {
    try_agents("查最新版本", TIMEOUT_QUERY, fetch_latest)
}

/// 用给定的客户端查一次
fn fetch_latest(agent: &ureq::Agent) -> Result<Release> {
    let mut resp = agent
        .get(api_latest())
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| anyhow!("{}", friendly_net_error(&e)))?;
    let body = resp
        .body_mut()
        .read_to_string()
        .context("读取 GitHub 的返回内容失败")?;
    parse_release(&body)
}

/// 程序自己所在的那一级目录
fn exe_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("定位当前程序路径失败")?;
    exe.parent()
        .map(|p| p.to_path_buf())
        .context("当前程序没有所在目录？")
}

/// 下载新版本 → 边下边算 SHA-256 → 和 GitHub 公布的指纹核对。
/// 成功返回下载好的文件路径（就放在程序所在目录，保证和 exe 同一个盘）。
///
/// 指纹对不上就**删掉并报错**：装一个内容不对的程序，比不更新糟糕得多。
/// 老资产没有指纹时如实记一条日志，但不拦着更新（总不能因为对方没给指纹就不让升级）。
/// 下载到本地的临时文件名。
///
/// 版本号来自 GitHub 的 tag（`tag_name`），虽然只有仓库主人自己能建 tag，
/// 但要是哪天建出一个带路径分隔符的 tag（`1.5.0-x/../../evil`），直接拼进
/// 文件名就可能写到别的目录去 —— 拼路径前先洗干净，只留字母数字和 `.` `-` `_`。
fn download_file_name(version: &str) -> String {
    let safe: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // 洗完之后可能只剩点号（`..`），那样还是危险，兜底成 `update`
    let safe = if safe.chars().all(|c| c == '.') {
        "update".to_string()
    } else {
        safe
    };
    format!("BBVoxi-{safe}.new.exe")
}

pub fn download_and_verify(
    release: &Release,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    let dir = exe_dir()?;
    precheck_dir(&dir)?;
    let target = dir.join(download_file_name(&release.version));

    // 先跟随系统代理（实测快 10 倍），连不上再退回直连
    let path = match try_agents("下载", TIMEOUT_DOWNLOAD, |agent| {
        fetch_to_file(agent, release, &target, &mut on_progress)
    }) {
        Ok(()) => target.clone(),
        Err(e) => {
            let _ = std::fs::remove_file(&target); // 半截文件不许留
            return Err(e);
        }
    };

    // 校验指纹。读文件失败也要把这份下载删掉：留着它只会让主人下次在目录里
    // 看到一个来路不明的 `BBVoxi-x.y.z.new.exe`。
    let actual = match sha256_file(&path) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
    };
    match release.digest.as_deref().and_then(parse_sha256) {
        Some(want) if want == actual => Ok(path),
        Some(want) => {
            let _ = std::fs::remove_file(&path);
            bail!("下载下来的文件和 GitHub 公布的指纹对不上，已删除（期望 {want}，实际 {actual}）")
        }
        None => {
            // 分清两种情况再记日志：以前一律说"没有公布指纹"，可实际上对方可能
            // 公布了一个我们不认识的算法 —— 排障的人会去找一个其实存在的东西。
            match release.digest.as_deref() {
                Some(d) => crate::log::log(format!(
                    "{} 公布的指纹不是 sha256（{d}），本次跳过校验",
                    release.asset_name
                )),
                None => crate::log::log(format!(
                    "{} 没有公布指纹，本次跳过校验（文件已存到 {}）",
                    release.asset_name,
                    path.display()
                )),
            }
            Ok(path)
        }
    }
}

/// 用给定的客户端把资产下载到 `target`（边下边报进度）。失败时保证不留半截文件。
fn fetch_to_file(
    agent: &ureq::Agent,
    release: &Release,
    target: &Path,
    on_progress: &mut impl FnMut(u64, u64),
) -> Result<()> {
    let mut resp = agent
        .get(&release.download_url)
        .header("Accept", "application/octet-stream")
        .call()
        .map_err(|e| anyhow!("{}", friendly_net_error(&e)))?;
    let total = resp.body().content_length().unwrap_or(0);
    if total > MAX_DOWNLOAD_BYTES {
        bail!("这个更新包有 {total} 字节，超过上限，已放弃（可能下错了东西）");
    }

    let mut file =
        std::fs::File::create(target).with_context(|| format!("创建 {} 失败", target.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    // reader 借着 resp，所以 resp 必须活到循环结束
    let mut reader = resp.body_mut().as_reader();
    let result = (|| -> Result<()> {
        loop {
            let n = reader.read(&mut buf).context("下载中断（网络断了）")?;
            if n == 0 {
                break;
            }
            done += n as u64;
            if done > MAX_DOWNLOAD_BYTES {
                bail!("下载超过上限，已中止");
            }
            file.write_all(&buf[..n]).context("写入文件失败")?;
            on_progress(done, total);
        }
        file.flush().context("把内容写进磁盘失败")?;
        Ok(())
    })();
    drop(file);
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(target);
            // 带上"下到多少字节"：超时/断流时，主人和日志一眼就能看出是
            // "一点没动"（网络不通）还是"下到一半断了"（网络不稳）—— 处置完全不同
            Err(e.context(format!("已经下到 {done} 字节")))
        }
    }
}

/// 把下载好的新版本换上去。**换完必须重启才生效**（当前进程跑的还是旧代码）。
///
/// Windows 不允许覆盖正在运行的 exe，所以这一步交给 `self_replace`：
/// 它按标准姿势做「旧文件改名让位 → 新文件原子就位 → 本进程退出后由它的小助手
/// 删掉旧文件」。它要求程序所在目录**可写**（`precheck_dir` 已经提前查过）。
pub fn install(downloaded: &Path) -> Result<()> {
    let outcome = self_replace::self_replace(downloaded)
        .context("替换程序文件失败（可能被杀软/别的程序占用，或目录没有写权限）");
    // 下载的那份**不管成没成都删掉**：成了的话内容已经拷进去了，留着只会让主人
    // 下次在目录里看到一个多余的 `BBVoxi-x.y.z.new.exe`；没成的话它也没用了
    // （下次更新会重新下）。
    let _ = std::fs::remove_file(downloaded);
    outcome
}

/// 起一个新进程跑刚装上的新版本；调用方随后应当立刻退出本进程。
///
/// `--updated` 是关键：告诉新实例"你是接班人，不是重复启动" —— 否则它会去抢
/// 单实例锁、发现老进程还在，就发个「唤醒」消息然后自己退出（表现成"重启后
/// 什么都没发生"）。`--settings` 让设置窗直接露出来，主人一眼能看到新版本号。
///
/// 起完**先确认它还活着**再交回请求：`spawn` 成功只说明进程被创建了，
/// 新 exe 要是坏的（下载被截断、杀软删了半个文件），它会立刻退出 ——
/// 那时如果老进程也退了，主人看到的就是"程序整个消失"。所以这里等一小会儿，
/// 发现新进程已经死了就把错误抛回去，让老进程继续好好活着。
pub fn restart() -> Result<()> {
    let exe = std::env::current_exe().context("定位当前程序路径失败")?;
    let mut child = std::process::Command::new(&exe)
        .args(["--updated", "--settings"])
        .spawn()
        .context("启动新版本失败，请手动双击 BBVoxi.exe")?;
    std::thread::sleep(RESTART_SMOKE_WAIT);
    match child.try_wait() {
        Ok(None) => Ok(()),
        Ok(Some(status)) => bail!(
            "新版本刚启动就退出了（{status}）：更新可能没成功，请点「手动下载」重新下一份"
        ),
        Err(e) => bail!("无法确认新版本是否启动（{e}）：请手动双击 BBVoxi.exe 试试"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 版本比较必须按**数字**比：字符串比较会把 1.10.0 判成比 1.9.0 旧，
    /// 于是主人发了 1.10.0 之后，更新按钮永远说"已是最新"，人却蒙在鼓里。
    #[test]
    fn version_comparison_is_numeric_not_lexicographic() {
        assert!(is_newer("1.4.0", "1.5.0"));
        assert!(
            is_newer("1.9.0", "1.10.0"),
            "1.10.0 比 1.9.0 新（按字符串比会判反）"
        );
        assert!(is_newer("1.4.0", "1.4.1"), "补丁版也是新版");
        assert!(!is_newer("1.4.0", "1.4.0"), "同一个版本不算有新版本");
        assert!(
            !is_newer("1.5.0", "1.4.0"),
            "线上比本机旧（回滚过）时，不能说有新版本，否则会把人降级"
        );
        assert!(!is_newer("1.4.0", "1.4"), "少写的段按 0 补齐：1.4 == 1.4.0");
    }

    /// tag 可能带 `v` 前缀；带 `-rc/-beta` 的按语义化版本排在正式版之前
    #[test]
    fn version_comparison_handles_prefix_and_prerelease() {
        assert!(is_newer("1.4.0", "v1.5.0"), "v1.5.0 也要认");
        assert!(!is_newer("1.5.0", "1.5.0-rc"), "正式版比自己的 rc 新，不能提示降级");
        assert!(is_newer("1.5.0-rc", "1.5.0"), "rc 升到正式版要能提示");
        assert!(
            !is_newer("1.4.0", "最新版"),
            "读不懂的 tag 一律当没有更新：绝不能拿它去覆盖主人正在用的程序"
        );
        assert!(!is_newer("1.4.0", ""), "空 tag 不能崩，也不能当成有更新");
        assert!(!is_newer("乱七八糟", "1.5.0"), "本机版本读不懂时同样不许动");
    }

    /// 解析 GitHub 的真实返回结构（字段名照抄线上，指纹也是 1.4.0 的真实值）
    #[test]
    fn parses_a_real_github_release_payload() {
        let json = r#"{
            "tag_name": "1.4.0",
            "name": "1.4.0",
            "draft": false,
            "prerelease": false,
            "body": "**Windows 按住说话的语音输入法**\n本版做三件事：设置界面页签化。",
            "assets": [{
                "name": "BBVoxi-1.4.0.exe",
                "content_type": "application/x-msdownload",
                "state": "uploaded",
                "size": 8394752,
                "digest": "sha256:BDF2A709B0E1A3091BC8489D01ECDD3FA420D58D481EA027B5035DAB9CDAB965",
                "browser_download_url": "https://github.com/HaiSeaman/BBVoxi/releases/download/1.4.0/BBVoxi-1.4.0.exe"
            }]
        }"#;
        let r = parse_release(json).expect("真实返回必须能解析");
        assert_eq!(r.version, "1.4.0");
        assert_eq!(r.asset_name, "BBVoxi-1.4.0.exe");
        assert_eq!(
            r.download_url,
            "https://github.com/HaiSeaman/BBVoxi/releases/download/1.4.0/BBVoxi-1.4.0.exe"
        );
        assert_eq!(
            r.digest.as_deref(),
            Some("sha256:BDF2A709B0E1A3091BC8489D01ECDD3FA420D58D481EA027B5035DAB9CDAB965")
        );
        assert!(r.notes.contains("语音输入法"), "更新说明要带出来给主人看");
    }

    /// 带 v 前缀的 tag：存进 Release 的版本号要去掉前缀（界面显示用）
    #[test]
    fn release_version_strips_the_v_prefix() {
        let json = r#"{"tag_name":"v1.5.0","body":"","assets":[
            {"name":"BBVoxi-1.5.0.exe","browser_download_url":"https://x/y.exe"}]}"#;
        assert_eq!(parse_release(json).unwrap().version, "1.5.0");
    }

    /// 发布里没有 exe（只传了源码包）→ 必须明确报错，
    /// 不能让界面显示一个"点了没反应"的更新
    #[test]
    fn release_without_exe_asset_is_an_error() {
        let json = r#"{"tag_name":"1.5.0","body":"","assets":[
            {"name":"source.tar.gz","browser_download_url":"https://x/src.gz"}]}"#;
        let err = parse_release(json).unwrap_err().to_string();
        assert!(err.contains("exe"), "报错要说清是没有 exe 资产：{err}");
    }

    /// 坏 JSON / 缺版本号 / 缺下载地址：一律报错，绝不许 panic
    #[test]
    fn malformed_payloads_error_instead_of_panicking() {
        assert!(parse_release("这不是 JSON").is_err());
        assert!(parse_release("{}").is_err(), "没有 tag_name 要报错");
        let no_url = r#"{"tag_name":"1.5.0","assets":[{"name":"BBVoxi-1.5.0.exe"}]}"#;
        assert!(parse_release(no_url).is_err(), "没有下载地址要报错");
    }

    /// GitHub 的指纹字段是 `sha256:<十六进制>`；别的算法**不能**当校验依据
    #[test]
    fn digest_parsing_only_trusts_sha256() {
        assert_eq!(
            parse_sha256("sha256:ABCD"),
            Some("abcd".into()),
            "统一转小写，方便和本机算出来的比"
        );
        assert_eq!(parse_sha256("sha512:abcd"), None, "不认识的算法不能当作通过");
        assert_eq!(parse_sha256("sha256:"), None, "空指纹不算数");
        assert_eq!(parse_sha256("sha256:xyz"), None, "不是十六进制就不算数");
        assert_eq!(parse_sha256("没冒号"), None);
    }

    /// 本机算的 SHA-256 必须和业界标准向量一致（"abc" 的结果是公开测试向量）
    #[test]
    fn sha256_matches_the_known_test_vector() {
        let dir = std::env::temp_dir().join(format!("bbvoxi_sha_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("abc.bin");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 改一个字节就得变：不然"校验"形同虚设
        std::fs::write(&f, b"abd").unwrap();
        assert_ne!(
            sha256_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 更新说明按**字符**截（按字节切中文会切出乱码，界面显示"…"收尾）
    #[test]
    fn notes_summary_truncates_by_characters() {
        let long = "这是一段很长的更新说明".repeat(50);
        let s = notes_summary(&long, 20);
        assert_eq!(s.chars().count(), 21, "20 个字符 + 一个省略号");
        assert!(s.ends_with('…'));
        let short = "简短说明";
        assert_eq!(notes_summary(short, 20), "简短说明", "短的不该加省略号");
    }

    /// 预检能写 / 不能写两种结果都要对：这是"提前告诉主人换目录"的依据
    #[test]
    fn writable_precheck_reports_both_outcomes() {
        let dir = std::env::temp_dir().join(format!("bbvoxi_pre_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(precheck_dir(&dir).is_ok(), "能写的目录必须通过");
        assert!(
            std::fs::read_dir(&dir).unwrap().next().is_none(),
            "预检不许留下垃圾文件"
        );
        let missing = dir.join("这个目录不存在").join("也不存在");
        assert!(precheck_dir(&missing).is_err(), "写不进去的目录必须报错");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 仓库地址、接口地址、下载页三处必须指向同一个仓库。
    ///
    /// 为什么值得测：写错一个字母，现象只是"检查更新失败"，主人根本看不出是
    /// 地址写错了 —— 而这类错字在复制粘贴时最容易发生。
    #[test]
    fn urls_point_at_the_same_repo() {
        assert_eq!(REPO, "HaiSeaman/BBVoxi");
        let api = api_latest();
        assert!(
            api.starts_with(&format!("https://api.github.com/repos/{REPO}/releases")),
            "接口地址与仓库不一致：{api}"
        );
        let page = releases_page();
        assert!(
            page.starts_with(&format!("https://github.com/{REPO}/releases")),
            "下载页与仓库不一致：{page}"
        );
    }

    /// 下载临时文件名必须洗干净：版本号来自 GitHub 的 tag，一个带路径分隔符的
    /// 怪 tag（`1.5.0-x/../../evil`）直接拼进文件名就可能写到别的目录去。
    /// 断言的是**真正的安全性质**：拼出来的路径父目录必须还是原目录，
    /// 而且不含 Windows 文件名非法字符（不然 `File::create` 会直接失败）。
    #[test]
    fn download_file_name_is_sanitized() {
        assert_eq!(download_file_name("1.5.0"), "BBVoxi-1.5.0.new.exe");
        let base = std::path::Path::new("C:\\somewhere");
        for weird in [
            "1.5.0-x/../../evil",
            "1.5.0\\..\\..\\evil",
            "..",
            "../",
            "a:b|c?d*e\"f<g>h",
        ] {
            let name = download_file_name(weird);
            let full = base.join(&name);
            assert_eq!(
                full.parent(),
                Some(base),
                "文件名「{name}」让路径跑出目录了：{}",
                full.display()
            );
            assert!(name.ends_with(".new.exe"), "{name}");
            for bad in ['/', '\\', ':', '*', '?', '"', '<', '>', '|'] {
                assert!(!name.contains(bad), "「{name}」含 Windows 非法字符 {bad}");
            }
        }
    }

    /// 真机验证（默认不跑）：**代理开着但代理软件没运行**时，更新要能退回直连。
    ///
    /// 为什么专门验这条：主人这台机器的系统代理指向 `127.0.0.1:10808`（实测比直连
    /// 快 10 倍，所以默认优先走它），可代理软件一关、系统设置还开着的话，请求会
    /// 直接连不上。这里用一个"死代理"（127.0.0.1:9，没人监听）把那种情形演出来，
    /// 验证直连这条路仍然能查到版本。
    #[test]
    #[ignore = "需要联网；手动跑：cargo test dead_proxy -- --ignored --nocapture"]
    fn dead_proxy_falls_back_to_direct() {
        let dead = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(3)))
            .timeout_global(Some(Duration::from_secs(6)))
            .proxy(ureq::Proxy::new("http://127.0.0.1:9").ok())
            .build()
            .new_agent();
        assert!(fetch_latest(&dead).is_err(), "死代理上不该成功");

        let direct = build_agent(TIMEOUT_QUERY, true);
        let r = fetch_latest(&direct).expect("直连必须还能查到版本（这就是兜底的意义）");
        println!("死代理失败、直连成功：线上最新 {}", r.version);
    }

    /// 真机验证（默认不跑，手动跑：`cargo test real_update_path -- --ignored --nocapture`）：
    /// 真去 GitHub 查一次最新发布、**真把 exe 下载下来**、真核对指纹、真删掉。
    ///
    /// 为什么要它：单测只能证明解析和比较的逻辑对，证明不了"主人这台机器能不能
    /// 真的连上 GitHub、能不能跟着重定向把 8MB 拉回来、算出来的指纹对不对"。
    /// （替换自身与重启那两步不能在测试里跑 —— 那会把正在跑测试的进程换掉。）
    #[test]
    #[ignore = "需要联网；手动跑：cargo test real_update_path -- --ignored --nocapture"]
    fn real_update_path_check_download_and_verify() {
        let rel = latest().expect("查线上最新发布");
        println!(
            "线上最新：{}  资产 {}  指纹 {:?}  说明 {} 字",
            rel.version,
            rel.asset_name,
            rel.digest,
            rel.notes.chars().count()
        );
        assert!(!rel.version.is_empty());
        let mut last = 0u64;
        let path = download_and_verify(&rel, |done, total| {
            if done / (1024 * 1024) != last / (1024 * 1024) {
                println!("  下载中 {done}/{total} 字节");
            }
            last = done;
        })
        .expect("下载并核对指纹");
        let size = std::fs::metadata(&path).unwrap().len();
        println!("下载完成并通过指纹核对：{size} 字节 → {}", path.display());
        assert!(size > 1_000_000, "下回来的东西太小了，不像一个 exe");
        std::fs::remove_file(&path).unwrap();
    }
}

