//! ct-punc 标点管线状态机（C 候选）。
//!
//! 把原 `lib.rs::spawn_punc_thread` ~160 行闭包里的「句→批→标点→降级」业务规则抽成
//! **纯状态机**，线程壳（`spawn_punc_thread`）只剩 IO 时序（`recv_timeout` / `emit` /
//! `request_punc`+`recv`+降级）。决策全可单测，IO 全留壳——「interface is the test surface」
//! 在此重新成立（现状：纯函数 `should_punc_flush` 拽出来单测了，真正出 bug 的 drain/grace/
//! 就绪首标全在无测试闭包里）。
//!
//! **边界**：pipeline 只持业务状态（`raw_full` 累积原文 + `pending_chars` 计数），不持时钟
//! （`elapsed` 作 `decide_flush` 参数）、不读 atomic（`ready` 作参数）、不碰 `transcription_text`
//! （共享显示状态，线程壳管）。标点结果**不回填** `raw_full`——整体重跑语义下 `raw_full` 永是
//! 原文（每次 flush 都对全文原文重标点，ct-punc 幂等），故 pipeline 无 `apply_punc`。

/// 触发一次 ct-punc 整体重标点的时间阈值（距上次 flush 毫秒）。
const PUNC_INTERVAL_MS: u64 = 1500;
/// 触发一次 ct-punc 整体重标点的字数阈值（自上次 flush 新增字数）。
const PUNC_CHAR_THRESHOLD: usize = 30;

/// pipeline 产出的动作：线程壳 `match` 后执行对应 IO（emit / request_punc+recv+降级）。
///
/// 动作自携带线程壳执行所需的全部数据（句子 / 待标点全文），pipeline 不持有也不回调任何 IO 句柄。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PuncAction {
    /// 无动作（就绪态累积中、未达阈值、未就绪由 `push` 直写外的静默）。
    Nothing,
    /// 未就绪/降级：句直写主字幕。
    /// 线程壳执行：`transcription_text.push_str(sentence)` + emit `text=sentence, is_final=false`。
    PassThrough { sentence: String },
    /// 就绪：对 `text`（= `raw_full` 全文）做一次整体 ct-punc 标点。
    /// 线程壳执行：`request_punc(text)` + `recv`（失败/超时/空 → 用 `text` 原文降级）→
    /// `*transcription_text = 结果` + emit `text="", is_final=false`。
    RequestPunc { text: String },
    /// 收尾：emit `is_final=true`（线程壳 `full` 取 `transcription_text` 当前值）。
    EmitFinal,
}

/// ct-punc 标点管线状态机（纯：只 `String` + `usize`，无 IO / 无时钟 / 无 atomic）。
#[derive(Debug)]
pub struct PuncPipeline {
    /// 累积原文（就绪后整体送 ct-punc；整体重跑语义下永是原文，不被标点结果覆盖）。
    raw_full: String,
    /// 自上次 flush 起新增字数（仅就绪态累加；`decide_flush` 触发后清零）。
    pending_chars: usize,
}

impl PuncPipeline {
    pub fn new() -> Self {
        Self {
            raw_full: String::new(),
            pending_chars: 0,
        }
    }

    /// 收一句 ASR 原文。
    /// - 就绪（`ready=true`）：累积 `raw_full` + `pending_chars`，返回 `Nothing`（等 `decide_flush` 决策）。
    /// - 未就绪（`ready=false`）：累积 `raw_full`（保就绪后首次整体标点含已直写部分），返回 `PassThrough`（线程壳直写）。
    pub fn push(&mut self, s: &str, ready: bool) -> PuncAction {
        self.raw_full.push_str(s);
        if ready {
            self.pending_chars = self.pending_chars.saturating_add(s.chars().count());
            PuncAction::Nothing
        } else {
            PuncAction::PassThrough {
                sentence: s.to_string(),
            }
        }
    }

    /// 就绪态达阈值的 flush 决策（线程壳在 `Ok(s)` 后及静音 `Timeout` 时调）。
    /// `elapsed_ms` = 距上次 flush 毫秒（线程壳持 `last_flush`，提供 `last_flush.elapsed()`）。
    /// 达阈值 → `RequestPunc { raw_full }` 并清零 `pending_chars`；否则 `Nothing`。
    pub fn decide_flush(&mut self, elapsed_ms: u64, ready: bool) -> PuncAction {
        if ready && should_punc_flush(elapsed_ms, self.pending_chars) {
            self.pending_chars = 0;
            PuncAction::RequestPunc {
                text: self.raw_full.clone(),
            }
        } else {
            PuncAction::Nothing
        }
    }

    /// 收尾决策（线程壳在 stop grace 结束 / `Disconnected` 后调）。
    /// - 就绪且 `raw_full` 非空：`RequestPunc { raw_full }`（线程壳做最终整体标点 + emit flushed + emit final）。
    /// - 否则（未就绪 / 空）：`EmitFinal`（线程壳直接 emit is_final；未就绪已逐句直写）。
    pub fn finalize(&mut self, ready: bool) -> PuncAction {
        if ready && !self.raw_full.is_empty() {
            self.pending_chars = 0;
            PuncAction::RequestPunc {
                text: self.raw_full.clone(),
            }
        } else {
            PuncAction::EmitFinal
        }
    }
}

impl Default for PuncPipeline {
    fn default() -> Self {
        Self::new()
    }
}

/// 是否触发一次 ct-punc 重标点（纯函数，pipeline 内部决策用；从 `lib.rs` 搬来，单一真相源）。
///
/// `elapsed_ms` = 距上次 flush 毫秒；`pending_chars` = 自上次 flush 起新增字数。
/// 无新增 → 不触发；达时间阈值或字数阈值（且非 0）→ 触发。
fn should_punc_flush(elapsed_ms: u64, pending_chars: usize) -> bool {
    pending_chars > 0 && (elapsed_ms >= PUNC_INTERVAL_MS || pending_chars >= PUNC_CHAR_THRESHOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── should_punc_flush（从 lib.rs 搬来，单一真相源）──

    #[test]
    fn punc_flush_no_new_chars_false() {
        assert!(!should_punc_flush(10_000, 0));
    }

    #[test]
    fn punc_flush_by_interval() {
        assert!(should_punc_flush(PUNC_INTERVAL_MS, 5));
        assert!(should_punc_flush(PUNC_INTERVAL_MS + 500, 5));
    }

    #[test]
    fn punc_flush_by_char_threshold() {
        assert!(should_punc_flush(100, PUNC_CHAR_THRESHOLD));
        assert!(should_punc_flush(0, PUNC_CHAR_THRESHOLD + 10));
    }

    #[test]
    fn punc_flush_below_both_false() {
        assert!(!should_punc_flush(PUNC_INTERVAL_MS - 100, PUNC_CHAR_THRESHOLD - 5));
        assert!(!should_punc_flush(0, 1));
    }

    // ── push ──

    #[test]
    fn push_ready_accumulates_returns_nothing() {
        let mut p = PuncPipeline::new();
        assert_eq!(p.push("你好", true), PuncAction::Nothing);
        assert_eq!(p.push("世界", true), PuncAction::Nothing);
        // 累积在 raw_full，finalize 可见
        match p.finalize(true) {
            PuncAction::RequestPunc { text } => assert_eq!(text, "你好世界"),
            other => panic!("就绪非空应 RequestPunc，得到 {:?}", other),
        }
    }

    #[test]
    fn push_not_ready_returns_passthrough() {
        let mut p = PuncPipeline::new();
        assert_eq!(
            p.push("你好", false),
            PuncAction::PassThrough {
                sentence: "你好".into()
            }
        );
    }

    #[test]
    fn push_raw_full_accumulates_across_ready_toggle() {
        // 未就绪直写期也累积 raw_full，就绪后首次 finalize 含已直写部分
        let mut p = PuncPipeline::new();
        p.push("A", false);
        p.push("B", false);
        p.push("C", true);
        match p.finalize(true) {
            PuncAction::RequestPunc { text } => assert_eq!(text, "ABC"),
            other => panic!("跨就绪切换应累积全部，得到 {:?}", other),
        }
    }

    // ── decide_flush ──

    #[test]
    fn decide_flush_at_char_threshold_request() {
        let mut p = PuncPipeline::new();
        p.push(&"x".repeat(PUNC_CHAR_THRESHOLD), true); // 达字数阈值
        let act = p.decide_flush(0, true); // elapsed=0 仍触发（字数达标）
        assert!(matches!(act, PuncAction::RequestPunc { .. }));
    }

    #[test]
    fn decide_flush_at_interval_request() {
        let mut p = PuncPipeline::new();
        p.push("短", true); // pending_chars=1
        match p.decide_flush(PUNC_INTERVAL_MS + 100, true) {
            PuncAction::RequestPunc { text } => assert_eq!(text, "短"),
            other => panic!("达时间阈值应 RequestPunc，得到 {:?}", other),
        }
    }

    #[test]
    fn decide_flush_below_threshold_nothing() {
        let mut p = PuncPipeline::new();
        p.push("短", true);
        assert_eq!(p.decide_flush(100, true), PuncAction::Nothing);
    }

    #[test]
    fn decide_flush_resets_pending_chars() {
        let mut p = PuncPipeline::new();
        p.push(&"x".repeat(PUNC_CHAR_THRESHOLD), true);
        let _ = p.decide_flush(0, true); // 触发 flush → 清零 pending_chars
        p.push("y", true); // pending_chars=1，未达阈值
        assert_eq!(p.decide_flush(0, true), PuncAction::Nothing);
    }

    #[test]
    fn decide_flush_not_ready_nothing() {
        let mut p = PuncPipeline::new();
        p.push(&"x".repeat(PUNC_CHAR_THRESHOLD + 10), true);
        // 未就绪：即使累积超阈值也不 RequestPunc（未就绪走 push 的 passthrough）
        assert_eq!(p.decide_flush(PUNC_INTERVAL_MS, false), PuncAction::Nothing);
    }

    // ── finalize ──

    #[test]
    fn finalize_ready_nonempty_request() {
        let mut p = PuncPipeline::new();
        p.push("你好世界", true);
        match p.finalize(true) {
            PuncAction::RequestPunc { text } => assert_eq!(text, "你好世界"),
            other => panic!("就绪非空 finalize 应 RequestPunc，得到 {:?}", other),
        }
    }

    #[test]
    fn finalize_not_ready_emit_final() {
        let mut p = PuncPipeline::new();
        p.push("你好", false); // 未就绪直写
        assert_eq!(p.finalize(false), PuncAction::EmitFinal);
    }

    #[test]
    fn finalize_empty_emit_final() {
        let mut p = PuncPipeline::new();
        assert_eq!(p.finalize(true), PuncAction::EmitFinal);
    }
}
