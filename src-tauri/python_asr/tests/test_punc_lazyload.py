"""ct-punc 懒加载测试（ISSUE-lazyload）。

复刻 test_stub_model.py 的接缝：FunAsrServer 可注入 stub model / monkeypatch
_make_punc_model，脱离 funasr/模型下载被测。覆盖：
  - emit_json 绕开 suppress_stdout 的 dup2（blocker 修法）
  - _load_punc_worker 成功/失败/幂等 + 推 punc_ready 信号
  - punctuate 持锁读快照 + 未就绪降级 + 并发不崩
"""
import sys, os, threading
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import asr_server
from asr_server import FunAsrServer, emit_json, suppress_stdout


class _StubPuncModel:
    """模拟 ct-punc AutoModel.generate(input=text) -> list[dict] with key/text。"""

    def __init__(self, text):
        self.text = text
        self.last_input = None

    def generate(self, input=None, **kwargs):
        self.last_input = input
        return [{"key": 0, "text": self.text}]


# ── emit_json：绕开 suppress_stdout 的 dup2（blocker）──────────────────────


def test_emit_json_bypasses_dup2(monkeypatch):
    """emit_json 用保存的真实 fd，suppress_stdout 的 dup2(fd1→devnull) 吞不掉它。

    这是懒加载的生死线：worker 信号若走 fd1（print），会与 punctuate/_recognize
    的 suppress_stdout 块跨线程交错被吞 → Rust 永远收不到 → ct-punc 永久降级。
    """
    r, w = os.pipe()
    monkeypatch.setattr(asr_server, "REAL_STDOUT_FD", w)
    try:
        with suppress_stdout():  # fd1 已被 dup2 到 devnull
            emit_json({"type": "punc_ready", "ready": True})
        os.close(w)
        data = os.read(r, 4096).decode("utf-8")
    finally:
        os.close(r)
    assert '"punc_ready"' in data
    assert '"ready": true' in data


def test_emit_json_fallback_when_fd_none(monkeypatch):
    """REAL_STDOUT_FD 未初始化（None）时 fallback print，不崩。"""
    monkeypatch.setattr(asr_server, "REAL_STDOUT_FD", None)
    emit_json({"type": "punc_ready", "ready": True})  # 不抛异常即通过


# ── _load_punc_worker：成功/失败/幂等 + punc_ready 信号 ─────────────────────


def _pipe_punc_worker(monkeypatch, svc, make_model):
    """跑一次 worker，捕获其 emit_json 输出（pipe 模拟真 stdout）。返回 (svc, data)。"""
    r, w = os.pipe()
    monkeypatch.setattr(asr_server, "REAL_STDOUT_FD", w)
    monkeypatch.setattr(svc, "_make_punc_model", make_model)
    svc._load_punc_worker()
    os.close(w)
    data = os.read(r, 4096).decode("utf-8")
    os.close(r)
    return data


def test_worker_success_loads_and_signals(monkeypatch):
    svc = FunAsrServer()
    assert svc.punc_model is None
    data = _pipe_punc_worker(monkeypatch, svc, lambda: _StubPuncModel("你好，世界。"))
    assert svc.punc_loaded
    assert svc.punc_model is not None
    assert '"punc_ready"' in data and '"ready": true' in data


def test_worker_failure_signals_not_ready(monkeypatch):
    svc = FunAsrServer()

    def boom():
        raise RuntimeError("download failed")

    data = _pipe_punc_worker(monkeypatch, svc, boom)
    assert not svc.punc_loaded
    assert svc.punc_model is None  # 失败不置 model（单调不变量）
    assert '"ready": false' in data
    assert "download failed" in data  # error 落信号供 Rust 日志


def test_worker_idempotent_no_reload(monkeypatch):
    """已有 punc_model 时 worker 直接返回，不重复加载（幂等）。"""
    svc = FunAsrServer(punc_model=_StubPuncModel("x"))
    assert svc.punc_loaded
    calls = []

    def factory():
        calls.append(1)
        return _StubPuncModel("new")

    data = _pipe_punc_worker(monkeypatch, svc, factory)
    assert calls == [], "已有 model 不应重复加载"
    # 幂等也推 ready:true（告知 Rust 当前就绪态）
    assert '"ready": true' in data


# ── punctuate：持锁读快照 + 未就绪降级 + 并发不崩 ─────────────────────────


def test_punctuate_when_not_ready_passthrough(monkeypatch):
    """未就绪（model=None）：原文直通，不丢字、不报错。"""
    svc = FunAsrServer()
    assert svc.punc_model is None
    r = svc.punctuate("你好世界")
    assert r["text"] == "你好世界"
    assert r["error"] is None


def test_punctuate_with_ready_model(monkeypatch):
    """就绪：持锁读快照、锁外 generate，返回带标点文本。"""
    svc = FunAsrServer(punc_model=_StubPuncModel("你好，世界。"))
    r = svc.punctuate("你好世界")
    assert r["text"] == "你好，世界。"
    assert r["error"] is None


def test_punctuate_concurrent_with_load_no_crash(monkeypatch):
    """worker 加载中并发 punctuate 不崩（锁保护 + 未就绪降级兜底）。

    红线：punctuate 在主循环内同步调用，绝不能因 worker 持锁而长时间阻塞。
    worker 锁只覆盖赋值（微秒级），punctuate acquire 超时即降级。
    """
    svc = FunAsrServer()
    monkeypatch.setattr(svc, "_make_punc_model", lambda: _StubPuncModel("你好，世界。"))
    t = threading.Thread(target=svc._load_punc_worker, daemon=True)
    t.start()
    # 加载窗口内调 punctuate：无论 model 是否已就绪，都不崩、不丢字
    r = svc.punctuate("你好世界")
    assert r["error"] is None
    assert "你好世界" in r["text"]
    t.join(timeout=5)
    assert not t.is_alive(), "worker 应在 5s 内完成"
    assert svc.punc_loaded


def test_worker_top_level_exception_still_signals(monkeypatch):
    """worker 顶层任何逃逸异常仍推 ready:false 终态信号（防永久静默降级）。"""
    svc = FunAsrServer()

    class _NastyModel:
        def __init__(self):
            raise RuntimeError("construct boom")

    data = _pipe_punc_worker(monkeypatch, svc, _NastyModel)
    assert '"ready": false' in data
    assert not svc.punc_loaded
