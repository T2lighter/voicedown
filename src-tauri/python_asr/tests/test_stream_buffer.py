import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import numpy as np
from asr_server import StreamBuffer

# StreamBuffer: 把不规则小块累积, 每满 chunk_stride(9600 样本=600ms@16k)切一块。
# feed_audio 每 300ms 收到 4800 样本(48k→16k 重采样后), 故通常每 2 次 push 切 1 块。

STRIDE = 9600  # 与 FunAsrServer.CHUNK_STRIDE 一致


def test_no_chunk_before_stride():
    """累积不足 stride 不切块。"""
    b = StreamBuffer(STRIDE)
    chunks = b.push(np.zeros(4800, dtype=np.float32))  # 300ms
    assert chunks == []
    assert len(b._buf) == 4800


def test_exact_stride_one_chunk():
    """恰好满 stride 切 1 块, 缓冲清空。"""
    b = StreamBuffer(STRIDE)
    chunks = b.push(np.zeros(STRIDE, dtype=np.float32))
    assert len(chunks) == 1
    assert len(chunks[0]) == STRIDE
    assert len(b._buf) == 0


def test_two_pushes_yield_one_chunk():
    """两次 4800(300ms) push: 4800+4800=9600=stride, 切 1 块, 缓冲清空。"""
    b = StreamBuffer(STRIDE)
    assert b.push(np.zeros(4800, dtype=np.float32)) == []
    chunks = b.push(np.zeros(4800, dtype=np.float32))
    assert len(chunks) == 1
    assert len(chunks[0]) == STRIDE
    assert len(b._buf) == 0  # 9600 恰好等于 stride, 无余


def test_three_pushes_leave_remainder():
    """三次 4800(300ms) push:
    push1: 4800<stride 无块; push2: 9600=stride 切 1 块; push3: 4800<stride 无块。共 1 块, 余 4800。"""
    b = StreamBuffer(STRIDE)
    assert b.push(np.zeros(4800, dtype=np.float32)) == []
    assert len(b.push(np.zeros(4800, dtype=np.float32))) == 1  # push2 切块
    assert b.push(np.zeros(4800, dtype=np.float32)) == []      # push3 不足
    assert len(b._buf) == 4800


def test_overflow_splits_multiple_chunks():
    """一次 push 超过 2*stride 切 2 块, 余数留缓冲。"""
    b = StreamBuffer(STRIDE)
    n = STRIDE * 2 + 1000
    chunks = b.push(np.zeros(n, dtype=np.float32))
    assert len(chunks) == 2
    assert all(len(c) == STRIDE for c in chunks)
    assert len(b._buf) == 1000


def test_chunks_are_contiguous_slices():
    """切出的块是原音频的连续切片, 无重叠无遗漏 (流式不丢不重)。"""
    b = StreamBuffer(STRIDE)
    audio = np.arange(STRIDE * 2, dtype=np.float32)
    chunks = b.push(audio)
    assert len(chunks) == 2
    np.testing.assert_array_equal(chunks[0], audio[:STRIDE])
    np.testing.assert_array_equal(chunks[1], audio[STRIDE : STRIDE * 2])


def test_take_remainder_flushes_and_clears():
    """take_remainder 取出不足 stride 的尾巴并清空 (flush 末段用)。"""
    b = StreamBuffer(STRIDE)
    b.push(np.zeros(4800, dtype=np.float32))
    rem = b.take_remainder()
    assert len(rem) == 4800
    assert len(b._buf) == 0
    # 再次取为空
    assert len(b.take_remainder()) == 0


def test_reset_clears_buffer():
    """reset 清空缓冲 (跨会话干净开始)。"""
    b = StreamBuffer(STRIDE)
    b.push(np.zeros(4800, dtype=np.float32))
    b.reset()
    assert len(b._buf) == 0


def test_reset_then_clean_start():
    """reset 后新音频从空缓冲开始, 不混入上一会话残留。"""
    b = StreamBuffer(STRIDE)
    b.push(np.ones(5000, dtype=np.float32))
    b.reset()
    chunks = b.push(np.zeros(STRIDE, dtype=np.float32))
    assert len(chunks) == 1
    # 切出的块应全 0 (新会话), 不含上一会话的 1.0 残留
    assert np.all(chunks[0] == 0.0)
