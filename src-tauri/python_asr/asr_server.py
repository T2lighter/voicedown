#!/usr/bin/env python3
"""VoiceDown ASR 服务 (stdio JSON lines)。

协议:
  {"action":"ping"}                                  -> {"status":"ready","models_loaded":true}
  {"action":"transcribe","wav_path":"...","language":"auto"} -> {"text":"...","error":null}
  {"action":"feed_audio",...}                        (流式, 无同步响应)
  Py->Rust 推送: {"text":"...","is_final":true,"error":null}
  {"action":"exit"}                                  -> 进程退出
  {"action":"flush"}                                 -> 收尾: is_final=True 强制刷出末段

模型: paraformer-zh-streaming (iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online,
      funasr 类 ParaformerStreaming)，真增量流式 ASR。

流式调用 (实测确认, 见 task 探针):
  model.generate(input=chunk, cache=cache, is_final=bool, chunk_size=[0,10,5],
                 encoder_chunk_look_back=4, decoder_chunk_look_back=1)
    - cache: 首次空 dict, 跨 chunk 复用同一实例 (模型原地更新 encoder/decoder/frontend/prev_samples)
    - chunk_size=[0,10,5]: [encoder 左回看, 当前块显示粒度(10×60ms=600ms), decoder 右前瞻(5×60ms=300ms)]
    - chunk_stride = chunk_size[1] * 960 = 9600 样本 = 600ms @16k (960 = 16000×60ms)
    - is_final: 仅末块 True, 触发 tail_threshold 强制刷出剩余 CIF 积分对应的尾文本
    - 返回 list[dict], dict keys=['key','text'] (无 is_final 字段), text 为该 chunk 新增文本 (增量, 无富标签前缀)

  ⚠️ 离线参数 batch_size_s/merge_vad/merge_length_s 不可用于流式逐块调用:
     小块独立调 (无 cache/chunk_size/is_final) 返回空, merge_length_s=30 会把输出推迟到累积 ~30s
     (这正是旧实现"开始捕获后等 30s 才出字幕"的根因)。

  ⚠️ model id 不可用 "iic/Paraformer-online" (funasr 1.3.14 RuntimeError "not registered",
     modelscope 404, 本地仅 44 字节指针文件)。必须用别名 "paraformer-zh-streaming"。
"""
import json
import os
import re
import sys
import time
import warnings
import threading
from contextlib import contextmanager

import numpy as np

warnings.filterwarnings("ignore", category=UserWarning, module="jieba._compat")
os.environ.setdefault("OMP_NUM_THREADS", "8")


@contextmanager
def suppress_stdout():
    """彻底屏蔽 stdout 输出 (Python 层 + 底层 C/进程直接写 fd 1)。

    funasr / modelscope / onnxruntime 在推理/加载时会通过 print 甚至直接
    write(1, ...) 输出进度 (CP936 字节), 会破坏 stdio JSON 协议。本上下文用
    os.dup2 把 fd 1 临时指向 /dev/null (POSIX) 或 nul (Windows), 同时把
    sys.stdout 缓冲区重定向到 stderr, 保证协议干净。
    """
    devnull = os.open(os.devnull, os.O_WRONLY)
    saved_fd = os.dup(1)
    real_stdout = sys.stdout
    try:
        sys.stdout = sys.stderr
        os.dup2(devnull, 1)
        yield
    finally:
        os.dup2(saved_fd, 1)
        sys.stdout = real_stdout
        os.close(saved_fd)
        os.close(devnull)


# 真实 stdout fd 副本：main() 开头 os.dup(1) 保存。所有协议行（result/punc/punc_ready/
# ready/ping/...）都经 emit_json 写此 fd，绕开 suppress_stdout 的 dup2(fd1→devnull)——
# 后者是进程级重定向，ct-punc 后台懒加载的 worker 与主循环 punctuate/_recognize 的
# suppress_stdout 块跨线程交错时，会吞掉走 fd1 的 print（流式 result/flush 完成信号丢失
# → 字幕丢句/停止卡顿）。emit_json 用独立 fd 不受 dup2 影响，是 stdio 协议洁净的基座。
REAL_STDOUT_FD = None  # main() 设置；测试可 monkeypatch；None 时 fallback print


def emit_json(obj):
    """经 REAL_STDOUT_FD 写一行 JSON（绕开 dup2），未初始化时 fallback print。"""
    line = json.dumps(obj, ensure_ascii=False) + "\n"
    if REAL_STDOUT_FD is not None:
        os.write(REAL_STDOUT_FD, line.encode("utf-8"))
    else:
        print(line, flush=True)


# 去掉富标签前缀 (<|zh|><|NEUTRAL|><|Speech|><|woitn|>...)
# ParaformerStreaming 输出无富标签, strip 是无害的防御 (兼容未来换 SenseVoice 等)。
_RICH_TAG_RE = re.compile(r"^(<\|[^|]*\|>)+")


class StreamBuffer:
    """累积 16k 音频, 每满 chunk_stride 样本切一块 (流式喂模型用)。

    feed_audio 每 300ms 收到一块 (48k→16k 重采样后 4800 样本), 但流式模型期望
    chunk_stride=9600 样本 (600ms) 一块。本类把小块累积, 满 stride 切出完整块,
    不足 stride 的尾巴留给下一次或 flush。纯逻辑, 可单测, 不依赖模型。
    """

    def __init__(self, chunk_stride):
        self.chunk_stride = chunk_stride
        self._buf = np.zeros(0, dtype=np.float32)

    def push(self, audio_16k):
        """追加 16k 音频, 返回新切出的完整 chunk 列表 (每个恰 chunk_stride 长)。"""
        self._buf = np.concatenate([self._buf, np.asarray(audio_16k, dtype=np.float32)])
        chunks = []
        while len(self._buf) >= self.chunk_stride:
            chunks.append(self._buf[: self.chunk_stride])
            self._buf = self._buf[self.chunk_stride :]
        return chunks

    def take_remainder(self):
        """取出剩余不足 stride 的尾巴 (flush 时作为末块喂模型), 并清空缓冲。"""
        r = self._buf
        self._buf = np.zeros(0, dtype=np.float32)
        return r

    def reset(self):
        """清空缓冲 (跨会话干净开始)。"""
        self._buf = np.zeros(0, dtype=np.float32)


class FunAsrServer:
    # ParaformerStreaming 流式配置 (官方 README/tutorial 一致)。
    CHUNK_SIZE = [0, 10, 5]  # [encoder 左回看, 显示粒度 600ms, decoder 右前瞻 300ms]
    ENCODER_CHUNK_LOOK_BACK = 4
    DECODER_CHUNK_LOOK_BACK = 1
    CHUNK_STRIDE = CHUNK_SIZE[1] * 960  # 9600 样本 = 600ms @16k

    def __init__(self, model=None, punc_model=None):
        # model 注入（测试接缝）：传 stub model 即跳过真模型加载（load_models 的
        # models_loaded 守卫会 no-op），使 feed/flush 等纯逻辑路径脱离 funasr 被测。
        # 默认 None → main() 真路径由 load_models() 加载 paraformer-zh-streaming。
        # punc_model 同理：注入 stub 即视为标点模型就绪；None → load_models 加载 ct-punc。
        self.model = model
        self.models_loaded = model is not None
        self.punc_model = punc_model
        self.punc_loaded = punc_model is not None
        self._punc_lock = threading.Lock()  # 保护 punc_model/punc_loaded 原子读写（后台懒加载 vs punctuate）
        self._cache = {}  # 跨 chunk 流式状态, 首次空 dict, 会话内复用
        self._buffer = StreamBuffer(self.CHUNK_STRIDE)

    def load_models(self):
        """加载 paraformer-zh-streaming 流式模型（启动即发 ready，不阻塞）。

        ct-punc 标点模型改为后台懒加载（见 load_punc / _load_punc_worker）：启动只加
        paraformer → 立即 ready（~15s，用户可即刻录音）→ daemon 线程后台加载 ct-punc
        → 完成推 punc_ready 信号 → Rust 翻 punc_ready 标志。流式模型加载失败 → 抛出
        （bridge 不可用）。ct-punc 是独立 AutoModel 实例，仅后处理用，不进流式 generate
        loop（FunASR #2231 红线）。
        """
        if self.models_loaded:
            return
        t0 = time.time()
        t_stage = t0

        def _lap(label):
            nonlocal t_stage
            now = time.time()
            print(f"[ASR] {label} 耗时 {now - t_stage:.2f}s", file=sys.stderr)
            t_stage = now

        # modelscope/funasr 下载/加载时会 print 进度到 stdout (甚至直接写 fd 1),
        # 污染 stdio JSON 协议。用 dup2 彻底屏蔽。
        with suppress_stdout():
            print("[ASR] 加载 paraformer-zh-streaming...")
            from funasr import AutoModel

            self.model = AutoModel(
                model="paraformer-zh-streaming",
                model_revision="v2.0.4",
                disable_update=True,
            )
            _lap("paraformer-zh-streaming 加载")

        self.models_loaded = True
        print(
            f"[ASR] paraformer 就绪 (耗时 {time.time()-t0:.1f}s)，ct-punc 后台懒加载中",
            file=sys.stderr,
        )

    def _make_punc_model(self):
        """真路径：加载 ct-punc AutoModel。测试可 monkeypatch 替换为 stub。

        ⚠️ model id 必须用注册别名 "ct-punc"（=CT-Transformer 标点模型）。曾误用
        modelscope 仓库名 → funasr "is not registered" → 静默降级。在 suppress_stdout
        内调用（防 modelscope/funasr 进度污染 stdio JSON 协议）。
        """
        from funasr import AutoModel

        return AutoModel(model="ct-punc", disable_update=True)

    def load_punc(self):
        """后台懒加载 ct-punc。锁外构造模型（秒级，不阻塞主循环 punctuate），锁内原子
        赋值 punc_model/punc_loaded。返回 (success:bool, error:str|None)。

        幂等：已加载直接返回。失败不置 punc_model=None（单调不变量：一旦 set 非 None
        永不回 None，保证 punctuate 持锁读快照后锁外 generate 安全）。
        """
        with self._punc_lock:
            if self.punc_model is not None:
                return True, None  # 幂等：已加载
        try:
            with suppress_stdout():
                model = self._make_punc_model()
        except Exception as e:
            return False, str(e)
        with self._punc_lock:
            if self.punc_model is None:
                self.punc_model = model
                self.punc_loaded = True
        return True, None

    def _load_punc_worker(self):
        """daemon 线程入口：后台加载 ct-punc，完成后推 punc_ready 信号。

        顶层 try/except 保证任何逃逸异常仍推 ready:false 终态信号（防永久静默降级）。
        信号用 emit_json（绕开 suppress_stdout 的 dup2，跨线程交错不被吞——生死线）。
        """
        ready, error = False, None
        try:
            ready, error = self.load_punc()
        except Exception as e:  # 顶层兜底：保证终态信号
            ready, error = False, str(e)
        if ready:
            print("[ASR] ct-punc 就绪，标点已启用", file=sys.stderr)
            emit_json({"type": "punc_ready", "ready": True})
        else:
            print(f"[ASR] ct-punc 加载失败，标点降级关闭: {error}", file=sys.stderr)
            emit_json({"type": "punc_ready", "ready": False, "error": error or "unknown"})

    def punctuate(self, text):
        """ct-punc 标点恢复（stateless generate）。返回 {"text":..., "error":...}。

        空文本 → 原文直通。持锁读 punc_model 快照后锁外 generate（worker 锁只覆盖赋值，
        微秒级）；锁竞争（worker 正在赋值）时 acquire 超时即原文降级——红线：punctuate
        在主循环内同步调用，绝不因 worker 持锁而长时间阻塞。无模型/异常 → 原文，永不丢字。
        """
        if not text.strip():
            return {"text": text, "error": None}
        if not self._punc_lock.acquire(timeout=0.5):
            return {"text": text, "error": None}  # 锁竞争 → 原文降级（不阻塞主循环）
        model = self.punc_model
        self._punc_lock.release()
        if model is None:
            return {"text": text, "error": None}  # 未就绪 → 原文降级
        try:
            with suppress_stdout():
                res = model.generate(input=text)
            out = "".join(
                (item.get("text", "") if isinstance(item, dict) else str(item or ""))
                for item in (res or [])
            )
            return {"text": out or text, "error": None}
        except Exception as e:
            print(f"[ASR] ct-punc 失败，降级原文: {e}", file=sys.stderr)
            return {"text": text, "error": str(e)}

    def _recognize(self, chunk, is_final, on_result):
        """对流式 chunk 调一次 generate, 把新增文本经 on_result(text, is_final) 推出。

        cache 跨调用复用 (self._cache), 由模型原地更新。is_final=True 触发末段刷尾。
        generate 异常时: 重置 self._cache (防模型半更新 cache 污染后续识别), 并经
        on_result 推 error (走 suppress_stdout 块外的真 stdout, Rust 解析 error 字段
        并 emit asr-error, 避免 Rust 既收不到文本也收不到错误的静默吞)。
        """
        try:
            with suppress_stdout():
                res = self.model.generate(
                    input=chunk,
                    cache=self._cache,
                    is_final=is_final,
                    chunk_size=self.CHUNK_SIZE,
                    encoder_chunk_look_back=self.ENCODER_CHUNK_LOOK_BACK,
                    decoder_chunk_look_back=self.DECODER_CHUNK_LOOK_BACK,
                )
        except Exception as e:
            print(f"[ASR] 识别失败: {e}", file=sys.stderr)
            # 重置 cache: generate 内部逐键原地更新 cache, 异常时可能半更新损坏,
            # 下次用损坏 cache 会持续错乱。重置以干净状态继续。
            self._cache = {}
            # 推 error 到 Rust (此处在 suppress_stdout 块外, stdout 是真 stdout)。
            # on_result 签名固定 error:None, 故直接 print JSON 带 error 字段。
            emit_json({"text": "", "is_final": is_final, "error": f"识别失败: {e}"})
            return
        for item in res or []:
            if isinstance(item, dict):
                text = item.get("text") or ""
            else:
                text = str(item) if item else ""
            text = _RICH_TAG_RE.sub("", text).strip()
            if text:
                on_result(text, is_final)

    def transcribe_file(self, wav_path, language="auto"):
        """文件转写 (测试/回退用)。返回 dict {"text":..., "error":...}。

        用流式 chunk 方式跑完整个文件 (与 feed_audio 同路径), 末块 is_final=True 刷尾。
        language 参数对流式中文模型无意义, 保留以兼容协议。
        """
        if not self.models_loaded:
            return {"text": "", "error": "模型未加载"}
        if not wav_path or not os.path.exists(wav_path):
            return {"text": "", "error": f"文件不存在: {wav_path}"}
        try:
            import librosa
            import soundfile as sf

            with suppress_stdout():
                wav, sr = sf.read(wav_path, dtype="float32")
                if wav.ndim > 1:
                    wav = wav.mean(axis=1)
                if sr != 16000:
                    wav = librosa.resample(wav, orig_sr=sr, target_sr=16000)
                cache = {}
                stride = self.CHUNK_STRIDE
                texts = []
                n = len(wav)
                if n == 0:
                    return {"text": "", "error": None}
                i = 0
                while i < n:
                    chunk = wav[i : i + stride]
                    i += stride
                    is_final = i >= n
                    res = self.model.generate(
                        input=chunk,
                        cache=cache,
                        is_final=is_final,
                        chunk_size=self.CHUNK_SIZE,
                        encoder_chunk_look_back=self.ENCODER_CHUNK_LOOK_BACK,
                        decoder_chunk_look_back=self.DECODER_CHUNK_LOOK_BACK,
                    )
                    for item in res or []:
                        t = item.get("text", "") if isinstance(item, dict) else str(item or "")
                        t = _RICH_TAG_RE.sub("", t).strip()
                        if t:
                            texts.append(t)
            return {"text": "".join(texts), "error": None}
        except Exception as e:
            import traceback

            traceback.print_exc(file=sys.stderr)
            return {"text": "", "error": str(e)}

    def feed_audio(self, audio_f32, sample_rate, on_result):
        """处理一块音频(48k 或 16k f32), 流式识别并通过 on_result(text, is_final) 回调。

        on_result: callable(str, bool) -> None, 由调用方把文本推给 stdout (走真 stdout)。
        48k 块重采样到 16k 后累积进 StreamBuffer, 每满 chunk_stride(600ms)切一块
        喂模型 (is_final=False), 模型经 cache 增量输出新增文本。

        协议防御: 畸形 audio_data (0-d/2D/含 None) 或 sample_rate<=0 不杀进程
        (异常被吞, 仅打 stderr)——Python 进程是长寿命单例, bridge 跨会话复用,
        单块畸形若逃逸主循环会崩溃整个 ASR 进程且无自动恢复。
        """
        if not self.models_loaded:
            return

        try:
            # 维度归一: 防 0-d 标量 / 2D 嵌套 → librosa.resample 要求 1D
            audio_arr = np.asarray(audio_f32, dtype=np.float32)
            if audio_arr.ndim != 1:
                audio_arr = np.atleast_1d(audio_arr).reshape(-1)
            if sample_rate <= 0:
                print(f"[ASR] 非法 sample_rate={sample_rate}, 丢弃该块", file=sys.stderr)
                return
            # 重采样到 16k (ParaformerStreaming 要求 16k)。librosa 仅在此分支按需 import：
            # 16k 直通路径（ISSUE-6 Rust 预降采样后 / stub 测试）不触发重依赖加载。
            if sample_rate != 16000:
                import librosa

                audio_16k = librosa.resample(audio_arr, orig_sr=sample_rate, target_sr=16000)
            else:
                audio_16k = audio_arr
            if audio_16k.size == 0:
                return
        except Exception as e:
            print(f"[ASR] feed_audio 预处理失败: {e}", file=sys.stderr)
            return

        # 累积到 chunk_stride(600ms)切完整块, 逐块流式识别 (is_final=False)
        for chunk in self._buffer.push(audio_16k):
            self._recognize(chunk, is_final=False, on_result=on_result)

    def flush_stream(self, on_result):
        """收尾: 把缓冲内剩余尾巴作为末块喂模型 (is_final=True), 强制刷出末段。

        stop_capture 时 Rust 发 flush 触发。is_final=True 让模型把 cache 内剩余
        CIF 积分对应的尾文本输出 (连续长语音末段不丢)。flush 后 reset_stream 重置
        cache + buffer, 下次会话从干净状态开始 (顺修跨会话 cache 残留污染)。
        与 exit 区别: flush 不退 Python 进程, 保 bridge 复用。

        幻觉守卫: 从未喂过有效 chunk (self._cache 空) 且尾巴极短 (<300ms=4800 样本)
        时跳过 is_final=True——ParaformerStreaming 对「无上下文 + 静音/极短尾巴 +
        is_final=True」会产生单字幻觉 (如实测 '啊', 约 25%)。有 cache (喂过有效音频)
        时即使尾巴短也必须刷尾 (保连续长语音末段不丢, 此时模型有上下文不会幻觉)。

        ISSUE-3 完成信号: 所有出口 (模型未就绪 / 幻觉守卫跳过 / 正常刷尾) 在 finally
        统一推一条 on_result("", True)——Rust flush 收尾循环以此为 break 标志, 空文本
        不污染转录 (emit_result 跳过空 text)。否则守卫跳过路径静默无输出, Rust 空转
        到 25s deadline ("启动后立即停止"卡顿)。
        """
        try:
            if not self.models_loaded:
                return
            remainder = self._buffer.take_remainder()
            if not self._cache and len(remainder) < 4800:
                # 无上下文 + 极短尾巴: 跳过识别直接重置 (避免幻觉污染转录)
                self.reset_stream()
                return
            # 即使尾巴为空也发一次 is_final=True: 模型内部 cache 可能有 prev_samples
            # 残留, 需要触发刷尾。喂一小段静音避免空数组入模型。
            chunk = remainder if len(remainder) > 0 else np.zeros(960, dtype=np.float32)
            self._recognize(chunk, is_final=True, on_result=on_result)
            # 重置: 跨会话干净开始 (cache 残留会导致下次会话开头识别错乱)
            self.reset_stream()
        finally:
            # 完成信号: 任何出口都推, 保证 Rust flush 循环秒级 break。
            on_result("", True)

    def reset_stream(self):
        """开始新一轮流式: 清空 cache 与缓冲。"""
        self._cache = {}
        self._buffer.reset()


def main():
    global REAL_STDOUT_FD
    REAL_STDOUT_FD = os.dup(1)  # 保存真实 stdout fd（在任何 dup2 前），供 emit_json 绕开 suppress_stdout
    # 行缓冲 stdout, 保证 Rust 端按行读取 JSON 响应。
    try:
        sys.stdout.reconfigure(line_buffering=True)
    except Exception:
        pass

    svc = FunAsrServer()
    try:
        svc.load_models()
        svc.reset_stream()
        emit_json({"status": "ready", "models_loaded": True, "punc_loaded": svc.punc_loaded})
    except Exception as e:
        emit_json({"status": "error", "models_loaded": False, "error": str(e)})
        sys.exit(1)

    # ct-punc 后台懒加载（daemon）：不阻塞主循环，完成后 emit_json 推 punc_ready 信号。
    # daemon=True 保证进程退出时不挂死。ready 已发（paraformer 就绪），用户可即刻录音。
    threading.Thread(target=svc._load_punc_worker, daemon=True).start()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as e:
            emit_json({"text": "", "error": f"JSON 错误: {e}"})
            continue
        action = req.get("action", "")
        if action == "ping":
            emit_json({"status": "ready" if svc.models_loaded else "loading",
                       "models_loaded": svc.models_loaded, "punc_loaded": svc.punc_loaded})
        elif action == "transcribe":
            r = svc.transcribe_file(req.get("wav_path", ""), req.get("language", "auto"))
            emit_json(r)
        elif action == "punc":
            # ISSUE-8：ct-punc 标点恢复（后处理，独立于流式 generate）。Rust punc 线程
            # 把累积原文整体送来，返回带标点全文。type=punc 让 Rust stdout 读取线程
            # 分流到 punc_rx（不与流式 ASR result 争抢 result_rx）。
            r = svc.punctuate(req.get("text", ""))
            emit_json({"type": "punc", "text": r["text"], "error": r["error"]})
        elif action == "feed_audio":
            if not svc.models_loaded:
                # 模型未就绪, 丢弃 (避免阻塞主循环)
                continue
            audio = req.get("audio_data", [])
            sr = req.get("sample_rate", 48000)
            svc.feed_audio(
                audio, sr,
                on_result=lambda t, final: emit_json(
                    {"text": t, "is_final": final, "error": None}),
            )
        elif action == "flush":
            # 收尾触发末段识别（不退出进程）：stop_capture 时 Rust 发 flush，
            # is_final=True 强制刷出 cache 内剩余末段，避免连续长语音末段丢失。
            if svc.models_loaded:
                svc.flush_stream(
                    on_result=lambda t, final: emit_json(
                        {"text": t, "is_final": final, "error": None}),
                )
        elif action == "exit":
            # 收尾: 退出进程
            break
        else:
            emit_json({"text": "", "error": f"未知 action: {action}"})
    sys.exit(0)


if __name__ == "__main__":
    main()
