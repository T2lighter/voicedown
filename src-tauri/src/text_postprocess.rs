/// VoiceDown - ASR 文本后处理模块
///
/// ASR 转写文本的纯函数后处理：繁→简字符转换（`t2s`，OpenCC STCharacters
/// 标准字典）+ 用户词典替换（`apply_dictionary`，最长匹配 + ASCII 词边界）。
/// 两者按「词典→繁简」顺序在 lib.rs `spawn_asr_thread` 串接调用。
///
/// 注：此模块不含 ASR 推理——识别由 Python 子进程 paraformer-zh-streaming
/// （funasr AutoModel）负责，音频重采样由 librosa 完成（asr_server.py）。

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

// ── 简繁转换 ──────────────────────────────────────────────

/// 将繁体中文转为简体中文
///
/// 基于 OpenCC STCharacters 标准字典，纯 Rust 实现，零外部依赖。
/// 首次调用时懒加载字典到 HashMap，后续调用 O(n) 字符级转换。
pub fn t2s(text: &str) -> String {
    static MAP: OnceLock<HashMap<char, char>> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        let mut m = HashMap::with_capacity(2700);
        for line in T2S_DATA.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split('\t');
            if let (Some(t_str), Some(s_str)) = (parts.next(), parts.next()) {
                if let (Some(t), Some(s)) = (t_str.chars().next(), s_str.chars().next()) {
                    m.insert(t, s);
                }
            }
        }
        m
    });
    text.chars().map(|c| *map.get(&c).unwrap_or(&c)).collect()
}

// ── 词典替换（ISSUE-4 / REM-08 词典硬化） ─────────────────────────

/// 单词字符（ASCII 字母/数字 + 下划线），用于 ASCII key 的词边界判定。
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// 对文本应用词典替换。
///
/// 单次扫描、最长匹配优先（keys 按长度降序）：
/// - 含 ASCII 字母/数字的 key 强制**词边界**（前后须为非单词字符或串首/尾），
///   故 `ai` 不误伤 `rain`，`voicedown` 作为整词才替换为 `VoiceDown`。
/// - 纯 CJK 等「无单词字符」的 key 退化为子串匹配（中文词边界交给 F-14 LLM，
///   不在 Rust 端引分词依赖——REM-08 决策）。
/// - 替换后的 value 不被重新扫描，避免二次污染（`ai→AI` 后 `AI` 不再被别的 key 命中）。
/// - keys 长度降序（同长按字典序）保证互为子串时（`aiops` vs `ai`）结果确定、可复现。
///
/// 纯函数（无 I/O、无全局状态），便于单测。调用顺序：在 `t2s` 之前（保持现有调用点）。
// ponytail: 每位置对所有 key 线性扫描；词典规模小（典型几十条）可接受，超大词典换 aho-corasick。
pub fn apply_dictionary(text: &str, dict: &BTreeMap<String, String>) -> String {
    if dict.is_empty() {
        return text.to_string();
    }

    // keys 按长度降序（最长优先）；同长按字典序，保证确定。
    let mut keys: Vec<&String> = dict.keys().collect();
    keys.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.as_str().cmp(b.as_str())));

    // 预计算每个 key 的 char 序列 + 是否需要词边界，避免扫描内重算。
    let entries: Vec<(Vec<char>, &str, bool)> = keys
        .iter()
        .map(|k| {
            let cv: Vec<char> = k.chars().collect();
            let needs_boundary = cv.iter().any(|c| c.is_ascii_alphanumeric());
            (cv, dict[*k].as_str(), needs_boundary)
        })
        .collect();

    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < n {
        let mut matched: Option<(usize, &str)> = None;
        for (k_chars, value, needs_boundary) in &entries {
            let klen = k_chars.len();
            if klen == 0 || i + klen > n {
                continue;
            }
            if !(0..klen).all(|j| chars[i + j] == k_chars[j]) {
                continue;
            }
            if *needs_boundary {
                let before_ok = i == 0 || !is_word_char(chars[i - 1]);
                let after_ok = i + klen >= n || !is_word_char(chars[i + klen]);
                if !before_ok || !after_ok {
                    continue; // 边界不满足，试更短 key
                }
            }
            matched = Some((klen, *value));
            break; // 长度降序，首个命中即最长
        }
        match matched {
            Some((klen, value)) => {
                out.push_str(value);
                i += klen; // 跳过已替换区，value 不被重扫
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn empty_dict_returns_text_unchanged() {
        assert_eq!(
            apply_dictionary("hello rain voicedown", &dict(&[])),
            "hello rain voicedown"
        );
    }

    #[test]
    fn ascii_word_boundary_protects_substrings() {
        // `ai` 整词替换，但不改 `rain` / `aiops`（内部 ai 非整词）。
        let d = dict(&[("ai", "AI")]);
        assert_eq!(
            apply_dictionary("ai and rain and aiops", &d),
            "AI and rain and aiops"
        );
    }

    #[test]
    fn ascii_whole_word_replacement_applies() {
        // `voicedown` 整词（句首 / 连字符 / 句尾均为边界）→ VoiceDown。
        let d = dict(&[("voicedown", "VoiceDown")]);
        assert_eq!(
            apply_dictionary("voicedown-app and voicedown", &d),
            "VoiceDown-app and VoiceDown"
        );
    }

    #[test]
    fn longest_match_wins_for_overlapping_keys() {
        // 互为子串：`aiops` 优先 `ai`；`ai` 整词仍替换。
        let d = dict(&[("ai", "AI"), ("aiops", "AIOPS")]);
        assert_eq!(apply_dictionary("aiops and ai", &d), "AIOPS and AI");
    }

    #[test]
    fn cjk_key_substring_match_no_boundary() {
        // 中文 key 无词边界，子串匹配；最长优先（`语音识别` 优先 `语音`）。
        let d = dict(&[("语音", "SP"), ("语音识别", "ASR")]);
        assert_eq!(apply_dictionary("语音识别技术", &d), "ASR技术");
    }

    #[test]
    fn replacement_value_not_re_scanned() {
        // `ai→AI` 后，`AI` 不被第二个 key（`AI→BUG`）二次命中。
        let d = dict(&[("ai", "AI"), ("AI", "BUG")]);
        assert_eq!(apply_dictionary("ai", &d), "AI");
    }

    // ── t2s 繁简转换（G 候选补测，确立行为基线） ──────────────

    #[test]
    fn t2s_converts_common_traditional_to_simplified() {
        // 常见繁体字 → 简体（映射见 t2s_data.txt，OpenCC STCharacters 标准字典）。
        assert_eq!(t2s("電腦"), "电脑"); // 電→电、腦→脑
        assert_eq!(t2s("台灣"), "台湾"); // 灣→湾，台 不变
        assert_eq!(t2s("國家"), "国家"); // 國→国，家 不变
        assert_eq!(t2s("學開關門"), "学开关门"); // 學→学、開→开、關→关、門→门
    }

    #[test]
    fn t2s_passthrough_ascii_and_empty() {
        // ASCII + 空串 passthrough 不变。
        assert_eq!(t2s("hello world"), "hello world");
        assert_eq!(t2s(""), "");
    }

    #[test]
    fn t2s_mixed_sentence() {
        // 混合句：ASCII passthrough + 繁→简。
        assert_eq!(t2s("VoiceDown 電腦 ASR"), "VoiceDown 电脑 ASR");
    }

    #[test]
    fn t2s_idempotent_on_simplified() {
        // 简体输出再转不变（简体字不在繁体 key 集，幂等）。
        let once = t2s("電腦");
        assert_eq!(t2s(&once), once);
    }
}

/// OpenCC STCharacters 标准字典（约 2600 对常见繁简映射）
const T2S_DATA: &str = include_str!("t2s_data.txt");
