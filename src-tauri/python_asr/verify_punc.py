"""一次性验证 funasr ct-punc 标点模型加载与调用（ISSUE-8）。
运行: python verify_punc.py
成功则加载 ct-punc、对样例文本标点、打印结果。

验证点（与 asr_server.py::FunAsrServer.punctuate 同路径）：
  - model id = "ct-punc"（注册别名）。⚠️ 仓库名 "punc_ct-transformer_zh-cn-common-vad_realtime-vocab272727"
    funasr 报 "is not registered" → punc_loaded=False 静默降级 → 主字幕无标点（曾踩坑）。
  - generate(input=text) -> list[dict] with 'text'（带标点全文；另有 punc_array tensor，忽略）
"""
import time


def main():
    from funasr import AutoModel

    t = time.time()
    model = AutoModel(model="ct-punc", disable_update=True)
    print(f"[PUNC] 加载耗时 {time.time()-t:.1f}s")

    sample = "那今天的会就开到这吧 happy new year 明年见"
    res = model.generate(input=sample)
    out = "".join(
        (item.get("text", "") if isinstance(item, dict) else str(item or ""))
        for item in (res or [])
    )
    print(f"[PUNC] 输入: {sample}")
    print(f"[PUNC] 输出: {out}")

    has_punct = any(c in out for c in "，。！？,!?")
    tag = "OK" if has_punct else "FAIL"
    print(f"[{tag}] 标点恢复{'成功' if has_punct else '失败'}")
    assert has_punct, "ct-punc 未产出标点，检查 model id / 网络"


if __name__ == "__main__":
    main()
