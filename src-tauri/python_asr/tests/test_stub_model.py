"""ISSUE-1 测试接缝：FunAsrServer 可注入 stub model，脱离 funasr/模型下载被测。

镜像 test_stream_buffer.py 避开真模型的做法：构造一个固定输出的 stub model，
使 feed_audio / flush_stream 的纯逻辑路径（攒块、回调、is_final）可被测试，
不 import funasr、不联网下载模型。后续 ISSUE-3/8/9（flush 信号/标点/时间戳）
都复用此接缝。
"""
import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import numpy as np
from asr_server import FunAsrServer


class _StubModel:
    """模拟 funasr AutoModel.generate 的返回结构（list[dict] with key/text）。

    generate 调用参数（cache/is_final/chunk_size/...）一律 **kwargs 忽略，
    只固定返回预设文本——这是后续切片验证触发逻辑（而非模型本身）所需的全部。
    """

    def __init__(self, text):
        self.text = text

    def generate(self, cache=None, **kwargs):
        # 忠实模拟真 ParaformerStreaming 的契约：cache 由模型原地更新（CLAUDE.md：
        # cache 跨 chunk 复用同一 dict，模型原地写入）。flush_stream 的幻觉守卫依赖
        # 「是否喂过有效音频」= cache 非空，故 stub 必须标记 cache，否则守卫误判跳过。
        if cache is not None:
            cache["stub_marker"] = True
        return [{"key": 0, "text": self.text}]


class _StubPuncModel:
    """模拟 ct-punc AutoModel.generate(input=text) -> list[dict] with key/text。"""

    def __init__(self, text):
        self.text = text
        self.last_input = None

    def generate(self, input=None, **kwargs):
        self.last_input = input
        return [{"key": 0, "text": self.text}]


def test_stub_model_feed_emits_text():
    """注入 stub model：喂满一块（9600 样本@16k）→ on_result 收到 stub 文本。

    证明 FunAsrServer 能在无 funasr 环境下实例化并跑通 feed 路径。
    """
    svc = FunAsrServer(model=_StubModel("你好世界"))
    assert svc.models_loaded  # 注入 stub 即视为就绪，不走真模型加载

    got = []
    svc.feed_audio(
        np.zeros(FunAsrServer.CHUNK_STRIDE, dtype=np.float32),
        16000,
        on_result=lambda t, final: got.append((t, final)),
    )
    assert got == [("你好世界", False)]


def test_stub_model_flush_emits_final():
    """注入 stub model：先喂一块建立 cache，flush 尾段 → on_result 带 is_final=True。

    走 flush_stream 真实路径（非幻觉守卫跳过分支：已有 cache 不跳过），
    验证 is_final 经 _recognize → on_result 正确透传。
    """
    svc = FunAsrServer(model=_StubModel("末段"))
    # 先喂一块满 stride 建立 cache（避免触发「无 cache + 极短尾巴」幻觉守卫跳过）
    svc.feed_audio(
        np.zeros(FunAsrServer.CHUNK_STRIDE, dtype=np.float32),
        16000,
        on_result=lambda t, final: None,
    )
    got = []
    svc.flush_stream(on_result=lambda t, final: got.append((t, final)))
    assert any(final is True for _, final in got)
    assert any(t == "末段" for t, _ in got)


# ── ISSUE-3：flush 完成信号（所有出口统一推 is_final=True + 空 text）─────────

def test_flush_completion_signal_guard_skip():
    """幻觉守卫跳过路径（无 cache + 尾巴极短）也推完成信号，不再静默无输出。

    否则 Rust flush 收尾循环收不到 is_final，会空转到 25s deadline（"启动后立即停止"卡顿）。
    """
    svc = FunAsrServer(model=_StubModel("x"))
    # 不喂任何音频 → cache 空、buffer 空 → 守卫跳过（不发识别结果）
    got = []
    svc.flush_stream(on_result=lambda t, final: got.append((t, final)))
    assert ("", True) in got, "守卫跳过路径必须推完成信号"


def test_flush_completion_signal_models_not_ready():
    """模型未就绪路径推完成信号。"""
    svc = FunAsrServer()  # 无 stub → models_loaded=False
    got = []
    svc.flush_stream(on_result=lambda t, final: got.append((t, final)))
    assert ("", True) in got, "模型未就绪路径必须推完成信号"


# ── ISSUE-8：ct-punc 标点恢复（punctuate 方法 + 降级）──────────────────────

def test_punctuate_with_stub():
    """注入 stub punc model：punctuate 调 ct-punc 返回带标点文本。"""
    svc = FunAsrServer(punc_model=_StubPuncModel("你好，世界。"))
    assert svc.punc_loaded
    r = svc.punctuate("你好世界")
    assert r["error"] is None
    assert r["text"] == "你好，世界。"
    assert svc.punc_model.last_input == "你好世界"


def test_punctuate_degrades_without_model():
    """无 punc model（加载失败/未注入）：原文直通，不丢字。"""
    svc = FunAsrServer()  # 无 punc_model
    assert not svc.punc_loaded
    r = svc.punctuate("你好世界")
    assert r["text"] == "你好世界"
    assert r["error"] is None


def test_punctuate_empty_passthrough():
    """空/空白文本直通，不调模型。"""
    svc = FunAsrServer(punc_model=_StubPuncModel("不应被调用"))
    assert svc.punctuate("")["text"] == ""
    assert svc.punctuate("   ")["text"] == "   "
    assert svc.punc_model.last_input is None


