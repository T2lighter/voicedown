//! ISSUE-3：导出配置（可配置导出）。
//!
//! **无条件编译**（不挂 `asr` feature）——导出在纯音频模式（`--no-default-features`）也要可用，
//! 而 ⚙ 依赖 asr-gated 的 `get_llm_config`，故 ExportConfig 独立本模块、独立 📤 弹窗。
//! 持久化范式镜像 `text_optimizer`（config_path/load/save + 缺失·损坏降级默认）。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 文本导出格式（原文 + 优化文本共用，单选）。serde 序列化为 `"txt"` / `"md"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextFormat {
    Txt,
    Md,
}

impl Default for TextFormat {
    fn default() -> Self {
        TextFormat::Txt
    }
}

/// 导出配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportConfig {
    /// 是否导出（默认 true，沿用现状）。
    pub enabled: bool,
    /// 文本格式（默认 Txt）。
    pub text_format: TextFormat,
    /// 导出目录（默认 `%USERPROFILE%\Documents\VoiceDown`）。
    pub export_dir: String,
}

impl Default for ExportConfig {
    fn default() -> Self {
        let user_profile = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string());
        Self {
            enabled: true,
            text_format: TextFormat::default(),
            export_dir: format!("{}\\Documents\\VoiceDown", user_profile),
        }
    }
}

// 持久化已迁 json_config_store（D 候选）：load/save 经 JsonConfigStore::default()，
// 文件名 "export_config"。config_path/parse_config/load_config/save_config 已删。

/// 路径校验错误（前端按变体显示不同红字文案）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportPathError {
    /// 为空或含 Windows 非法字符 / 控制字符。
    InvalidChars,
    /// 指向已存在的文件（非目录）。
    NotADirectory,
    /// 不可写（权限不足 / 父路径或盘不存在）。
    Unwritable,
    /// 创建/探针失败（含原始 OS 错误，如磁盘满）。
    CreateFailed(String),
}

impl std::fmt::Display for ExportPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportPathError::InvalidChars => write!(f, "路径为空或含非法字符（<>:\"|?* 或控制字符）"),
            ExportPathError::NotADirectory => write!(f, "路径指向的是文件，不是目录"),
            ExportPathError::Unwritable => write!(f, "目录不可写（权限不足或盘/父路径不存在）"),
            ExportPathError::CreateFailed(e) => write!(f, "创建目录失败: {}", e),
        }
    }
}

/// 校验导出目录：合法可写 → `Ok(PathBuf)`；否则对应 `ExportPathError`。
///
/// 让 OS 当裁判，避免重实现 Windows 路径规则：空/预检非法字符（盘符 `:` 保留）→ InvalidChars；
/// 指向文件 → NotADirectory；不存在 → `create_dir_all` 自动建（失败按 kind 映射）；
/// 最后写探针文件验可写（失败按 kind 映射）。保存时 + 导出时双校验（PRD）。
pub fn validate_export_dir(path: &str) -> Result<PathBuf, ExportPathError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(ExportPathError::InvalidChars);
    }
    // 预检 Windows 非法字符（盘符 ':' 保留，因 C:\ 合法）+ 控制字符
    if trimmed
        .chars()
        .any(|c| matches!(c, '<' | '>' | '"' | '|' | '?' | '*') || c.is_control())
    {
        return Err(ExportPathError::InvalidChars);
    }
    let p = PathBuf::from(trimmed);
    if p.is_file() {
        return Err(ExportPathError::NotADirectory);
    }
    if !p.exists() {
        if let Err(e) = std::fs::create_dir_all(&p) {
            return Err(map_io_err(&e));
        }
    }
    // 可写探针：建临时文件 + 删
    let probe = p.join(".voicedown_probe");
    if let Err(e) = std::fs::File::create(&probe) {
        return Err(map_io_err(&e));
    }
    let _ = std::fs::remove_file(&probe);
    Ok(p)
}

fn map_io_err(e: &std::io::Error) -> ExportPathError {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => ExportPathError::Unwritable,
        std::io::ErrorKind::NotFound => ExportPathError::Unwritable,
        _ => ExportPathError::CreateFailed(e.to_string()),
    }
}

// ── 导出产物决策（ISSUE-4）──────────────────────────────

/// 单个文本产物（文件名 + 已按格式渲染的内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub filename: String,
    pub content: String,
}

/// 导出产物决策（`export_plan` 输出）：各字段 `None` = 该产物不产出（未启用 / 无数据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactsPlan {
    pub wav: Option<String>,
    pub text: Option<Artifact>,
    pub optimized: Option<Artifact>,
}

/// 渲染 markdown：flat 内容 + 顶部 `#` 标题（**非**离线定稿三级结构化，ADR 0001/0002）。
/// 末尾空白 trim，避免多余空行。
pub fn render_md(text: &str, title: &str) -> String {
    format!("# {}\n\n{}", title, text.trim_end())
}

/// 按格式渲染文本：Txt 原样 / Md 调 `render_md`（标题用 `base`，与文件名同源）。
fn render_text(text: &str, format: TextFormat, title: &str) -> String {
    match format {
        TextFormat::Txt => text.to_string(),
        TextFormat::Md => render_md(text, title),
    }
}

/// 导出产物决策（纯函数）。
///
/// - `enabled=false` → 全 `None`（零文件）；
/// - `text_format` 决定扩展名（`.txt` / `.md`）与内容（原样 / `render_md`）；
/// - `has_audio` / `transcription` / `optimized` 各自独立门控（无音频仍写文本，反之亦然）。
///
/// `base` = 文件名前缀（如 `capture_<ts>`），亦作 md 的 H1 标题。
/// `transcription` / `optimized` 用 `Option<&str>`：`Some` 同时编码「有数据 + 内容」。
pub fn export_plan(
    cfg: &ExportConfig,
    base: &str,
    has_audio: bool,
    transcription: Option<&str>,
    optimized: Option<&str>,
) -> ArtifactsPlan {
    if !cfg.enabled {
        return ArtifactsPlan {
            wav: None,
            text: None,
            optimized: None,
        };
    }
    let ext = match cfg.text_format {
        TextFormat::Txt => "txt",
        TextFormat::Md => "md",
    };
    let wav = if has_audio {
        Some(format!("{}.wav", base))
    } else {
        None
    };
    let text = transcription.map(|t| Artifact {
        filename: format!("{}.{}", base, ext),
        content: render_text(t, cfg.text_format, base),
    });
    let optimized = optimized.map(|o| Artifact {
        filename: format!("{}_optimized.{}", base, ext),
        content: render_text(o, cfg.text_format, base),
    });
    ArtifactsPlan {
        wav,
        text,
        optimized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enabled_txt_documents_dir() {
        let c = ExportConfig::default();
        assert!(c.enabled, "默认沿用现状：开启导出");
        assert_eq!(c.text_format, TextFormat::Txt);
        assert!(
            c.export_dir.ends_with("Documents\\VoiceDown"),
            "默认导出目录 = Documents\\VoiceDown，实际: {}",
            c.export_dir
        );
    }

    #[test]
    fn serde_roundtrip_txt_and_md() {
        for (tf, expect) in [(TextFormat::Txt, "txt"), (TextFormat::Md, "md")] {
            let cfg = ExportConfig {
                enabled: true,
                text_format: tf,
                export_dir: "C:\\out".into(),
            };
            let s = serde_json::to_string(&cfg).unwrap();
            assert!(
                s.contains(&format!("\"text_format\":\"{}\"", expect)),
                "{}",
                s
            );
            let back: ExportConfig = serde_json::from_str(&s).unwrap();
            assert_eq!(back.text_format, tf);
            assert!(back.enabled);
            assert_eq!(back.export_dir, "C:\\out");
        }
    }


    // 测试函数名唯一 → 并发安全（cargo test 默认多线程）。
    fn unique_temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("voicedown_export_{}", name))
    }

    #[test]
    fn validate_ok_creates_missing_dir() {
        let root = unique_temp("ok_nested");
        let dir = root.join("deep");
        let _ = std::fs::remove_dir_all(&root); // 清理上次残留
        assert!(!dir.exists());
        let res = validate_export_dir(dir.to_str().unwrap());
        assert!(res.is_ok(), "合法路径应通过: {:?}", res);
        assert!(dir.exists(), "应自动创建目录");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_existing_file_not_a_directory() {
        let file = unique_temp("notadir_file");
        let _ = std::fs::remove_file(&file);
        std::fs::write(&file, b"x").unwrap();
        let res = validate_export_dir(file.to_str().unwrap());
        assert_eq!(res.unwrap_err(), ExportPathError::NotADirectory);
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn validate_invalid_chars_rejected() {
        let res = validate_export_dir("C:\\foo<bar");
        assert_eq!(res.unwrap_err(), ExportPathError::InvalidChars);
        // 控制字符
        let res2 = validate_export_dir("C:\\foo\nbar");
        assert_eq!(res2.unwrap_err(), ExportPathError::InvalidChars);
    }

    #[test]
    fn validate_empty_rejected() {
        assert_eq!(
            validate_export_dir("").unwrap_err(),
            ExportPathError::InvalidChars
        );
        assert_eq!(
            validate_export_dir("   ").unwrap_err(),
            ExportPathError::InvalidChars
        );
    }

    #[test]
    fn validate_existing_writable_dir_ok() {
        // temp_dir 本身存在且可写
        let res = validate_export_dir(std::env::temp_dir().to_str().unwrap());
        assert!(res.is_ok(), "已存在可写目录应通过: {:?}", res);
    }

    // ── render_md ──

    #[test]
    fn render_md_content_with_h1() {
        let md = render_md("正文内容", "标题");
        assert!(md.starts_with("# 标题"), "{}", md);
        assert!(md.contains("正文内容"), "{}", md);
    }

    #[test]
    fn render_md_trims_trailing_whitespace() {
        let md = render_md("正文\n\n", "标题");
        assert!(!md.ends_with('\n'), "应 trim 末尾空白: {}", md);
        assert!(md.contains("正文"));
    }

    #[test]
    fn render_md_empty_content_safe() {
        let md = render_md("", "标题");
        assert!(md.starts_with("# 标题"));
    }

    // ── export_plan ──

    #[test]
    fn export_plan_disabled_all_none() {
        let cfg = ExportConfig {
            enabled: false,
            text_format: TextFormat::Txt,
            export_dir: "C:\\o".into(),
        };
        let plan = export_plan(&cfg, "capture_x", true, Some("t"), Some("o"));
        assert!(plan.wav.is_none());
        assert!(plan.text.is_none());
        assert!(plan.optimized.is_none());
    }

    #[test]
    fn export_plan_txt_ext_and_raw_content() {
        let cfg = ExportConfig {
            enabled: true,
            text_format: TextFormat::Txt,
            export_dir: "C:\\o".into(),
        };
        let plan = export_plan(&cfg, "capture_x", true, Some("原文"), Some("优化文"));
        assert_eq!(plan.wav.as_deref(), Some("capture_x.wav"));
        let text = plan.text.expect("有转录应有 text 产物");
        assert_eq!(text.filename, "capture_x.txt");
        assert_eq!(text.content, "原文"); // Txt 原样
        let opt = plan.optimized.expect("有优化应有 optimized 产物");
        assert_eq!(opt.filename, "capture_x_optimized.txt");
        assert_eq!(opt.content, "优化文");
    }

    #[test]
    fn export_plan_md_ext_and_h1_content() {
        let cfg = ExportConfig {
            enabled: true,
            text_format: TextFormat::Md,
            export_dir: "C:\\o".into(),
        };
        let plan = export_plan(&cfg, "capture_x", true, Some("原文"), Some("优化文"));
        assert_eq!(plan.wav.as_deref(), Some("capture_x.wav"));
        let text = plan.text.expect("md 也应有 text 产物");
        assert_eq!(text.filename, "capture_x.md");
        assert!(
            text.content.starts_with("# capture_x"),
            "md 内容应以 H1(base) 开头: {}",
            text.content
        );
        assert!(text.content.contains("原文"));
        assert_eq!(
            plan.optimized
                .expect("md 也应有 optimized 产物")
                .filename,
            "capture_x_optimized.md"
        );
    }

    #[test]
    fn export_plan_no_audio_wav_none_text_kept() {
        let cfg = ExportConfig {
            enabled: true,
            text_format: TextFormat::Txt,
            export_dir: "C:\\o".into(),
        };
        let plan = export_plan(&cfg, "b", false, Some("t"), None);
        assert!(plan.wav.is_none(), "无音频应无 wav");
        assert!(plan.text.is_some(), "有转录应保留 text");
    }

    #[test]
    fn export_plan_no_transcription_text_none_wav_kept() {
        let cfg = ExportConfig {
            enabled: true,
            text_format: TextFormat::Txt,
            export_dir: "C:\\o".into(),
        };
        let plan = export_plan(&cfg, "b", true, None, None);
        assert!(plan.text.is_none(), "无转录应无 text");
        assert!(plan.wav.is_some(), "有音频应保留 wav");
    }

    #[test]
    fn export_plan_no_optimized_optimized_none() {
        let cfg = ExportConfig {
            enabled: true,
            text_format: TextFormat::Txt,
            export_dir: "C:\\o".into(),
        };
        let plan = export_plan(&cfg, "b", true, Some("t"), None);
        assert!(plan.optimized.is_none(), "无优化应无 optimized 产物");
        assert!(plan.text.is_some());
    }
}
