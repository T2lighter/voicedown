//! F-14 文本智能优化模块
//!
//! LLM 后端抽象（Ollama / OpenAI 兼容）+ 增量分批缓冲 + 配置管理。
//! 全模块随 `asr` feature 门控。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaConfig {
    pub endpoint: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub enabled: bool,
    /// "ollama" | "openai"
    pub backend: String,
    pub ollama: OllamaConfig,
    pub openai: OpenAiConfig,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: "ollama".to_string(),
            ollama: OllamaConfig {
                endpoint: "http://localhost:11434".to_string(),
                model: "qwen2.5:3b".to_string(),
            },
            openai: OpenAiConfig {
                endpoint: "https://api.deepseek.com/v1".to_string(),
                model: "deepseek-chat".to_string(),
                api_key: String::new(),
            },
        }
    }
}

// 持久化已迁 json_config_store（D 候选）：load/save 经 JsonConfigStore::default()，
// 文件名 "llm_config"。config_path/parse_config/load_config/save_config 已删。

// ── Prompt 构造 ────────────────────────────────────────

pub const FINALIZE_PROMPT: &str = "你是中文文档编辑助手。对下面的语音转写全文（可能已轻度润色）做三件事：① 三级结构化：按主题转换切分「# 章节标题」，正文较长的章节仅在确有子主题时加「## 小标题」，禁止每段都升标题、禁止对单一主题强行拆章节；② 全局风格统一：全文用词、语气、书面化程度前后一致，专业、简洁、可读；③ 去冗余：删除口语语气词、重复、啰嗦，合并口水话，但不删事实、细节、专有名词、数字、人名。保持原意与信息量，不做摘要、不增删事实。只输出 markdown 文档（# / ## / 正文 / 列表），不要解释、不要加引号、不要前后缀说明。";

pub const OPTIMIZE_PROMPT: &str = "你是中文语音转写文本的后处理助手。对文本做：纠正同音/近音错别字；补全并修正标点；删除口语语气词（嗯、啊、那个、就是、然后等）但保留必要连接词；轻度书面化润色；按语义完整性重新分段，通常每段约 3-5 句，以语义为准、不要机械按句数切；段落之间用一个空行（两个换行）分隔。保持原意与人称，不增删事实信息。只输出修改后的纯文本，不要解释、不要加引号。";

/// mode="finalize" → Finalize；其余（含 "optimize"/未知）→ Optimize。
pub fn build_prompt(mode: &str) -> &'static str {
    match mode {
        "finalize" => FINALIZE_PROMPT,
        _ => OPTIMIZE_PROMPT,
    }
}

// ── 增量分批缓冲 ───────────────────────────────────────

use std::time::Instant;

pub const MAX_INTERVAL_SECS: u64 = 30;
/// 字数驱动攒批目标：本批新句累积到此即 flush。
pub const TARGET_CHARS: usize = 600;
/// 句数硬上限：超短句撑大单批时的安全阀。取 24 让字数(TARGET_CHARS)主导攒批——
/// 句数仅在超短句极端积压时兜底，避免过早截断使单批上下文不足、LLM 分段过细。
pub const MAX_SENTENCES_HARD: usize = 24;

pub struct OptimizerBuffer {
    pending: Vec<String>,
    last_flush: Instant,
}

impl OptimizerBuffer {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            last_flush: Instant::now(),
        }
    }

    /// 当前 pending 总字数。
    fn chars(&self) -> usize {
        self.pending.iter().map(|s| s.chars().count()).sum()
    }

    /// 若加入 s 会让本批新句总字数超过 TARGET_CHARS，应先把已累积的 flush 掉（s 另起一批）。
    /// pending 为空时永远 false（单句再长也直接进批，由 should_flush_size 决定是否立即发）。
    pub fn would_exceed(&self, s: &str) -> bool {
        !self.pending.is_empty() && self.chars() + s.chars().count() > TARGET_CHARS
    }

    /// 加入 s 后是否应立即 flush（字数达标，或句数硬上限）。
    pub fn should_flush_size(&self) -> bool {
        !self.pending.is_empty()
            && (self.chars() >= TARGET_CHARS || self.pending.len() >= MAX_SENTENCES_HARD)
    }

    /// 时间兜底：pending 非空且距上次 flush 达 MAX_INTERVAL_SECS。
    pub fn should_flush_time(&self) -> bool {
        !self.pending.is_empty() && self.last_flush.elapsed().as_secs() >= MAX_INTERVAL_SECS
    }

    /// 取出本批新句（\n 连接）、清空 pending、重置计时。
    pub fn drain_new_sentences(&mut self) -> String {
        let joined = self.pending.join("\n");
        self.pending.clear();
        self.last_flush = Instant::now();
        joined
    }

    pub fn push(&mut self, s: String) {
        self.pending.push(s);
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

impl Default for OptimizerBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ── 重叠重写状态机（消除批间段落断裂）──────────────────

/// 重叠重写：维护 `committed`（已确认前文，只增）+ `overlap`（上批末句，每批重写）。
/// 每批把 `overlap + 新句` 送 LLM 重新分段，段边界在 overlap 区重新对齐 → 跨批段落连贯。
/// 代价：overlap 区每批可能被 LLM 微调（右栏闪烁，已接受）。
pub struct OverlapRewriter {
    committed: String,
    overlap: String,
}

impl OverlapRewriter {
    pub fn new() -> Self {
        Self { committed: String::new(), overlap: String::new() }
    }

    /// 构造送 LLM 的输入：`overlap + new`（第一批 overlap 空 → 仅 new）。
    pub fn build_input(&self, new_sentences: &str) -> String {
        let n = new_sentences.trim();
        if self.overlap.is_empty() {
            n.to_string()
        } else {
            format!("{}\n{}", self.overlap.trim(), n)
        }
    }

    /// 应用 LLM 输出：按段落切分，前 N-1 段进 committed（显示），末段进 overlap（隐藏）。
    /// 整批 1 段 → 对半切 2 段（前段显示 + 尾段保留衔接）；0-1 句无法切 → 全显示。
    /// `out` 空/失败 → 降级：把 `overlap + fallback_new` 当原文输出走同一逻辑（不丢字）。
    pub fn apply_output(&mut self, out: &str, fallback_new: &str) {
        let out_t = out.trim();
        let text = if !out_t.is_empty() {
            out_t.to_string()
        } else {
            // 降级：模仿原文输出 = overlap + fallback_new
            let fb = fallback_new.trim();
            match (self.overlap.is_empty(), fb.is_empty()) {
                (true, true) => return,
                (true, false) => fb.to_string(),
                (false, true) => self.overlap.trim().to_string(),
                (false, false) => format!("{}\n{}", self.overlap.trim(), fb),
            }
        };
        let paras = split_paragraphs(&text);
        match paras.len() {
            0 => { /* 空不变 */ }
            1 => match split_single_paragraph(&paras[0]) {
                Some((head, tail)) => {
                    // ≥2 句：对半切 → 前段显示，尾段保留衔接
                    self.append_to_committed(&head);
                    self.overlap = tail;
                }
                None => {
                    // 0-1 句无法再切：全显示，不留衔接
                    self.append_to_committed(&paras[0]);
                    self.overlap.clear();
                }
            },
            _ => {
                let last = paras.last().unwrap().clone();
                let head = paras[..paras.len() - 1].join("\n\n");
                self.append_to_committed(&head);
                self.overlap = last;
            }
        }
    }

    /// 完整文本 = committed（overlap 隐藏不显示，停止时由 finalize 吸收）。
    pub fn full_text(&self) -> String {
        self.committed.trim_end().to_string()
    }

    /// 停止收尾：overlap 吸收进 committed（无后续批，末句也确认）。
    pub fn finalize(&mut self) {
        let o = self.overlap.trim().to_string();
        if !o.is_empty() {
            self.append_to_committed(&o);
            self.overlap.clear();
        }
    }

    fn append_to_committed(&mut self, s: &str) {
        let s = s.trim();
        if s.is_empty() {
            return;
        }
        if !self.committed.is_empty() {
            self.committed.push_str("\n\n");
        }
        self.committed.push_str(s);
    }
}

impl Default for OverlapRewriter {
    fn default() -> Self {
        Self::new()
    }
}

// ── 输入构造 / 末句提取（纯函数，便于单测）──────────────

/// 离线定稿的输入选择：`optimized` 非空白 → 用优化文；否则（ISSUE-1 范围）→ None。
/// `transcription` 占位参数（ISSUE-2 扩展 fallback 到原始转录）。空白 trim 判空。
pub fn pick_finalize_input(optimized: &str, transcription: &str) -> Option<String> {
    let o = optimized.trim();
    if !o.is_empty() {
        return Some(o.to_string());
    }
    // ISSUE-2：优化文空白 → fallback 到原始转录（未开实时优化也能定稿）
    let t = transcription.trim();
    if !t.is_empty() {
        return Some(t.to_string());
    }
    None
}

/// 按换行切段落（兼容 `\n` 与 `\n\n`），trim 每段、滤空行。主路径用。
/// `OverlapRewriter::apply_output` 消费此函数做段落三分支。
pub fn split_paragraphs(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 按句末标点（`。！？!?`）切句，保留标点；无标点返回整段 trim。
/// 1 段兜底"强制对半切"用。
pub fn split_sentences(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let punct: &[char] = &['。', '！', '？', '!', '?'];
    let mut sentences: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in trimmed.chars() {
        cur.push(ch);
        if punct.contains(&ch) {
            let s = cur.trim().to_string();
            if !s.is_empty() {
                sentences.push(s);
            }
            cur.clear();
        }
    }
    let tail = cur.trim().to_string();
    if !tail.is_empty() {
        sentences.push(tail);
    }
    sentences
}

/// 把单段文本在约中点的句末标点切成两段 `(前段, 尾段)`。
/// 句数 < 2（无法切）→ `None`。用于 `apply_output` 的"LLM 只分出 1 段"兜底：
/// 前段进 committed 显示，尾段作 overlap 保留衔接。
/// `OverlapRewriter::apply_output` 1 段兜底分支消费此函数。
pub fn split_single_paragraph(text: &str) -> Option<(String, String)> {
    let sentences = split_sentences(text);
    if sentences.len() < 2 {
        return None;
    }
    let mid = sentences.len() / 2;
    let head = sentences[..mid].join("");
    let tail = sentences[mid..].join("");
    Some((head, tail))
}

// ── 后端请求体构造（纯函数，便于单测）──────────────────

pub fn build_ollama_body(model: &str, prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "prompt": prompt,
        "stream": false,
        "options": { "temperature": 0.3 }
    })
}

pub fn build_openai_body(model: &str, system: &str, user: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ],
        "temperature": 0.3
    })
}

// ── LlmBackend trait + 实现 ────────────────────────────

pub trait LlmBackend: Send + Sync {
    /// 优化一段文本；失败返回错误字符串（调用方降级为原始文本）。
    fn optimize(&self, text: &str, mode: &str) -> Result<String, String>;
    /// 探测后端是否可用（不发消耗性请求）。
    fn check_available(&self) -> bool;
    fn name(&self) -> &str;
}

fn blocking_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

pub struct OllamaBackend {
    endpoint: String,
    model: String,
    client: reqwest::blocking::Client,
}

impl OllamaBackend {
    pub fn new(endpoint: String, model: String) -> Self {
        Self { endpoint, model, client: blocking_client() }
    }
}

impl LlmBackend for OllamaBackend {
    fn optimize(&self, text: &str, mode: &str) -> Result<String, String> {
        let prompt = build_prompt(mode);
        let url = format!("{}/api/generate", self.endpoint.trim_end_matches('/'));
        let body = build_ollama_body(&self.model, &format!("{}\n\n{}", prompt, text));
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .map_err(|e| format!("Ollama 请求失败: {e}"))?;
        let v: serde_json::Value =
            resp.json().map_err(|e| format!("Ollama 响应解析失败: {e}"))?;
        v.get("response")
            .and_then(|r| r.as_str())
            .map(|s| s.trim().to_string())
            .ok_or_else(|| "Ollama 响应缺 response 字段".to_string())
    }

    fn check_available(&self) -> bool {
        let url = format!("{}/api/tags", self.endpoint.trim_end_matches('/'));
        self.client
            .get(&url)
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    fn name(&self) -> &str {
        "ollama"
    }
}

pub struct OpenAiBackend {
    endpoint: String,
    model: String,
    api_key: String,
    client: reqwest::blocking::Client,
}

impl OpenAiBackend {
    pub fn new(endpoint: String, model: String, api_key: String) -> Self {
        Self { endpoint, model, api_key, client: blocking_client() }
    }
}

impl LlmBackend for OpenAiBackend {
    fn optimize(&self, text: &str, mode: &str) -> Result<String, String> {
        let system = build_prompt(mode);
        let url = format!("{}/chat/completions", self.endpoint.trim_end_matches('/'));
        let body = build_openai_body(&self.model, system, text);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .map_err(|e| format!("OpenAI 请求失败: {e}"))?;
        let v: serde_json::Value =
            resp.json().map_err(|e| format!("OpenAI 响应解析失败: {e}"))?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.trim().to_string())
            .ok_or_else(|| "OpenAI 响应缺 choices[0].message.content".to_string())
    }

    fn check_available(&self) -> bool {
        // 不发真实请求（避免消耗/网络抖动误判）；仅校验配置完整。
        !self.api_key.is_empty() && reqwest::Url::parse(&self.endpoint).is_ok()
    }

    fn name(&self) -> &str {
        "openai"
    }
}

/// enabled=false 或配置不全 → None。
pub fn build_backend(cfg: &LlmConfig) -> Option<Box<dyn LlmBackend>> {
    if !cfg.enabled {
        return None;
    }
    match cfg.backend.as_str() {
        "openai" => Some(Box::new(OpenAiBackend::new(
            cfg.openai.endpoint.clone(),
            cfg.openai.model.clone(),
            cfg.openai.api_key.clone(),
        ))),
        _ => Some(Box::new(OllamaBackend::new(
            cfg.ollama.endpoint.clone(),
            cfg.ollama.model.clone(),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_disabled() {
        let c = LlmConfig::default();
        assert!(!c.enabled, "默认必须禁用，避免未配置就外发");
        assert_eq!(c.backend, "ollama");
    }

    #[test]
    fn config_roundtrip() {
        let cfg = LlmConfig {
            enabled: true,
            backend: "openai".into(),
            ollama: OllamaConfig { endpoint: "http://x".into(), model: "m".into() },
            openai: OpenAiConfig { endpoint: "http://y".into(), model: "d".into(), api_key: "k".into() },
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: LlmConfig = serde_json::from_str(&s).unwrap();
        assert!(back.enabled);
        assert_eq!(back.backend, "openai");
        assert_eq!(back.openai.api_key, "k");
        assert_eq!(back.ollama.model, "m");
    }


    #[test]
    fn split_paragraphs_single_newline() {
        let v = split_paragraphs("段一。\n段二。");
        assert_eq!(v, vec!["段一。".to_string(), "段二。".to_string()]);
    }

    #[test]
    fn split_paragraphs_double_newline() {
        let v = split_paragraphs("段一。\n\n段二。");
        assert_eq!(v, vec!["段一。".to_string(), "段二。".to_string()]);
    }

    #[test]
    fn split_paragraphs_blank_lines_filtered() {
        let v = split_paragraphs("\n\n段一。\n\n\n段二。\n");
        assert_eq!(v, vec!["段一。".to_string(), "段二。".to_string()]);
    }

    #[test]
    fn split_paragraphs_single_paragraph() {
        let v = split_paragraphs("只有一段。多句。");
        assert_eq!(v, vec!["只有一段。多句。".to_string()]);
    }

    #[test]
    fn split_paragraphs_empty() {
        assert!(split_paragraphs("").is_empty());
        assert!(split_paragraphs("   \n  \n").is_empty());
    }

    #[test]
    fn split_paragraphs_trims_each() {
        let v = split_paragraphs("  段一。  \n  段二。 ");
        assert_eq!(v, vec!["段一。".to_string(), "段二。".to_string()]);
    }

    #[test]
    fn split_sentences_by_punct() {
        let v = split_sentences("句一。句二！句三？");
        assert_eq!(v, vec!["句一。".to_string(), "句二！".to_string(), "句三？".to_string()]);
    }

    #[test]
    fn split_sentences_no_punct_whole() {
        let v = split_sentences("没有标点的整段");
        assert_eq!(v, vec!["没有标点的整段".to_string()]);
    }

    #[test]
    fn split_sentences_empty() {
        assert!(split_sentences("").is_empty());
        assert!(split_sentences("   ").is_empty());
    }

    #[test]
    fn split_sentences_keeps_punct_and_tail() {
        // 末尾无标点的片段作为末句保留
        let v = split_sentences("你好。再见");
        assert_eq!(v, vec!["你好。".to_string(), "再见".to_string()]);
    }

    #[test]
    fn split_single_paragraph_half_even() {
        // 4 句 → mid=2，前 2 句 / 后 2 句
        let (h, t) = split_single_paragraph("一。二。三。四。").unwrap();
        assert_eq!(h, "一。二。");
        assert_eq!(t, "三。四。");
    }

    #[test]
    fn split_single_paragraph_half_odd() {
        // 5 句 → mid=2，前 2 句 / 后 3 句
        let (h, t) = split_single_paragraph("一。二。三。四。五。").unwrap();
        assert_eq!(h, "一。二。");
        assert_eq!(t, "三。四。五。");
    }

    #[test]
    fn split_single_paragraph_two_sentences() {
        let (h, t) = split_single_paragraph("句一。句二。").unwrap();
        assert_eq!(h, "句一。");
        assert_eq!(t, "句二。");
    }

    #[test]
    fn split_single_paragraph_too_few_none() {
        assert!(split_single_paragraph("只有一句。").is_none());
        assert!(split_single_paragraph("").is_none());
        assert!(split_single_paragraph("无标点整段").is_none());
    }

    #[test]
    fn pick_finalize_input_prefers_optimized() {
        assert_eq!(
            pick_finalize_input("优化文", "原始转录"),
            Some("优化文".to_string()),
            "优化文非空 → 用优化文（忽略 transcription）"
        );
    }

    #[test]
    fn pick_finalize_input_fallback_to_transcription() {
        assert_eq!(
            pick_finalize_input("   ", "原始转录"),
            Some("原始转录".to_string()),
            "优化文空白 → fallback 到原始转录"
        );
        assert_eq!(
            pick_finalize_input("", "  原始转录  "),
            Some("原始转录".to_string()),
            "优化文空串 → fallback 到原始转录（trim）"
        );
    }

    #[test]
    fn pick_finalize_input_both_empty_none() {
        assert_eq!(pick_finalize_input("", ""), None, "两者都空 → None");
        assert_eq!(pick_finalize_input("   ", "  "), None, "两者都空白 → None");
    }

    #[test]
    fn ollama_body_shape() {
        let body = build_ollama_body("qwen2.5:3b", "hi");
        assert_eq!(body["model"], "qwen2.5:3b");
        assert_eq!(body["stream"], false);
        assert_eq!(body["options"]["temperature"], 0.3);
        assert_eq!(body["prompt"], "hi");
    }

    #[test]
    fn openai_body_shape() {
        let body = build_openai_body("deepseek-chat", "sys", "usr");
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "sys");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "usr");
        assert_eq!(body["temperature"], 0.3);
    }

    #[test]
    fn openai_check_available_requires_key_and_url() {
        let no_key = OpenAiBackend::new("https://api.deepseek.com/v1".into(), "m".into(), "".into());
        assert!(!no_key.check_available(), "空 key 不可用");
        let with_key = OpenAiBackend::new("https://api.deepseek.com/v1".into(), "m".into(), "sk-x".into());
        assert!(with_key.check_available());
        let bad_url = OpenAiBackend::new("not a url".into(), "m".into(), "sk-x".into());
        assert!(!bad_url.check_available(), "非法 url 不可用");
    }

    #[test]
    fn build_backend_disabled_returns_none() {
        let cfg = LlmConfig::default(); // enabled=false
        assert!(build_backend(&cfg).is_none());
    }

    #[test]
    fn build_backend_ollama_when_enabled() {
        let mut cfg = LlmConfig::default();
        cfg.enabled = true;
        cfg.backend = "ollama".into();
        let b = build_backend(&cfg).expect("enabled 应返回 Some");
        assert_eq!(b.name(), "ollama");
    }

    #[test]
    fn build_backend_openai_when_enabled() {
        let mut cfg = LlmConfig::default();
        cfg.enabled = true;
        cfg.backend = "openai".into();
        let b = build_backend(&cfg).expect("enabled 应返回 Some");
        assert_eq!(b.name(), "openai");
    }

    #[test]
    fn buf_would_exceed_empty_pending_false() {
        let b = OptimizerBuffer::new();
        assert!(!b.would_exceed(&"字".repeat(200)), "pending 空，单句再长也不 exceed");
    }

    #[test]
    fn buf_would_exceed_over_target_true() {
        let mut b = OptimizerBuffer::new();
        b.push("字".repeat(TARGET_CHARS - 10));
        assert!(b.would_exceed(&"字".repeat(20)), "已近阈值 + 新句越界 → exceed");
    }

    #[test]
    fn buf_would_exceed_under_target_false() {
        let mut b = OptimizerBuffer::new();
        b.push("字".repeat(TARGET_CHARS - 10));
        assert!(!b.would_exceed(&"字".repeat(5)), "已近阈值 + 新句未越界 → 不 exceed");
    }

    #[test]
    fn buf_should_flush_size_by_chars() {
        let mut b = OptimizerBuffer::new();
        b.push("字".repeat(TARGET_CHARS));
        assert!(b.should_flush_size(), "字数达 TARGET 应 flush");
    }

    #[test]
    fn buf_should_flush_size_below_target_false() {
        let mut b = OptimizerBuffer::new();
        b.push("字".repeat(TARGET_CHARS - 10));
        assert!(!b.should_flush_size(), "未达 TARGET 不 flush");
    }

    #[test]
    fn buf_should_flush_size_by_hard_limit() {
        let mut b = OptimizerBuffer::new();
        for _ in 0..MAX_SENTENCES_HARD {
            b.push("短".into());
        }
        assert!(b.should_flush_size(), "句数达 MAX_SENTENCES_HARD 硬上限应 flush");
    }

    #[test]
    fn buf_no_longer_flushes_at_12_sentences() {
        // 回归保护：12 短句（旧 MAX_SENTENCES_HARD）字数远未达 TARGET_CHARS 时不应触发 flush。
        // 这正是 2026-07-02「分段过细」根因——句数硬上限曾=12，在 ~43字/句下先于 600 字截断，
        // 单批仅 ~446 字致 LLM 分段过细。常量若被改回 12，此测试会红。
        let mut b = OptimizerBuffer::new();
        for _ in 0..12 {
            b.push("短".into());
        }
        assert!(!b.should_flush_size(), "12 短句字数未达 TARGET，不应被句数截断");
    }

    #[test]
    fn buf_should_flush_time_false_when_fresh() {
        let mut b = OptimizerBuffer::new();
        b.push("x".into());
        assert!(!b.should_flush_time(), "刚 new 不应触发时间兜底");
    }

    // ── OverlapRewriter（末段保留 + 隐藏；committed 段间 \n\n）──
    #[test]
    fn overlap_first_batch_input_is_just_new() {
        let r = OverlapRewriter::new();
        assert_eq!(r.build_input("新句1\n新句2"), "新句1\n新句2");
    }

    #[test]
    fn overlap_apply_output_sets_overlap_to_last_paragraph() {
        let mut r = OverlapRewriter::new();
        // "句一。\n句二。" 按 \n 切成 2 段 → 前段 committed，末段 overlap
        r.apply_output("句一。\n句二。", "fb");
        assert_eq!(r.committed, "句一。");
        assert_eq!(r.overlap, "句二。");
    }

    #[test]
    fn overlap_build_input_includes_overlap_second_batch() {
        let mut r = OverlapRewriter::new();
        r.apply_output("句一。\n句二。", "fb");
        assert_eq!(r.build_input("新句。"), "句二。\n新句。");
    }

    #[test]
    fn overlap_apply_output_grows_committed_second_batch() {
        let mut r = OverlapRewriter::new();
        r.apply_output("句一。\n句二。", "fb1");
        // 第二批 2 段：前段追加 committed（\n\n 连接），末段进 overlap
        r.apply_output("句二。\n新句三。", "fb2");
        assert_eq!(r.committed, "句一。\n\n句二。");
        assert_eq!(r.overlap, "新句三。");
    }

    #[test]
    fn overlap_full_text_excludes_overlap() {
        let mut r = OverlapRewriter::new();
        r.apply_output("段一。\n段二。", "fb");
        // full_text 只返回 committed（末段 overlap 隐藏，不显示）
        let full = r.full_text();
        assert_eq!(full, "段一。");
        assert!(!full.starts_with('\n'));
        assert!(!full.ends_with('\n'));
    }

    #[test]
    fn overlap_finalize_absorbs_overlap_into_committed() {
        let mut r = OverlapRewriter::new();
        r.apply_output("句一。\n句二。", "fb");
        r.finalize();
        // finalize 把隐藏的末段吸收进 committed（\n\n 连接）
        assert_eq!(r.committed, "句一。\n\n句二。");
        assert!(r.overlap.is_empty());
        assert_eq!(r.full_text(), "句一。\n\n句二。");
    }

    #[test]
    fn overlap_degrade_empty_output_uses_fallback_no_dup() {
        let mut r = OverlapRewriter::new();
        r.apply_output("句一。\n句二。", "fb1");
        // 第二批 LLM 失败（out 空）→ overlap+fallback 原文重新切段
        r.apply_output("", "新句三。\n新句四。");
        assert_eq!(r.committed, "句一。\n\n句二。\n\n新句三。");
        assert_eq!(r.overlap, "新句四。");
        // full_text 不含 overlap
        assert_eq!(r.full_text(), "句一。\n\n句二。\n\n新句三。");
    }

    #[test]
    fn overlap_single_paragraph_forced_split() {
        let mut r = OverlapRewriter::new();
        // LLM 只分出 1 段（4 句无换行）→ 对半切，前段显示，尾段保留衔接
        r.apply_output("一。二。三。四。", "fb");
        assert_eq!(r.committed, "一。二。");
        assert_eq!(r.overlap, "三。四。");
    }

    #[test]
    fn overlap_single_paragraph_too_few_shows_all() {
        let mut r = OverlapRewriter::new();
        // 1 段且仅 1 句 → 无法切，全显示，overlap 清空
        r.apply_output("只有一句。", "fb");
        assert_eq!(r.committed, "只有一句。");
        assert!(r.overlap.is_empty());
    }

    #[test]
    fn config_default_has_no_mode() {
        let c = LlmConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.backend, "ollama");
    }

    #[test]
    fn parse_legacy_mode_field_ignored() {
        // 旧配置含 mode 字段，serde 忽略未知字段，解析成功
        let c: LlmConfig = serde_json::from_str(r#"{"enabled":true,"backend":"ollama","mode":"deep","ollama":{"endpoint":"http://x","model":"m"},"openai":{"endpoint":"http://y","model":"d","api_key":"k"}}"#).unwrap();
        assert!(c.enabled);
        assert_eq!(c.backend, "ollama");
    }

    #[test]
    fn prompt_optimize_and_finalize() {
        assert_eq!(build_prompt("optimize"), OPTIMIZE_PROMPT);
        assert_eq!(build_prompt("finalize"), FINALIZE_PROMPT);
        assert_ne!(OPTIMIZE_PROMPT, FINALIZE_PROMPT);
    }

    #[test]
    fn prompt_unknown_defaults_optimize() {
        assert_eq!(build_prompt("garbage"), OPTIMIZE_PROMPT);
    }
}
