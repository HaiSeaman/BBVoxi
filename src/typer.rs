//! 边说话边打字。
//!
//! 语音识别的中间结果会不断被修正（同音字、标点、断句），所以不能简单地"来一段打一段"。
//! 成熟输入法的做法是维护两个状态：
//!   - `sent`：已经打进目标程序的文本
//!   - `desired`：当前期望的文本（已定稿部分 + 当前句的中间结果）
//! 每次中间结果更新时，取 `sent` 与 `desired` 的最长公共前缀，
//! 把 `sent` 多出来的尾巴用退格删掉，再补打新的尾巴。
//! 这样：文本变长时只补打增量，识别结果被修正时也只回退/重打被改动的部分。

use anyhow::Result;

use crate::injector;

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

pub struct LiveTyper {
    sent: String,
    /// 是否边说话边打字。被放弃（切走窗口）或测试模式下会被置为 false
    pub enabled: bool,
    /// 注入失败的原因，失败后不再重试，避免反复报错
    pub failure: Option<String>,
    /// 运行中被放弃（录音途中切走了窗口）→ 收尾必须彻底放弃输入，见 `plan`
    abandoned: bool,
    /// 测试模式：结果只回显到设置窗，任何情况下都不注入（见 `muted`）
    muted: bool,
    /// 开始录音时的前台窗口。之后每次输入前都要拿当前窗口跟它核对，切走了就整体放弃
    watch: Option<isize>,
}

impl LiveTyper {
    /// `watch` = 开始录音那一刻的前台窗口（`session::foreground_window()`）。
    pub fn new(enabled: bool, watch: Option<isize>) -> Self {
        Self {
            sent: String::new(),
            enabled,
            failure: None,
            abandoned: false,
            muted: false,
            watch,
        }
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
        self.muted || self.abandoned || self.failure.is_some()
    }

    /// 每次准备输入之前核对前台窗口：主人切走了就放弃本次会话的自动输入。
    ///
    /// 为什么把它做成「每次输入前必须上报当前窗口」的**必填参数**（而不是一个
    /// 可选的外部检查）：漏掉一次校验，补打和退格就会落到别的程序里。这个坑
    /// 已经踩过两次 —— 第一次是收尾没校验，第二次是「实时阶段校验了、收尾忘了」。
    /// 现在 `sync` / `finish` 的签名逼着调用方每次都报一次，漏不掉。
    pub fn guard(&mut self, now: Option<isize>) {
        let switched = matches!((self.watch, now), (Some(a), Some(b)) if a != b);
        if switched {
            self.abandon("录音过程中切换了窗口");
        }
    }

    /// 放弃本次会话的自动输入：实时打字停掉，收尾也一个字都不输入。
    ///
    /// **不管「边说话边打字」是开是关，都要放弃**。关着实时输入时虽然还没往
    /// 目标程序写过字，但把整段结果打进"主人中途切过去的新窗口"同样是打错地方 ——
    /// 同一个动作不该有两种结果。（原来的实现只在 `enabled` 为真时才生效，
    /// 于是关着实时输入时切走窗口，反而会把全文打进新窗口。）
    pub fn abandon(&mut self, reason: &str) {
        if self.abandoned {
            return;
        }
        self.abandoned = true;
        self.enabled = false;
        crate::log::log(format!("已放弃本次自动输入：{reason}"));
    }

    /// 是否因"录音途中切走了窗口"而被放弃（收尾不会输入任何字符）
    pub fn is_abandoned(&self) -> bool {
        self.abandoned
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
    /// 但被放弃的会话（录音途中切走了窗口）和测试模式例外：一个字符都不输入。
    /// 前者 `sent` 记的是打在旧窗口里的字，对新窗口做退格会删掉主人的内容；
    /// 后者连设置窗都不该被写进去。
    ///
    /// `now` = 此刻的前台窗口。**这一步不能省**：主人松手之后、收尾之前完全
    /// 可能又切走了窗口（等最终结果时尤其容易），此刻补打和退格都会落到别人家。
    pub fn finish(&mut self, desired: &str, now: Option<isize>) -> Result<()> {
        self.guard(now);
        match plan(self.blocked(), &self.sent, desired) {
            Some(planned) => self.execute(planned, desired),
            None => Ok(()),
        }
    }

    fn execute(&mut self, planned: Diff, desired: &str) -> Result<()> {
        let started = std::time::Instant::now();
        if planned.backspaces > 0 {
            if let Err(e) = injector::backspace(planned.backspaces) {
                self.failure = Some(e.to_string());
                return Err(e);
            }
        }
        if !planned.append.is_empty() {
            if let Err(e) = injector::type_text(&planned.append) {
                self.failure = Some(e.to_string());
                return Err(e);
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    /// 录音途中切走窗口后，`sent` 里记的是打在**旧窗口**里的字。
    /// 收尾若照常做差分，就会先对新窗口退格（删掉主人的内容）再补打 ——
    /// 所以被放弃的会话在收尾时必须一个字符都不输入。
    #[test]
    fn abandoned_session_plans_no_final_edit() {
        assert_eq!(
            plan(true, "今天天气", "今天天汽不错。"),
            None,
            "被放弃的会话收尾绝不能输入（否则退格会砸到新窗口）"
        );
    }

    /// 没被放弃时，收尾照常给出差分（正常路径不能被这次修复误伤）
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
    /// 规则应该只有一条：按快捷键时盯着哪个窗口，字就只往那个窗口写；中途切走就不写。
    /// （关着实时输入时虽然还没往目标程序写过字，但把结果打进主人切过去的新窗口，
    /// 同样是"文字打到别处"。）
    #[test]
    fn switching_windows_abandons_even_when_live_typing_was_off() {
        let mut t = LiveTyper::new(false, Some(100));
        t.guard(Some(200));
        assert!(t.is_abandoned(), "关着实时输入时切走窗口也应放弃自动输入");
        t.finish("x", Some(200)).unwrap();
        assert!(
            t.sent.is_empty(),
            "整段文字被打进了主人中途切过去的那个窗口"
        );
    }

    /// 「一开始就没开实时输入」和「运行中被停用」是两回事：
    /// 前者收尾仍要一次性把全文打出去，后者必须彻底放弃。
    #[test]
    fn disabled_by_default_still_plans_its_final_edit() {
        // 没被放弃：收尾照常给出"补打全文"的计划（这里只看计划，不真的注入）
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
    /// 前台窗口一旦变了，就必须停止实时输入，并标记为"收尾也不许输入"。
    #[test]
    fn guard_stops_typing_after_the_window_changes() {
        let mut t = LiveTyper::new(true, Some(100));
        t.guard(Some(200));
        assert!(!t.enabled, "切到别的窗口后必须停止实时输入");
        assert!(
            t.is_abandoned(),
            "切窗口后收尾也不能再输入（退格会砸到新窗口）"
        );
    }

    /// 同一个窗口时不能误伤：实时输入要照常工作
    #[test]
    fn guard_keeps_typing_in_the_same_window() {
        let mut t = LiveTyper::new(true, Some(100));
        t.guard(Some(100));
        assert!(t.enabled, "同一个窗口不该被停用");
        assert!(!t.is_abandoned());
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

    /// 守住 `finish` 这条调用链本身（上一条只测了 `plan` 函数，
    /// 万一 `finish` 忘了把 abandoned 传进去，它是发现不了的）。
    ///
    /// 断言 `sent` 没被记账：`execute` 只有在真的注入之后才会更新它。
    /// 探针用单个 ASCII 字符，即使将来有人改坏了这里、真的注入出去，
    /// 也只是一个 "x"，不会造成破坏。
    #[test]
    fn finish_types_nothing_after_being_abandoned() {
        let mut t = LiveTyper::new(true, Some(100));
        t.abandon("录音过程中切换了窗口");
        t.finish("x", Some(100)).unwrap();
        assert!(
            t.sent.is_empty(),
            "被放弃的会话在 finish 里仍然输入了字符（会打到别的程序里）"
        );
    }
}
