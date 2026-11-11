#!/usr/bin/env bash
# 从 HF 权重复现泰语 ASR 模型产物：下载 → 转 GGML → 量化 → 校验指纹。
#
# 为什么要有这个脚本：ADR-0004 §2 记的来源链是**事后补记**的，
# 「HF revision → 转换 → 量化」这一段靠人工重跑验证过一次（2026-08-28）。
# 把它固化成脚本，来源链才算真正可复现——否则下次换模型又是一遍口口相传。
#
# 用法：scripts/build-thai-model.sh [输出目录]
#   WHISPER_CPP  whisper.cpp 仓库路径（默认与本仓库同级）
#
# 产物：$OUT/ggml-distill-whisper-th-large-v3-q5_0.bin（约 574 MB）

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WCPP="${WHISPER_CPP:-$ROOT/../whisper.cpp}"
OUT="${1:-${TMPDIR:-/tmp}/agentear-thai-model}"
WORK="$OUT/work"

# ADR-0004 §4 选定的模型。换模型只改这三行。
HF_REPO="biodatlab/distill-whisper-th-large-v3"
# T3.5.5：钉完整 40 位 commit（2026-11-10 取自 HF API，前 12 位即 ADR-0004 §2
# 入库记录的 62df42cecab9；当天 HF HEAD 与之一致）。下载走 resolve/<commit>/，
# 不再取 HEAD 再比前缀。
HF_REV="62df42cecab9f484226ad5f9afdb557552021bbb"
HF_REV_PREFIX="${HF_REV:0:12}"
# 量化产物的 sha256 前 12 位（ADR-0004 §4 表）。48 bit，排碰撞够用；
# 它证明的是「与入库记录一致」，不是「逐字节一致」——转换是有损的。
EXPECT_SHA12="5bfc04f1931a"

# T3.5.5：mel_filters.npz 来自 openai/whisper 的这个 commit，文件 sha256 如下
# （2026-11-10 实测，4271 字节；该文件在 main 上与此 commit 逐字节相同）。
MEL_COMMIT="86098128c0b4f24f0e2aa2994de830614b474227"
MEL_SHA256="7450ae70723a5ef9d341e3cee628c7cb0177f36ce42c44b7ed2bf3325f0f6d4c"

QUANT="$WCPP/build/bin/quantize"
CONVERT="$WCPP/models/convert-h5-to-ggml.py"

die() { echo "!! $*" >&2; exit 1; }

[ -x "$QUANT" ] || die "找不到 quantize：${QUANT}（设 WHISPER_CPP 指向 whisper.cpp 仓库）"
[ -f "$CONVERT" ] || die "找不到转换脚本：$CONVERT"
command -v uv >/dev/null || die "缺少 uv"

mkdir -p "$WORK"
HF_DIR="$WORK/hf"
ASSETS="$WORK/whisper-assets"

echo "==> 准备 Python 环境（一次性，装在 $WORK/venv）"
export UV_PROJECT_ENVIRONMENT="$WORK/venv"
VENV="$WORK/venv"
[ -d "$VENV" ] || uv venv "$VENV" --python 3.11 >/dev/null
# convert-h5-to-ggml.py 要 torch + transformers 读 safetensors 和 config
uv pip install --python "$VENV/bin/python" -q \
  torch transformers huggingface_hub numpy

echo "==> 下载权重（$HF_REPO @ ${HF_REV_PREFIX}…）"
# ⚠️ 全程用 curl，不用 `hf` CLI 也不用 huggingface_hub 的下载函数。
# ADR-0004 §2 坑 3：`biodatlab` 仓库里的 `.ipynb_checkpoints/` 会把
# snapshot_download 和 hf CLI 打断，症状是只下到 model.safetensors 而配置
# 文件全缺，而后面转换报的错跟真正的原因毫无关系。
# 2026-09-02 实测 `hf_hub_download` 在本机同样失败（LocalEntryNotFoundError），
# 而同一时刻 curl 拿同一个 URL 是 200——所以逐文件 curl 是唯一验证过能用的路子。
#
# 两个端点轮着试：环境里的 HF_ENDPOINT（镜像）优先，官方站兜底。
ENDPOINTS=("${HF_ENDPOINT:-https://huggingface.co}" "https://huggingface.co")

hf_get() {  # hf_get <repo相对路径> <目标文件> <是否必需>
  local rel="$1" dest="$2" required="$3" ep code
  for ep in "${ENDPOINTS[@]}"; do
    # -f 让 4xx/5xx 返回非零；-L 跟重定向（HF 的 resolve 一律 307 到 CDN）
    code="$(curl -sfL --max-time 900 -w '%{http_code}' \
      -o "$dest.part" "$ep/$HF_REPO/resolve/$HF_REV/$rel" 2>/dev/null)" || code=""
    if [ "$code" = "200" ] && [ -s "$dest.part" ]; then
      mv "$dest.part" "$dest"
      echo "    $rel ✅"
      return 0
    fi
    rm -f "$dest.part"
  done
  if [ "$required" = "required" ]; then
    die "必需文件 $rel 下载失败（试过 ${ENDPOINTS[*]}）"
  fi
  # 可选文件缺失是正常的——不是每个仓库都有 normalizer.json
  echo "    $rel —— 跳过（仓库里没有）"
}

mkdir -p "$HF_DIR"

# revision 已在上面钉死，下载直接走 resolve/<commit>/，上游再动也影响不到这里。
[ "${#HF_REV}" -eq 40 ] || die "HF_REV 必须是完整 40 位 commit"
echo "    revision ${HF_REV}（钉死）"

# 转换脚本只读这几个文件。整仓 clone 会顺带拉走一份 3 GB 的 pytorch_model.bin。
hf_get config.json              "$HF_DIR/config.json"              required
hf_get model.safetensors        "$HF_DIR/model.safetensors"        required
hf_get tokenizer.json           "$HF_DIR/tokenizer.json"           optional
hf_get preprocessor_config.json "$HF_DIR/preprocessor_config.json" optional
hf_get added_tokens.json        "$HF_DIR/added_tokens.json"        optional
hf_get special_tokens_map.json  "$HF_DIR/special_tokens_map.json"  optional
hf_get vocab.json               "$HF_DIR/vocab.json"               optional
hf_get merges.txt               "$HF_DIR/merges.txt"               optional
hf_get tokenizer_config.json    "$HF_DIR/tokenizer_config.json"    optional
hf_get normalizer.json          "$HF_DIR/normalizer.json"          optional
hf_get generation_config.json   "$HF_DIR/generation_config.json"   optional

echo "==> 取 mel_filters.npz"
# ADR-0004 §2 坑 4：convert-h5-to-ggml.py 要 <dir_whisper>/whisper/assets/mel_filters.npz，
# 而 whisper.cpp 的 checkout 里没有——它是 OpenAI whisper **Python 包**的资产。
# 不必为这 4 KB 装整个 openai-whisper，也不要往共享的 whisper.cpp checkout 里写东西。
MEL="$ASSETS/whisper/assets/mel_filters.npz"
if [ ! -f "$MEL" ]; then
  mkdir -p "$(dirname "$MEL")"
  # T3.5.5：钉 openai/whisper 的 commit + 文件 sha256，不再取 main。
  curl -sfL -o "$MEL" \
    "https://raw.githubusercontent.com/openai/whisper/${MEL_COMMIT}/whisper/assets/mel_filters.npz" \
    || die "mel_filters.npz 下载失败"
fi
[ "$(shasum -a 256 "$MEL" | awk '{print $1}')" = "$MEL_SHA256" ] \
  || { rm -f "$MEL"; die "mel_filters.npz 的 sha256 与钉死的值不符（已删除，重跑会重新下载）"; }
# large-v3 系走 128 维 mel，要和 config.json 的 num_mel_bins 对上
"$VENV/bin/python" -c "
import json, numpy, sys
n = json.load(open('$HF_DIR/config.json'))['num_mel_bins']
keys = list(numpy.load('$MEL').keys())
assert f'mel_{n}' in keys, f'mel_filters.npz 里没有 mel_{n}（有 {keys}）'
print(f'    num_mel_bins={n} ✅')
"

echo "==> 转 GGML（f16）"
F16="$WORK/ggml-model.bin"
if [ ! -f "$F16" ]; then
  # 不传第 4 个参数 = f16。ADR-0004 §3 的 f16 档因峰值 RSS 1.84 GB 出局，
  # 但它是量化的**输入**，这一步必须过。
  (cd "$WORK" && "$VENV/bin/python" "$CONVERT" "$HF_DIR" "$ASSETS" "$WORK")
fi
# ⚠️ ADR-0004 §2 坑 2：quantize **不校验输入**。转换失败留下的残缺文件
# 照样能量化成功，产出同样残缺的「模型」。所以在这里自己把关。
F16_SIZE="$(stat -f%z "$F16")"
[ "$F16_SIZE" -gt 1000000000 ] || die "f16 中间产物只有 $F16_SIZE 字节，转换没成功"
echo "    $F16_SIZE 字节  sha256 $(shasum -a 256 "$F16" | cut -c1-12)"

echo "==> 量化 q5_0"
Q5="$OUT/ggml-distill-whisper-th-large-v3-q5_0.bin"
"$QUANT" "$F16" "$Q5" q5_0 | tail -3

echo "==> 校验指纹"
GOT="$(shasum -a 256 "$Q5" | cut -c1-12)"
SIZE="$(stat -f%z "$Q5")"
echo "    sha256 前 12 位  $GOT"
echo "    体积             $SIZE 字节"
if [ "$GOT" = "$EXPECT_SHA12" ]; then
  echo "    ✅ 与 ADR-0004 §4 入库记录一致"
else
  # 对不上不一定是坏事（可能上游或工具链变了），但**必须显式失败**——
  # 悄悄发一个和评测数据对不上的模型，等于让 §4 那张 CER 表失效。
  die "指纹不符：期望 ${EXPECT_SHA12}，实得 ${GOT}。评测数据与这个产物对不上，不要分发。"
fi

echo
echo "产物：$Q5"
echo "完整 sha256：$(shasum -a 256 "$Q5" | cut -d' ' -f1)"
