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

pub struct LiveTyper {
    sent: String,
    /// 关闭后只记录不输入（例如录音中途用户切走了窗口）
    pub enabled: bool,
    /// 注入失败的原因，失败后不再重试，避免反复报错
    pub failure: Option<String>,
}

impl LiveTyper {
    pub fn new(enabled: bool) -> Self {
        Self {
            sent: String::new(),
            enabled,
            failure: None,
        }
    }

    pub fn disable(&mut self, reason: &str) {
        if self.enabled {
            self.enabled = false;
            crate::log::log(format!("实时输入已关闭：{reason}"));
        }
    }

    /// 实时阶段调用：把目标程序里的文本调整成 `desired`。
    /// 关闭实时输入时什么都不做（不记账），避免收尾时误以为已经打过字。
    pub fn sync(&mut self, desired: &str) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.apply(desired)
    }

    /// 会话收尾调用：无论开关如何，都要保证目标程序里的文本 == 最终识别结果
    pub fn finish(&mut self, desired: &str) -> Result<()> {
        self.apply(desired)
    }

    fn apply(&mut self, desired: &str) -> Result<()> {
        if self.failure.is_some() {
            return Ok(());
        }
        let planned = diff(&self.sent, desired);
        if planned.is_noop() {
            return Ok(());
        }

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
}
