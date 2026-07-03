# VoiceDown — v0.4

Windows 窗口音频转录工具。选择目标窗口 → 开始 WASAPI **进程级** Loopback 录制 → SenseVoiceSmall 语音转文字 → 保存 WAV + TXT。

仅捕获目标进程及其子进程树的音频，不影响其他系统音频。

## 启动

```powershell
# 0. 安装 Python 依赖（首次）
pip install -r voicedown-app/src-tauri/python_asr/requirements.txt

# 1. 启动应用（带 ASR）
cd voicedown-app
npx tauri dev --features asr

# 2. 或仅音频捕获（不带 ASR）
npx tauri dev
```

## 已实现

| 功能 | 状态 | 说明 |
|------|------|------|
| F-01 窗口选择 | ✅ | EnumWindows 枚举可见窗口，下拉选择 |
| F-02 进程级音频捕获 | ✅ | WASAPI Process Loopback，仅捕获目标进程音频 |
| F-03 语音转文字 (ASR) | ✅ | SenseVoice + Fsmn_vad (funasr_onnx)，流式逐句，**启动预加载**，Python 子进程桥接 |
| F-14 文本智能优化 | ✅ | LLM 二阶段优化（Ollama/云端可选），增量分批，原始+优化双 txt |
| GUI 重设计 | ✅ | Apple 设计语言深色主题，双栏对比布局（原始转录 \| 优化文本）+ 设置模态（启用开关 / 后端分段 / 深度分段 / endpoint·model·api_key 输入） |
| 音频回放 | ✅ | 停止后自动显示 `<audio>` 播放器 |
| 转录文本保存 | ✅ | 停止后自动保存 `.txt` 文件 |

### ASR 引擎：SenseVoice + Fsmn_vad（流式逐句）

- **技术**: Alibaba FunASR SenseVoiceSmall + Fsmn_vad，通过 `funasr_onnx` Python 子进程调用
- **流式协议**: Rust 喂 48k 原始音频 → Python 端 `librosa` 重采样到 16k → **Fsmn_vad 逐句切分** → SenseVoice 逐句识别 → 结果回传 Rust
- **启动预加载**: 应用启动即后台加载模型（~8.6s，不阻塞 UI），`start_capture` 复用已就绪的 bridge —— 消除点击「开始」后才加载模型导致的**前期音频丢失**。前端双保险：监听 `asr-ready`/`asr-error` 事件 + 轮询 `get_asr_state`（每 1s）兜底（commit c49958b，因 Tauri 事件触发即忘曾导致永久卡在加载中），「开始」按钮在加载完成前禁用
- **中文准确率**: ~95%+ (test.wav 实测：「甚至出现交易几乎停滞的情况」零字错，对标 SenseVoice CER 基准 8.01%)
- **特点**: 自带标点恢复、逆文本正则化 (ITN)、简繁自动转换；VAD 逐句输出，实时性好
- **依赖**: Python 3.10+ + funasr_onnx + onnxruntime；首次运行若 SenseVoice ONNX 缺失，会调用 funasr + torch 现场导出（**非纯轻量**，需 funasr 与 torch）
- **关键版本锁**: torch 必须为 `2.3.1`（torch≥2.4 的 ONNX 导出图在 onnxruntime<1.21 下无法加载），Fsmn_vad 必须用 `quantize=True`（仓库仅发布量化版）

### 文本优化 (F-14) — 第二阶段

- **作用**：对 ASR 原始文本做错别字纠错、标点修正、语气词过滤、（深度模式）分段润色
- **后端**：抽象 trait，运行时按配置切换 —— Ollama 本地（`/api/generate`）或 OpenAI 兼容云端（`/chat/completions`，默认 DeepSeek）
- **触发**：增量分批（攒满 8 句 / 400 字 / 30s 任一阈值），独立线程不阻塞实时字幕
- **模式**：轻度（纠错+标点+语气词）/ 深度（+分段+润色），temperature 0.3
- **配置**：`%USERPROFILE%\Documents\VoiceDown\llm_config.json`，前端「优化设置」面板编辑
- **设置面板**：测试连接按钮右侧就地显示结果（测试中…/✓成功/✗失败，任意配置改动自动失效上次结果），API Key 填写后 debounce 自动保存到 `llm_config.json`
- **降级**：LLM 失败时该批用原始文本，永不丢原文、永不阻塞停止
- **产物**：`capture_xxx.txt`（原始）+ `capture_xxx_optimized.txt`（优化）
- **依赖**：随 `asr` feature 引入 `reqwest`（blocking + rustls-tls）

### 界面设计

- **设计语言**：Apple 风格深色主题，系统配色 token（`systemBackground #1c1c1e` 等），磨砂玻璃 toolbar（`backdrop-filter: blur + saturate`）
- **双栏对比布局**：开启优化时左栏「原始转录 (ASR)」| 右栏「优化文本 (LLM)」并排对照；未开启时退化为单栏
- **设置模态**：齿轮图标按钮触发，含启用开关（toggle switch）、后端分段控件（Ollama 本地 / 云端 API）、深度分段控件（轻度 / 深度）、endpoint·model·api_key 输入
- **状态指示**：状态点（idle 绿 / capturing 红脉动 / stopping 橙）+ ASR 徽章

### 音频捕获详情

- **技术**: Wasapi crate v0.23 (`AudioClient::new_application_loopback_client`)
- **格式**: 32-bit float, 48000Hz, 立体声 → WAV (16-bit PCM, mono, 48000Hz)
- **已验证**: Chrome, Firefox, 夸克网盘 (Electron) 均正常捕获有声内容

## 构建依赖

| 依赖 | 说明 |
|------|------|
| Rust 1.77+ | 编译后端 |
| Node.js 18+ | 前端构建 |
| Python 3.12 | ASR 引擎 (仅 `asr` feature) |
| funasr_onnx | SenseVoice + Fsmn_vad ONNX 推理 (pip install) |
| torch 2.3.1 | 首次现场导出 SenseVoice ONNX (全局安装) |
| reqwest 0.12 | F-14 LLM HTTP 调用 (仅 `asr` feature) |

**不再需要**: MSVC Build Tools / CMake / LLVM

## 已知问题

| 问题 | 状态 | 说明 |
|------|------|------|
| UWP 应用 (Media Player 等) | ⚠️ 待修复 | ApplicationFrameHost 托管窗口，需回退到顶层 PID 策略 |

## 项目结构

```
voicedown-app/
├── src/                          # 前端 (React + TypeScript)
│   ├── App.tsx                   # 主界面
│   └── App.css                   # 深色主题样式
├── src-tauri/
│   ├── python_asr/               # Python ASR 服务（流式 VAD）
│   │   ├── asr_server.py         # SenseVoice + Fsmn_vad 流式 stdio 服务
│   │   ├── verify_api.py         # API 冒烟测试
│   │   ├── requirements.txt      # Python 依赖（torch 2.3.1 锁版本）
│   │   └── tests/                # VAD endpoint 单元测试
│   └── src/
│       ├── main.rs               # 入口
│       ├── lib.rs                # IPC 命令 + ASR 流式桥接 (feed_audio/result)
│       ├── window_selector.rs    # F-01: EnumWindows + PID 选择
│       ├── audio_capture.rs      # F-02: WASAPI Process Loopback + 300ms 分块
│       ├── python_bridge.rs      # Python 子进程管理 (feed_audio/result_rx)
│       ├── text_optimizer.rs     # F-14: 文本优化 LLM 后端(Ollama/OpenAI) + 增量分批
│       └── asr_engine.rs         # 简繁转换 + 文本工具
├── package.json
├── vite.config.ts
└── src-tauri/tauri.conf.json     # 端口 5176
```

## 技术栈

| 层 | 技术 |
|----|------|
| Frontend | React 18 + TypeScript 5 + CSS |
| Desktop | Tauri 2.x |
| Audio | Rust + wasapi crate v0.23 (WASAPI Process Loopback) |
| ASR | Python + funasr_onnx (SenseVoiceSmall ONNX) |
| Window Enum | Rust + windows crate v0.58 (EnumWindows + Kernel32) |
