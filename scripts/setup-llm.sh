#!/usr/bin/env bash
# 备好 M2 理解层的边车：mlx-dspark + Ornith-1.0-9B 6bit。
#
# 为什么这是脚本而不是几行 README 说明：M0 那次是在 `spike/` 里手搭的
# 一次性环境，用完即删——结果 M2 开工时环境和模型都没了，
# `docs/benchmarks-m2.md` §7 的「复现」小节指向一个已经不存在的 venv。
# 把它固化下来，下次换机器或清过盘之后一条命令能回到同一个状态。
#
# 这个边车**不是 Rust 守护进程的一部分**：它是独立进程、有明确的 HTTP
# 边界，可以单独重启、单独崩溃。ADR-0002 的措辞就是按这条划的线——
# 「Rust 守护进程自身不内嵌 Python 运行时；外部推理服务的实现语言不受限」。
#
# 用法：scripts/setup-llm.sh [环境目录]   默认 ~/.agentear/llm
#   跑完用 scripts/serve-llm.sh 启动服务

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_DIR="${1:-$HOME/.agentear/llm}"
VENV="$ENV_DIR/venv"
MODEL_DIR="$ENV_DIR/models/ornith"

# ADR-0002 定的模型。6bit 那一档是有原因的：GGUF 的 Q5_K_M 和 MLX 格式
# 不兼容，MLX 侧的对应档就是 6bit。
HF_REPO="mlx-community/Ornith-1.0-9B-6bit"

die() { echo "!! $*" >&2; exit 1; }

command -v uv >/dev/null || die "缺少 uv"

echo "==> Python 环境（${VENV}）"
mkdir -p "$ENV_DIR"
[ -d "$VENV" ] || uv venv "$VENV" --python 3.11 >/dev/null
# mlx-dspark 带 mlx / mlx-lm。装在自己的 venv 里，不碰系统 Python。
# T3.5.5：钉版本（2026-11-10 取自实测在用的 venv）。升级 = 改这里 + 重跑 benchmarks-m2 的复现。
PINS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/pins"
# FU-24：-c 约束文件锁住间接依赖（mlx-metal 等）；直接依赖仍在命令行 == 钉死。
uv pip install --python "$VENV/bin/python" -q -c "$PINS_DIR/venv-constraints.txt" mlx==0.32.2 mlx-dspark==0.18.0 huggingface_hub==1.29.0

echo "==> 模型 ${HF_REPO}（约 7.7 GB）"
# ⚠️ **用 curl 逐文件，不用 snapshot_download。**
#
# 上一版这里用的是 snapshot_download，注释里还写着「mlx-community 的仓库
# 没有 biodatlab 那个 .ipynb_checkpoints 问题」——那个推断是错的。
# 实测（2026-09-02）它照样报 LocalEntryNotFoundError，而同一时刻 curl
# 拿同一个 URL 是 200。根因不在仓库，在这台机器的 HF_ENDPOINT 镜像和
# huggingface_hub 的交互，和 scripts/build-thai-model.sh 记的是同一件事。
#
# 两个端点轮着试：环境里的 HF_ENDPOINT（镜像）优先，官方站兜底。
#
# 💡 **2026-09-03 发现了更省事的办法**：`env -u HF_ENDPOINT <命令>`。
# 本机 huggingface_hub 报 LocalEntryNotFoundError 的根因就是那个镜像变量，
# 去掉它之后官方的下载路径直接可用（T3.1.2 实测）。
# 这里保留 curl 逐文件是因为它已经跑通且能续传 8 GB 的分片；
# **新写的脚本直接用 `env -u HF_ENDPOINT` 就行，不必再抄这一套。**
ENDPOINTS=("${HF_ENDPOINT:-https://huggingface.co}" "https://huggingface.co")

mkdir -p "$MODEL_DIR"

# T3.5.5：revision 钉死成常量，不再问 API 要 HEAD。
# 旧写法下「同一条命令」在不同日子装到的是不同权重。这个 commit 是
# mlx-community/Ornith-1.0-9B-6bit 的 HEAD（上游 2026-06-28 之后没动过，
# 2026-11-10 复核）。两个 shard 必然来自同一次提交。
# 升级 = 改常量 + 重跑 benchmarks-m2 + 更新 docs/upstream-pins.md。
HF_REV="${AGENTEAR_LLM_REV:-a2800933352a607ffbb1f814295fc3ff8e10ad69}"
[ "${#HF_REV}" -eq 40 ] || die "revision 必须是完整 40 位 commit：${HF_REV}"
echo "    revision ${HF_REV}（钉死）"

hf_get() {  # hf_get <文件名> <是否必需>
  local rel="$1" required="$2" dest="$MODEL_DIR/$1" ep
  # 大文件用 -C - 续传：8 GB 断在中途不该从头再来。
  # 小文件重下也无所谓，同一条命令统一处理。
  for ep in "${ENDPOINTS[@]}"; do
    if curl -sfL -C - --max-time 3600 --retry 3 --retry-delay 2 \
         -o "$dest" "${ep}/${HF_REPO}/resolve/${HF_REV}/${rel}" 2>/dev/null; then
      echo "    ${rel} ✅"
      return 0
    fi
    # curl 33/36 = 服务端不支持 range / 续传位置不对。此时残留文件是毒的，
    # 删掉让下一个端点从零开始（同 src/download.rs 里的处置）。
    case "$?" in 33|36) rm -f "$dest" ;; esac
  done
  [ "$required" = "required" ] && die "必需文件 ${rel} 下载失败（试过 ${ENDPOINTS[*]}）"
  echo "    ${rel} —— 跳过（仓库里没有）"
}

# mlx-lm 加载需要的全套。权重放最后——前面的小文件几秒就好，
# 早点暴露「配置缺失」这类问题，别等 8 GB 下完才发现。
hf_get config.json                    required
hf_get model.safetensors.index.json   required
hf_get tokenizer.json                 required
hf_get tokenizer_config.json          required
hf_get generation_config.json         optional
hf_get chat_template.jinja            optional
hf_get vocab.json                     optional
hf_get preprocessor_config.json       optional
hf_get processor_config.json          optional
hf_get video_preprocessor_config.json optional
hf_get model-00001-of-00002.safetensors required
hf_get model-00002-of-00002.safetensors required

# FU-24：逐文件 sha256 校验（清单 scripts/pins/ornith-1.0-9b-6bit.sha256，大文件取自 HF API lfs.oid）。
# 镜像（HF_ENDPOINT）上拿到的内容也要过这一关。清单只对默认 revision 有效，
# 用 AGENTEAR_LLM_REV 覆盖时无从校验，明说跳过而不是假装校验过。
MANIFEST="$PINS_DIR/ornith-1.0-9b-6bit.sha256"
if [ "$HF_REV" = "a2800933352a607ffbb1f814295fc3ff8e10ad69" ]; then
  echo "==> 校验 sha256（$(grep -vc '^#' "$MANIFEST") 个文件）"
  (cd "$MODEL_DIR" && grep -v '^#' "$MANIFEST" | shasum -a 256 -c --quiet) \
    || die "sha256 校验失败：模型文件与钉死的清单不符（镜像被改动或下载损坏）。删掉对应文件重跑"
else
  echo "!! revision 被覆盖为 ${HF_REV}，跳过 sha256 清单校验" >&2
fi

# 产物校验：转换/下载中断留下的残缺目录不该被当成可用模型。
# 9B 的 6bit 权重约 7 GB，明显小于这个数就是没下完。
SIZE_MB="$(du -sm "$MODEL_DIR" | cut -f1)"
[ "$SIZE_MB" -gt 6000 ] || die "模型目录只有 ${SIZE_MB} MB，没下完"

echo "==> 完成"
echo "    环境 $VENV"
echo "    模型 $MODEL_DIR (${SIZE_MB} MB)"
echo "    启动：scripts/serve-llm.sh"
