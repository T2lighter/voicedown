//! 音频 DSP 纯函数（E 候选：从 `audio_capture.rs` 抽出的数字信号处理层）。
//!
//! **纯、无状态**：下采样（FIR decimation）+ RMS 静音检测。仅 ASR 旁路用（WAV 存档仍存
//! 48k 原始）。阈值作参数（env 读取属 IO 层，留 `audio_capture`），故本模块零副作用可单测。
//!
//! **边界**：不读 env、不持时钟、不碰通道——纯 `&[f32] → Vec<f32>` / `&[f32], f32 → bool`。

/// 把 48k mono 下采样到 16k（3:1 整数 decimation，ISSUE-6/REM-06）。
///
/// FIR 低通（sinc + Hamming 窗，截止 ~7kHz）抗混叠后每 3 取 1，等效线性相位 decimator。
/// **纯函数无状态**：每块独立滤波，块边界 zero-pad 瞬态 <0.4ms（300ms 块占比 <0.2%，ASR 无感）。
/// 仅 ASR 旁路用：WAV 存档仍存 48k 原始（`accumulated`），仅送 Python 的块下采样到 16k，
/// 削减 ~3× 跨进程管道冗余 + 省去 Python librosa.resample 开销。
pub(crate) fn downsample_48k_to_16k(input: &[f32]) -> Vec<f32> {
    const M: usize = 3;
    let h = fir_decimator_coeffs();
    let hlen = h.len();
    let n = input.len();
    let n_out = n.div_ceil(M);
    let mut out = Vec::with_capacity(n_out);
    for k in 0..n_out {
        let base = k * M; // y[k] = Σ_j h[j]·input[k*M + j]，对齐 FIR 中心
        if base >= n {
            break;
        }
        let mut acc = 0.0f32;
        for j in 0..hlen {
            let idx = base + j;
            acc += h[j] * if idx < n { input[idx] } else { 0.0 };
        }
        out.push(acc);
    }
    out
}

/// 能量门控 VAD-lite——判定一块音频是否静音（RMS < 阈值，ISSUE-7/REM-07）。
///
/// Rust 侧旁路预过滤（**不进流式 generate loop**，避 FSMN-VAD #2231 协同异常）：
/// `run_capture` 对下采样后的 16k 块算 RMS，静音块降频喂（保 ParaformerStreaming cache
/// 上下文，恢复说话不丢首字），省去对纯静音的无意义前向。空样本 → 安全默认 silent。
pub(crate) fn is_silent(samples: &[f32], threshold: f32) -> bool {
    if samples.is_empty() {
        return true;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    let rms = (sum_sq / samples.len() as f32).sqrt();
    rms < threshold
}

/// 3:1 decimator 的抗混叠低通 FIR 系数（sinc + Hamming 窗，截止 7kHz @48k，ISSUE-6）。
/// 标准窗法（等效 scipy.signal.firwin）：`h[n] = sinc(2·fc_norm·(n-mid)) · hamming(n)`，
/// 直流增益归一化为 1。47 tap 保证 8k+ 阻带衰减 >40dB（12k 实测 ~0.001）。
fn fir_decimator_coeffs() -> Vec<f32> {
    const N: usize = 47; // taps（奇数，线性相位）
    const FC: f64 = 7000.0 / 48000.0; // 归一化截止（Nyquist=0.5）
    let two_fc = 2.0 * FC;
    let mid = (N as f64 - 1.0) / 2.0;
    let mut h = Vec::with_capacity(N);
    for i in 0..N {
        let t = two_fc * (i as f64 - mid); // 标准归一化 sinc 参数
        let sinc = if t.abs() < 1e-9 {
            1.0
        } else {
            let arg = std::f64::consts::PI * t;
            arg.sin() / arg
        };
        let w = 0.54 - 0.46 * (2.0 * std::f64::consts::PI * i as f64 / (N as f64 - 1.0)).cos();
        h.push((sinc * w) as f32);
    }
    let sum: f32 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= sum);
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsample_preserves_1khz_sine() {
        // ISSUE-6：48k 1kHz 正弦下采样到 16k 应保持 1kHz（语音主能量段，保真铁律）
        use std::f32::consts::PI;
        let fs = 48000.0f32;
        let n = 4800; // 100ms @ 48k
        let input: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / fs).sin())
            .collect();
        let out = downsample_48k_to_16k(&input);
        // 输出 ~1600 样本（100ms @16k）
        assert!((out.len() as i32 - 1600).abs() <= 1, "len={} want~1600", out.len());
        // 频率保持：100ms 内 1kHz = 100 周期 = 200 过零
        let zeros = out.windows(2).filter(|w| w[0] * w[1] < 0.0).count();
        assert!(
            (zeros as i32 - 200).abs() <= 6,
            "zeros={} want~200 (got {} samples)",
            zeros,
            out.len()
        );
    }

    #[test]
    fn downsample_length_and_dc_gain() {
        // ISSUE-6：长度精确 3:1；常数序列 DC 增益=1（中间样本避开边界 zero-pad 瞬态）
        let input: Vec<f32> = vec![0.0f32; 14400]; // 300ms @48k
        let out = downsample_48k_to_16k(&input);
        assert_eq!(out.len(), 4800, "14400 → 4800, got {}", out.len());

        let dc: Vec<f32> = vec![0.5f32; 3000];
        let out_dc = downsample_48k_to_16k(&dc);
        let mid = out_dc.len() / 2;
        assert!(
            (out_dc[mid] - 0.5).abs() < 0.01,
            "DC gain mid={} want~0.5",
            out_dc[mid]
        );
    }

    #[test]
    fn downsample_attenuates_aliasing() {
        // ISSUE-6：12kHz 下采样到 16k 系统会混叠到 4k，FIR 低通（截止 7k）应大幅抑制 12k
        use std::f32::consts::PI;
        let fs = 48000.0f32;
        let n = 4800;
        let make = |freq: f32| -> Vec<f32> {
            (0..n).map(|i| (2.0 * PI * freq * i as f32 / fs).sin()).collect()
        };
        let amp = |v: &[f32]| -> f32 {
            v.iter().skip(20).take(1500).map(|x| x.abs()).fold(0.0f32, f32::max)
        };
        let amp_1k = amp(&downsample_48k_to_16k(&make(1000.0)));
        let amp_12k = amp(&downsample_48k_to_16k(&make(12000.0)));
        assert!(
            amp_12k < amp_1k * 0.3,
            "12k amp={:.3} 未被显著抑制（1k amp={:.3}）",
            amp_12k,
            amp_1k
        );
    }

    #[test]
    fn is_silent_detects_silence() {
        // ISSUE-7：零信号 + 微小底噪 + 空样本 → silent（RMS < 阈值）
        assert!(is_silent(&[0.0; 4800], 0.01));
        assert!(is_silent(&[0.001; 4800], 0.01)); // RMS 0.001 < 0.01
        assert!(is_silent(&[], 0.01)); // 空样本 → 安全默认 silent
    }

    #[test]
    fn is_silent_detects_speech_and_boundary() {
        // ISSUE-7：正弦 RMS = a/√2；阈值两侧验证 < 语义（避精确边界浮点抖动）
        use std::f32::consts::PI;
        let fs = 16000.0f32;
        let sine = |a: f32| -> Vec<f32> {
            (0..4800)
                .map(|i| a * (2.0 * PI * 1000.0 * i as f32 / fs).sin())
                .collect()
        };
        // 强语音 a=1.0 → RMS 0.707 >> 0.01 → not silent
        assert!(!is_silent(&sine(1.0), 0.01));
        // 弱语音 a=0.05 → RMS ≈0.035 > 0.02 → not silent（边界外侧）
        assert!(!is_silent(&sine(0.05), 0.02));
        // 底噪 a=0.005 → RMS ≈0.0035 < 0.02 → silent（边界内侧）
        assert!(is_silent(&sine(0.005), 0.02));
    }
}
