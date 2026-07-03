"""一次性验证 funasr AutoModel 的 paraformer-zh-streaming 流式加载与调用方式。
运行: python verify_api.py <test.wav>
成功则打印模型加载耗时、流式逐块增量文本、is_final 刷尾文本，并确认调用 API。

验证点（与 asr_server.py 同路径）：
  - model id = "paraformer-zh-streaming" (iic/Paraformer-online 已 404 不可用)
  - generate(input=chunk, cache=cache, is_final=bool, chunk_size=[0,10,5],
             encoder_chunk_look_back=4, decoder_chunk_look_back=1)
  - cache 跨 chunk 复用（模型原地更新），is_final=True 刷尾
  - 返回 list[dict]，dict keys=['key','text']，text 为该 chunk 新增文本（无富标签前缀）
"""
import sys, time, os
import numpy as np


def main():
    if len(sys.argv) < 2:
        print("用法: python verify_api.py <test.wav>")
        sys.exit(1)
    wav_path = sys.argv[1]
    assert os.path.exists(wav_path), f"文件不存在: {wav_path}"

    import soundfile as sf
    import librosa

    # --- 加载 paraformer-zh-streaming（首跑下载 PyTorch 权重，~90s；缓存后 ~10s）---
    from funasr import AutoModel
    t = time.time()
    model = AutoModel(
        model="paraformer-zh-streaming",
        model_revision="v2.0.4",
        disable_update=True,
    )
    print(f"[ASR] 加载耗时 {time.time()-t:.1f}s")

    # --- 读 16k mono ---
    wav, sr = sf.read(wav_path, dtype="float32")
    if wav.ndim > 1:
        wav = wav.mean(axis=1)
    if sr != 16000:
        wav = librosa.resample(wav, orig_sr=sr, target_sr=16000)
    print(f"[ASR] 音频 {len(wav)} samples = {len(wav)/16000:.2f}s @16k")

    # --- 流式逐块识别（与 asr_server.py 同配置）---
    chunk_size = [0, 10, 5]
    chunk_stride = chunk_size[1] * 960  # 9600 samples = 600ms
    cache = {}
    texts = []
    i = 0
    n = len(wav)
    chunk_idx = 0
    while i < n:
        chunk = wav[i : i + chunk_stride]
        i += chunk_stride
        is_final = i >= n
        res = model.generate(
            input=chunk,
            cache=cache,
            is_final=is_final,
            chunk_size=chunk_size,
            encoder_chunk_look_back=4,
            decoder_chunk_look_back=1,
        )
        for item in res or []:
            t = item.get("text", "") if isinstance(item, dict) else str(item or "")
            if t.strip():
                texts.append(t)
                print(f"[ASR] chunk {chunk_idx} (is_final={is_final}): {t!r} | cache.keys={list(cache.keys())}")
        chunk_idx += 1

    full = "".join(texts)
    print(f"[ASR] 调用方式: generate(input=chunk, cache=cache, is_final=bool, chunk_size=[0,10,5], ...)")
    print(f"[ASR] 完整文本: {full}")
    print(f"[ASR] cache 最终 keys: {list(cache.keys())}")
    print("[OK] 验证完成")


if __name__ == "__main__":
    main()
