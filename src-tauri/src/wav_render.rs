//! WAV 字节渲染（E 候选：从 `audio_capture.rs` 抽出的纯渲染层）。
//!
//! 与文件 I/O 解耦的纯渲染：f32 样本流 → 16-bit mono PCM WAV，写入任意 `Write`。
//! `audio_capture::AudioCapturer::save_to_wav` 用它写入 `BufWriter`（不 clone `accumulated`、
//! 不 materialize 大 Vec，逐样本小写经 BufWriter 缓冲而非逐次系统调用）；测试用 `Vec<u8>`
//! 当 sink 验证字节布局。
//!
//! **边界**：本模块零系统调用、零副作用（采样率作参数，不读 `CAPTURE_SAMPLE_RATE` 常量）。
//! env 读取属 IO 层，不在此。

use std::io::Write;

/// 把 f32 样本流渲染成 16-bit mono PCM WAV，写入任意 `Write`。
///
/// 纯渲染：RIFF/WAVE/fmt 头 + `f32[-1,1] clamp → i16 LE`。
pub(crate) fn render_wav<W: Write>(w: &mut W, samples: &[f32], rate: u32) -> std::io::Result<()> {
    let n = samples.len() as u32;
    let data_size = n * 2; // 16-bit mono = 2 bytes/sample
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_size).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // fmt chunk 大小
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&1u16.to_le_bytes())?; // 单声道
    w.write_all(&rate.to_le_bytes())?; // 采样率
    w.write_all(&(rate * 2).to_le_bytes())?; // byte_rate = rate * block_align
    w.write_all(&2u16.to_le_bytes())?; // block_align = 2 (16-bit mono)
    w.write_all(&16u16.to_le_bytes())?; // bits_per_sample
    w.write_all(b"data")?;
    w.write_all(&data_size.to_le_bytes())?;
    // PCM 数据 (f32 [-1,1] → i16)
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        w.write_all(&v.to_le_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ISSUE-1：render_wav 纯函数（WAV 字节渲染）──
    // 渲染逻辑从 save_to_wav 抽出，写入任意 Write；测试用 Vec<u8> 当 sink 验证字节布局。

    #[test]
    fn render_wav_full_layout_three_samples() {
        // tracer：多样本 → 完整 WAV 字节布局（头 + data_size + 3 样本 i16 LE）
        let mut buf = Vec::new();
        render_wav(&mut buf, &[0.0, 1.0, -1.0], 48000).unwrap();
        // 44 字节头 + 3 样本 × 2 字节 = 50
        assert_eq!(buf.len(), 50);
        assert_eq!(&buf[0..4], b"RIFF");
        assert_eq!(&buf[8..12], b"WAVE");
        assert_eq!(&buf[12..16], b"fmt ");
        assert_eq!(&buf[36..40], b"data");
        // data_size = 6（3 样本 × 2）
        assert_eq!(u32::from_le_bytes([buf[40], buf[41], buf[42], buf[43]]), 6);
        // 样本：0.0→0x0000，1.0→0x7FFF(LE FF 7F)，-1.0→0x8001(LE 01 80)
        assert_eq!(&buf[44..46], &[0x00, 0x00]);
        assert_eq!(&buf[46..48], &[0xFF, 0x7F]);
        assert_eq!(&buf[48..50], &[0x01, 0x80]);
    }

    #[test]
    fn render_wav_sample_conversion() {
        // 边界样本 → i16 LE：1.0 → 32767(0x7FFF)；-1.0 → -32767(0x8001)
        let mut hi = Vec::new();
        render_wav(&mut hi, &[1.0], 48000).unwrap();
        assert_eq!(&hi[44..46], &[0xFF, 0x7F]);
        let mut lo = Vec::new();
        render_wav(&mut lo, &[-1.0], 48000).unwrap();
        assert_eq!(&lo[44..46], &[0x01, 0x80]);
    }

    #[test]
    fn render_wav_clamps_overshoot() {
        // 超量程样本 clamp 到 [-1,1]：2.0 同 1.0；-2.0 同 -1.0（不溢出、不 panic）
        let mut hi = Vec::new();
        render_wav(&mut hi, &[2.0], 48000).unwrap();
        assert_eq!(&hi[44..46], &[0xFF, 0x7F]);
        let mut lo = Vec::new();
        render_wav(&mut lo, &[-2.0], 48000).unwrap();
        assert_eq!(&lo[44..46], &[0x01, 0x80]);
    }

    #[test]
    fn render_wav_writes_parametric_rate() {
        // 采样率来自参数（非硬编码 48000）：rate=16000 → sample_rate=16000、byte_rate=32000
        let mut buf = Vec::new();
        render_wav(&mut buf, &[0.0], 16000).unwrap();
        assert_eq!(
            u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]),
            16000
        );
        assert_eq!(
            u32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]),
            32000
        );
    }

    #[test]
    fn render_wav_empty_samples_valid_header() {
        // 空样本不 panic：产出合法 44 字节头（data_size=0、RIFF chunk size=36），无样本字节
        let mut buf = Vec::new();
        render_wav(&mut buf, &[], 48000).unwrap();
        assert_eq!(buf.len(), 44);
        assert_eq!(&buf[0..4], b"RIFF");
        assert_eq!(&buf[36..40], b"data");
        assert_eq!(u32::from_le_bytes([buf[40], buf[41], buf[42], buf[43]]), 0);
        assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 36);
    }
}
