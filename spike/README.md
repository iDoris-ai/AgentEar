# M0 基准测试 spike（丢弃式）

**这是一次性测量脚本，验证完就删。不要在这里写产品代码。**
结论产出在 `docs/benchmarks.md`。

## 复现

```bash
# 1. 运行时二进制（macOS arm64，零 Python）
curl -sL -o r.tar.gz https://github.com/modelscope/FunASR/releases/download/runtime-llamacpp-v0.1.9/funasr-llamacpp-macos-arm64.tar.gz
mkdir -p bin && tar xzf r.tar.gz -C bin && rm r.tar.gz && chmod +x bin/llama-funasr-*

# 2. 模型（注意 VAD 在另一个仓库）
B=https://huggingface.co/FunAudioLLM
mkdir -p models
curl -sL -o models/funasr-encoder-f16.gguf "$B/Fun-ASR-Nano-GGUF/resolve/main/funasr-encoder-f16.gguf"  # 447.6 MiB
curl -sL -o models/qwen3-0.6b-q4km.gguf   "$B/Fun-ASR-Nano-GGUF/resolve/main/qwen3-0.6b-q4km.gguf"      # 461.8 MiB
curl -sL -o models/fsmn-vad.gguf          "$B/fsmn-vad-GGUF/resolve/main/fsmn-vad.gguf"                 # 1.6 MiB

# 3. 语料转 16kHz 单声道
ffmpeg -i <input>.m4a -ar 16000 -ac 1 -c:a pcm_s16le audio/sample01.wav

# 4. 跑（-l 测峰值 RSS）
/usr/bin/time -l ./bin/llama-funasr-cli \
  --enc models/funasr-encoder-f16.gguf \
  -m models/qwen3-0.6b-q4km.gguf \
  --vad models/fsmn-vad.gguf \
  -a audio/sample01.wav
```

模型和音频不入库（见 `.gitignore`）。
