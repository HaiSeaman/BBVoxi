//! 边说话边打字。
//!
//! 语音识别的中间结果会不断被修正（同音字、标点、断句），所以不能简单地"来一段打一段"。
//! 成熟输入法的做法是维护两个状态：
//!   - `sent`：已经打进目标程序的文本
//!   - `desired`：当前期望的文本（已定稿部分 + 当前句的中间结果）
//! 每次中间结果更新时，取 `sent` 与 `desired` 的最长公共前缀，
//! 把 `sent` 多出来的尾巴用退格删掉，再补打新的尾巴。
//! 这样：文本变长时只补打增量，识别结果被修正时也只回退/重打被改动的部分。

use anyhow::{anyhow, Result};

use crate::{clipboard, injector};

/// sent → desired 需要做的编辑动作
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// 需要退格的字符数
    pub backspaces: usize,
    /// 需要补打的文本
    pub append: String,
}

impl Diff {
    pub fn is_noop(&self) -> bool {
        self.backspaces == 0 && self.append.is_empty()
    }
}

pub fn diff(sent: &str, desired: &str) -> Diff {
    let sent_chars: Vec<char> = sent.chars().collect();
    let desired_chars: Vec<char> = desired.chars().collect();

    let mut common = 0;
    while common < sent_chars.len()
        && common < desired_chars.len()
        && sent_chars[common] == desired_chars[common]
    {
        common += 1;
    }

    Diff {
        backspaces: sent_chars.len() - common,
        append: desired_chars[common..].iter().collect(),
    }
}

/// 决定本次到底要不要输入、输入什么。`None` = 一个字符都不输入。
///
/// `blocked`（= `LiveTyper::blocked()`）覆盖三种"绝不能输入"的情形：
/// - 测试模式：结果只回显到设置窗，一个字符都不许往外写；
/// - 运行中被放弃（录音途中切走了窗口）：`sent` 记的是打在**旧窗口**里的字，
///   对它做退格会删掉新窗口里主人的内容 —— 这正是"退格不打错窗口"要防的事；
/// - 此前注入已失败：不反复重试、不重复报错。
fn plan(blocked: bool, sent: &str, desired: &str) -> Option<Diff> {
    if blocked {
        return None;
    }
    let planned = diff(sent, desired);
    (!planned.is_noop()).then_some(planned)
}

/// 取 `text` 里前 `committed` 个编码单元（按 `injector::type_text` 的口径）之后
/// 剩下的尾巴。逐字注入只进去一部分时，只有这段尾巴需要改走剪贴板补上 ——
/// 把整段重粘会和已经打进去的字重复。
///
/// 提交数正好落在一个字符中间（代理对被拆开）时无法表示半个字符，
/// 保守起见从那个字符开始整段重粘：宁可重复半个字形，也不悄悄丢字。
fn tail_after_units(text: &str, committed: usize) -> String {
    let mut used = 0usize;
    for (idx, ch) in text.char_indices() {
        if used >= committed {
            return text[idx..].to_string();
        }
        let n = if ch == '\r' { 0 } else { ch.len_utf16() };
        if used + n > committed {
            return text[idx..].to_string();
        }
        used += n;
    }
    String::new()
}

pub struct LiveTyper {
    sent: String,
    /// 是否边说话边打字（来自设置）
    pub enabled: bool,
    /// 注入失败的原因，失败后不再重试，避免反复报错
    pub failure: Option<String>,
    /// 此刻是否不在"按住快捷键时的那个窗口"里 —— 在别处时既不做实时输入，
    /// 收尾也一个字不打（见 `guard`）
    away: bool,
    /// 测试模式：结果只回显到设置窗，任何情况下都不注入（见 `muted`）
    muted: bool,
    /// 开始录音时的前台窗口。之后每次输入前都要拿当前窗口跟它核对
    watch: Option<isize>,
    /// 逐字注入被拒时是否改用剪贴板粘贴再试一次（见 `with_paste_fallback`）。
    /// 默认**关**：单测与 `muted` 都不该在无声无息中动主人的剪贴板。
    paste_fallback: bool,
}

impl LiveTyper {
    /// `watch` = 开始录音那一刻的前台窗口（`session::foreground_window()`）。
    pub fn new(enabled: bool, watch: Option<isize>) -> Self {
        Self {
            sent: String::new(),
            enabled,
            failure: None,
            away: false,
            muted: false,
            watch,
            paste_fallback: false,
        }
    }

    /// 挂上"注入被拒时改用剪贴板粘贴"这条兜底（来自设置）。
    ///
    /// 为什么做成链式方法而不是 `new` 的第三个参数：默认是**关**，既有调用点
    /// （含大量单测）因此一个字都不用改，也就不会有人不小心让单测去动真剪贴板。
    pub fn with_paste_fallback(mut self, on: bool) -> Self {
        self.paste_fallback = on;
        self
    }

    /// 测试模式专用：结果只回显到设置窗，**任何情况下都不注入一个字符**。
    ///
    /// 不能拿 `new(false, _)` 凑合：那个 `enabled=false` 的含义是「这次不边说话边打字」，
    /// 收尾的 `finish` 仍然要一次性把全文打出去（那正是关掉实时输入时的正常行为）。
    /// 测试模式需要的是更强的一条契约 —— 连收尾也不许输入，否则结果会打进
    /// 设置窗背后的那个程序里。
    pub fn muted() -> Self {
        Self {
            muted: true,
            ..Self::new(false, None)
        }
    }

    /// 本次会话是否完全禁止注入
    fn blocked(&self) -> bool {
        self.muted || self.away || self.failure.is_some()
    }

    /// 每次准备输入之前核对前台窗口：不在"按住快捷键时的那个窗口"里就整体停掉。
    ///
    /// 为什么把它做成「每次输入前必须上报当前窗口」的**必填参数**（而不是一个
    /// 可选的外部检查）：漏掉一次校验，补打和退格就会落到别的程序里。这个坑
    /// 已经踩过两次 —— 第一次是收尾没校验，第二次是「实时阶段校验了、收尾忘了」。
    /// 现在 `sync` / `finish` 的签名逼着调用方每次都报一次，漏不掉。
    ///
    /// 切走是**可恢复的**：主人切出去看一眼再切回来，`sent` 里记的仍然正是
    /// 那个窗口里的字，继续对它做差分完全安全。以前这里是一次性置位、永不复位，
    /// 于是"切出去又切回来"这一下就把整段识别结果丢掉了。
    /// （真正不能做的只有一件事：往**别的**窗口写字或退格。）
    pub fn guard(&mut self, now: Option<isize>) {
        // 句柄读不到时不判定：不能因为读不到句柄就把主人的输入停掉。
        // 这不是"不设防"——`watch` 只在确认过焦点已离开本程序时才记下来
        // （见 `session::wait_for_foreign_foreground`），"把自己的窗口误记成目标"
        // 这件事从源头堵住了。
        let (Some(watch), Some(now)) = (self.watch, now) else {
            return;
        };
        let away = watch != now;
        if away == self.away {
            return;
        }
        self.away = away;
        if away {
            crate::log::log("主人切到了别的窗口，暂停本次自动输入");
        } else {
            crate::log::log("主人回到原窗口，恢复本次自动输入");
        }
    }

    /// 实时阶段调用：把目标程序里的文本调整成 `desired`。
    /// 关闭实时输入时什么都不做（不记账），避免收尾时误以为已经打过字。
    ///
    /// `now` = 此刻的前台窗口，每次都要传（见 `guard`）。
    pub fn sync(&mut self, desired: &str, now: Option<isize>) -> Result<()> {
        self.guard(now);
        if !self.enabled {
            return Ok(());
        }
        match plan(self.blocked(), &self.sent, desired) {
            Some(planned) => self.execute(planned, desired),
            None => Ok(()),
        }
    }

    /// 会话收尾调用：把目标程序里的文本对账成最终识别结果。
    ///
    /// 返回 `Result<bool>`，三态的含义是调用方（`session`）必须区别对待的：
    /// - `Ok(true)`：文本已交付到目标程序，**或**本来就处于正确状态、无需输入；
    /// - `Ok(false)`：本次**放弃输入**（主人切到了别的窗口）—— 一个字都没写进去，
    ///   调用方必须改用剪贴板交付，否则主人这句话就凭空消失了；
    /// - `Err(_)`：注入失败（管理员窗口、被拦、粘贴兜底也没成……），同样要用剪贴板兜底。
    ///
    /// `now` = 此刻的前台窗口。**这一步不能省**：主人松手之后、收尾之前完全
    /// 可能又切走了窗口（等最终结果时尤其容易），此刻补打和退格都会落到别人家。
    pub fn finish(&mut self, desired: &str, now: Option<isize>) -> Result<bool> {
        self.guard(now);
        // 之前某一步注入已经失败 → 之后一个字都不再输入（避免反复报错），
        // 但**必须把原因抛出去**：调用方正是靠这个 Err 把整段结果放进剪贴板的。
        // 这里要是返回 Ok，主人刚说的话两条路都走不通 —— 既不进目标程序、
        // 也不进剪贴板，直接丢掉。
        if let Some(reason) = &self.failure {
            return Err(anyhow!("{reason}"));
        }
        // 测试模式：结果只回显在设置窗里，「永不外打」是软件对主人的承诺。
        // 返回 true 表示"无需（也严禁）再由调用方兜底" —— 这条路绝不许碰剪贴板。
        if self.muted {
            return Ok(true);
        }
        // 主人切走了窗口：退格会删掉别的窗口里的内容、整段重打也是打错地方，
        // 所以一个字都不输入。返回 false，让调用方把完整结果放进剪贴板 ——
        // 这是此刻唯一安全的交付方式（既不动别人的窗口，又把文字交到主人手上）。
        if self.away {
            return Ok(false);
        }
        match plan(self.blocked(), &self.sent, desired) {
            Some(planned) => {
                self.execute(planned, desired)?;
                Ok(true)
            }
            // 已经就是期望的状态，无需任何输入
            None => Ok(true),
        }
    }

    fn execute(&mut self, planned: Diff, desired: &str) -> Result<()> {
        let started = std::time::Instant::now();
        if planned.backspaces > 0 {
            // 退格失败**不做**粘贴兜底：删不掉的旧尾巴还留在目标程序里，
            // 再粘一段上去只会得到一段错的文本。留给会话收尾处理 ——
            // 那里会把完整结果放进剪贴板。
            if let Err(e) = injector::backspace(planned.backspaces) {
                self.failure = Some(e.to_string());
                return Err(e);
            }
        }
        if !planned.append.is_empty() {
            let expected = injector::typeable_units(&planned.append);
            match injector::type_text(&planned.append) {
                // 只有"实际提交数 == 期望数"才算整段打进目标程序，这时才记账。
                Ok(committed) if committed >= expected => {}
                // 只提交了一部分：多出来的尾巴必须改走剪贴板补上，
                // 而且只粘这段尾巴 —— 粘整段会和已经打进去的部分重复。
                Ok(committed) => {
                    let tail = tail_after_units(&planned.append, committed);
                    let e = anyhow!("逐字注入只提交了 {committed}/{expected} 个字符");
                    return self.paste_via_clipboard(&tail, desired, e);
                }
                Err(e) => return self.paste_via_clipboard(&planned.append, desired, e),
            }
        }
        self.sent = desired.to_string();

        // 只记录异常缓慢的那几次，正常情况（几百微秒）不写日志。
        // 排查"打字卡顿"时这里就是客观数据。
        let ms = started.elapsed().as_millis();
        if ms >= 80 {
            crate::log::log(format!(
                "实时输入偏慢：{ms}ms（退格 {} 字，补打 {} 字）",
                planned.backspaces,
                planned.append.chars().count()
            ));
        }
        Ok(())
    }

    /// 逐字注入被拒时的兜底：把该补的这段写到剪贴板，再用 `Ctrl+V` 粘贴。
    ///
    /// 为什么值得多这一步：`KEYEVENTF_UNICODE` 和 `Ctrl+V` 是两条不同的通道，
    /// 忽略前者的程序（远程桌面、部分 Electron/Java/老 MFC）认后者。
    ///
    /// 但 UIPI 拦下的是**整个** `SendInput`，那时候粘贴同样会失败 ——
    /// 所以先看错误类型，别白试一次、也别多报一次错。
    fn paste_via_clipboard(&mut self, append: &str, desired: &str, e: anyhow::Error) -> Result<()> {
        let denied = e.downcast_ref::<injector::AccessDenied>().is_some();
        if !self.paste_fallback || denied {
            self.failure = Some(e.to_string());
            return Err(e);
        }
        match self.paste(append) {
            Ok(()) => {
                self.sent = desired.to_string();
                crate::log::log(format!(
                    "逐字注入被拒，已改用剪贴板粘贴兜底（{} 字）",
                    append.chars().count()
                ));
                Ok(())
            }
            Err(pe) => {
                // 粘贴也没成：照旧记下失败、不再重试，避免反复报错。
                // 结果本身不会丢 —— 会话收尾会把它放进剪贴板。
                self.failure = Some(pe.to_string());
                Err(pe)
            }
        }
    }

    /// 走一次剪贴板粘贴。
    ///
    /// 写完**不再还原**主人原来的剪贴板内容（1.3.2 起改的）。原来会先存后还，
    /// 结果是：这段救回来的文字在 300 毫秒后又被我们自己擦掉 —— 而主人事后到
    /// 剪贴板里找它时，**什么也找不到**（他报的就是这个）。何况"粘贴到底成没成"
    /// 和"逐字注入成没成"一样无法核实（`SendInput` 只报告事件入队），
    /// 把唯一的退路（自己 Ctrl+V 一次）也抹掉毫无道理。
    /// 主人剪贴板里原来的内容会被这段文字顶掉 —— 这是「识别结果留一份」
    /// 那条设置本来就写明的代价（见 `config::Options::keep_on_clipboard`）。
    fn paste(&mut self, append: &str) -> Result<()> {
        clipboard::write_text(append)?;
        injector::paste()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 约定：这里的用例**不允许**真的走到 `injector` 去。注入会打到当前前台窗口
    // （跑 `cargo test` 时就是你的终端），所以每个用例都必须先让 `blocked()`
    // 为真，或者只走 `plan()`/`diff()` 这类纯函数。要构造"注入失败过"的状态，
    // 直接赋 `failure` 字段即可 —— 不要为了造这个态去真注入一次。

    #[test]
    fn appends_only_the_new_tail() {
        let d = diff("今天", "今天天气");
        assert_eq!(
            d,
            Diff {
                backspaces: 0,
                append: "天气".into()
            }
        );
    }

    #[test]
    fn rewrites_only_the_changed_character() {
        // 识别把"气"改成了"汽"：公共前缀是"今天天"，只需回退 1 个字再补 1 个字
        let d = diff("今天天气", "今天天汽");
        assert_eq!(
            d,
            Diff {
                backspaces: 1,
                append: "汽".into()
            }
        );
    }

    #[test]
    fn adds_punctuation_at_the_end() {
        // 定稿时补上句号：只补一个字符
        let d = diff("你好世界", "你好世界。");
        assert_eq!(
            d,
            Diff {
                backspaces: 0,
                append: "。".into()
            }
        );
    }

    #[test]
    fn noop_when_unchanged() {
        assert!(diff("一样", "一样").is_noop());
    }

    #[test]
    fn replaces_everything_when_prefix_differs() {
        let d = diff("错误内容", "正确内容");
        assert_eq!(d.backspaces, 4);
        assert_eq!(d.append, "正确内容");
    }

    #[test]
    fn surrogate_chars_count_as_one() {
        // 罕见字（代理对）按字符算退格数，与 Windows 的删除语义一致
        let d = diff("𠮷野家", "𠮷野");
        assert_eq!(d.backspaces, 1);
        assert!(d.append.is_empty());
    }

    /// 回归（会把字打进别的程序那个 bug）：
    /// 人在别的窗口里时，`sent` 里记的是打在**原窗口**里的字。
    /// 收尾若照常做差分，就会先对新窗口退格（删掉主人的内容）再补打 ——
    /// 所以不在目标窗口里时收尾必须一个字符都不输入。
    #[test]
    fn away_session_plans_no_final_edit() {
        assert_eq!(
            plan(true, "今天天气", "今天天汽不错。"),
            None,
            "不在目标窗口时收尾绝不能输入（否则退格会砸到别的窗口）"
        );
    }

    /// 兜底必须默认**关**着。单测里到处都在 `LiveTyper::new`，
    /// 只要有一个用例默认开着粘贴，它就会往**真实剪贴板**里写东西，
    /// 把开发机上主人正复制的内容洗掉。
    #[test]
    fn paste_fallback_is_off_by_default() {
        assert!(
            !LiveTyper::new(true, None).paste_fallback,
            "默认不能开粘贴兜底"
        );
        assert!(
            LiveTyper::new(true, None).with_paste_fallback(true).paste_fallback,
            "显式打开后应该生效（否则这个开关就是摆设）"
        );
    }

    /// 没在别的窗口里时，收尾照常给出差分（正常路径不能被这次修复误伤）
    #[test]
    fn normal_session_still_plans_the_final_edit() {
        assert_eq!(
            plan(false, "", "你好"),
            Some(Diff {
                backspaces: 0,
                append: "你好".into()
            })
        );
        assert_eq!(plan(false, "一样", "一样"), None, "无改动时不该做任何动作");
    }

    /// 回归（同一个动作却有两种结果）：关着「边说话边打字」时切走窗口，
    /// 整段文字照样会被打进新窗口 —— 而开着实时输入时是"一个字都不打"。
    ///
    /// 规则应该只有一条：按快捷键时盯着哪个窗口，字就只往那个窗口写。
    /// （关着实时输入时虽然还没往目标程序写过字，但把结果打进主人切过去的新窗口，
    /// 同样是"文字打到别处"。）
    #[test]
    fn switching_windows_stops_input_even_when_live_typing_was_off() {
        let mut t = LiveTyper::new(false, Some(100));
        t.guard(Some(200));
        assert!(t.away, "关着实时输入时切走窗口也应停止自动输入");
        t.finish("x", Some(200)).unwrap();
        assert!(t.sent.is_empty(), "整段文字被打进了主人切过去的那个窗口");
    }

    /// 切走只是**暂停**，不是报废：主人切出去看一眼再切回来，
    /// `sent` 里记的仍然正是原窗口里的那些字，继续对它做差分完全安全
    /// （`guard` 只改 `away`，从不碰 `sent` —— 恢复之所以安全就靠这一点）。
    ///
    /// 回归：以前这里是一次性置位、永不复位，于是"切出去又切回来"这一下
    /// 就把整段识别结果丢掉了 —— 屏幕上什么都没出现，正是主人报的那个现象。
    #[test]
    fn returning_to_the_original_window_resumes_typing() {
        let mut t = LiveTyper::new(true, Some(100));
        t.guard(Some(200));
        assert!(t.away, "切到别的窗口后应暂停");
        assert!(t.blocked(), "在别的窗口里时必须禁止注入");

        t.guard(Some(100));
        assert!(!t.away, "回到原窗口应恢复");
        assert!(!t.blocked(), "恢复后不应再禁止注入");
        assert!(t.enabled, "恢复不该动到主人的「边说话边打字」设置");
        assert!(t.sent.is_empty(), "单测里没注入过，sent 不该凭空出现内容");
    }

    /// 「一开始就没开实时输入」和「此刻不在目标窗口里」是两回事：
    /// 前者收尾仍要一次性把全文打出去，后者必须一个字都不输入。
    #[test]
    fn disabled_by_default_still_plans_its_final_edit() {
        // 在目标窗口里、只是没开实时输入：收尾照常给出"补打全文"的计划（这里只看计划，不真的注入）
        let t = LiveTyper::new(false, Some(100));
        assert!(!t.blocked(), "只是没开实时输入，不该被判定成禁止注入");
        assert_eq!(
            plan(t.blocked(), &t.sent, "你好"),
            Some(Diff {
                backspaces: 0,
                append: "你好".into()
            })
        );
    }

    /// 回归（实时输入会把字/退格打到别的程序里）：
    /// 前台窗口一旦变了，就必须停止实时输入，并按"收尾也不许输入"处理。
    #[test]
    fn guard_stops_typing_after_the_window_changes() {
        let mut t = LiveTyper::new(true, Some(100));
        t.guard(Some(200));
        assert!(t.away, "切到别的窗口后必须停止实时输入");
    }

    /// 同一个窗口时不能误伤：实时输入要照常工作
    #[test]
    fn guard_keeps_typing_in_the_same_window() {
        let mut t = LiveTyper::new(true, Some(100));
        t.guard(Some(100));
        assert!(t.enabled, "同一个窗口不该被停用");
        assert!(!t.away);
    }

    /// 拿不到窗口句柄时不拦：不能因为读不到句柄就把主人的输入停掉
    #[test]
    fn guard_does_not_block_when_handles_are_unknown() {
        for (start, now) in [(None, Some(200)), (Some(100), None), (None, None)] {
            let mut t = LiveTyper::new(true, start);
            t.guard(now);
            assert!(
                t.enabled,
                "句柄未知时不应停用实时输入（start={start:?}, now={now:?}）"
            );
        }
    }

    /// 回归（收尾前没有重新核对前台窗口 → 补打/退格落到别的程序里）：
    /// 主人松手之后、收尾之前又切走了窗口，此时 `sent` 记的是打在**旧窗口**里的字。
    /// `finish` 必须自己重新核对一次当前前台窗口，切走了就一个字都不输入。
    ///
    /// 探针用单字符：万一将来有人把这次校验删掉，也只会漏出一个 "x"。
    #[test]
    fn finish_rechecks_the_target_window() {
        let mut t = LiveTyper::new(true, Some(100));
        t.finish("x", Some(200)).unwrap();
        assert!(
            t.sent.is_empty(),
            "收尾没有重新核对前台窗口，把字打进了主人切过去的那个程序"
        );
    }

    /// 同一条校验也必须长在 `sync` 上：实时阶段主人切走之后，
    /// 下一条中间结果照样不能往新窗口里打字。
    #[test]
    fn sync_rechecks_the_target_window() {
        let mut t = LiveTyper::new(true, Some(100));
        t.sync("x", Some(200)).unwrap();
        assert!(t.sent.is_empty(), "实时阶段切走窗口后仍在打字");
    }

    /// 回归（「测试识别」会把结果打进别的程序）：
    /// 软件承诺「测试模式永不外打」——结果只回显在设置窗里。
    /// 之前测试模式是用 `LiveTyper::new(false, _)` 表示的，但 `finish` 故意不看
    /// `enabled`（那是给「关掉实时输入」用的），于是收尾照样把整段文字注了出去。
    /// 所以测试模式必须是一个独立的、任何情况下都不注入的状态。
    #[test]
    fn muted_session_never_types() {
        let mut t = LiveTyper::muted();
        t.sync("你好", Some(100)).unwrap();
        assert!(t.sent.is_empty(), "测试模式的实时阶段不应注入任何字符");
        // 探针用单字符：万一将来有人改坏了 muted，也只会漏出一个 "x"
        t.finish("x", Some(100)).unwrap();
        assert!(
            t.sent.is_empty(),
            "测试模式的收尾仍然注入了字符（会打进设置窗后面的程序）"
        );
    }

    /// 回归（会把整段识别结果丢掉）：注入失败过之后，`finish` 必须**报错**，
    /// 而不是返回一个"什么都不做"的 Ok。
    ///
    /// 调用方 `session` 是靠 `Err` 才把整段结果放进剪贴板的（`hand_off_to_clipboard`）。
    /// 之前 `finish` 走 `plan(blocked())`，而 `blocked()` 把已失败也算作禁止注入，
    /// 于是它安静地返回 Ok —— 结果既不进目标程序也不进剪贴板，主人的话直接消失。
    ///
    /// 这里直接构造失败态（不改真注入路径），断言 Err 确实被抛出来。
    #[test]
    fn finish_reports_a_latched_failure_so_the_caller_can_use_the_clipboard() {
        let mut t = LiveTyper::new(true, Some(100));
        t.failure = Some("目标程序以管理员权限运行".into());
        let err = t
            .finish("你好", Some(100))
            .expect_err("失败过之后 finish 必须报错，否则结果会被静默丢弃");
        assert!(
            err.to_string().contains("管理员权限"),
            "错误里要带上失败原因，界面上那句提示才有用：{err}"
        );
        assert!(t.sent.is_empty(), "报错路径不该注入任何字符");
    }

    /// 守住 `finish` 这条调用链本身（上一条只测了 `plan` 函数，
    /// 万一 `finish` 忘了把 `away` 传进去，它是发现不了的）。
    ///
    /// 断言 `sent` 没被记账：`execute` 只有在真的注入之后才会更新它。
    /// 探针用单个 ASCII 字符，即使将来有人改坏了这里、真的注入出去，
    /// 也只是一个 "x"，不会造成破坏。
    #[test]
    fn finish_types_nothing_while_in_another_window() {
        let mut t = LiveTyper::new(true, Some(100));
        t.guard(Some(200)); // 主人切走了
        let delivered = t.finish("x", Some(200)).unwrap();
        assert!(!delivered, "放弃输入时必须返回 false，调用方才改用剪贴板");
        assert!(
            t.sent.is_empty(),
            "不在目标窗口里时 finish 仍然输入了字符（会打到别的程序里）"
        );
    }

    /// 回归（主人的话既不进目标程序也不进剪贴板，凭空消失）：
    /// 主人切走了窗口时，`finish` 必须返回 `Ok(false)` 明确表示"放弃输入"，
    /// 这样调用方才知道要把结果放进剪贴板。返回 `Ok(true)` 会让两条交付路都断掉。
    #[test]
    fn away_finish_reports_abandoned_input() {
        let mut t = LiveTyper::new(true, Some(100));
        t.guard(Some(200)); // 主人切到别的窗口
        assert!(
            !t.finish("你好", Some(200)).unwrap(),
            "切走窗口时必须返回 false（放弃输入），由调用方改用剪贴板"
        );
    }

    /// 测试模式的收尾必须返回 `Ok(true)`：它表示"文本已由我们处理妥当、
    /// 无需调用方再兜底"。返回 false 会让 `session` 把结果写进剪贴板 ——
    /// 而「测试结果只显示在窗口里、绝不外泄」是软件对主人的承诺。
    #[test]
    fn muted_finish_reports_delivered_so_caller_never_touches_clipboard() {
        let mut t = LiveTyper::muted();
        assert!(
            t.finish("你好", Some(100)).unwrap(),
            "测试模式收尾必须返回 true，严禁让调用方走剪贴板"
        );
        assert!(t.sent.is_empty(), "测试模式不该注入任何字符");
    }

    /// 兜底粘贴只能粘"没成功提交的那一段尾巴"：
    /// 已经打进去的前缀再粘一遍就会重复（这正是收尾重复出字的根因）。
    #[test]
    fn tail_after_units_returns_only_the_uncommitted_suffix() {
        // 提交了 2 个单元（"你好"）→ 尾巴是剩下的
        assert_eq!(tail_after_units("你好世界。", 2), "世界。");
        // 全部提交 → 没有尾巴
        assert_eq!(tail_after_units("你好", 2), "");
        // 一个都没提交 → 整段都是尾巴
        assert_eq!(tail_after_units("你好", 0), "你好");
        // \r 不产生按键、不计入提交数：提交 3 个单元时应跳过 \r
        assert_eq!(tail_after_units("a\r\nbc", 3), "c");
    }
}
