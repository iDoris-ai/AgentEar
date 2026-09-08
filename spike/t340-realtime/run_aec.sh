#!/bin/bash
# T3.4.0 AEC 重测：带「扬声器确实在响」的有效性守卫。
# 每轮先跑 off 组当守卫——它若识别不出 TTS 内容，说明扬声器静音/音量过低，该轮作废。
set -u
cd "$(dirname "$0")"
TTS=../tts/zh_anchor.wav
ROUNDS=${1:-5}
REF="今天清迈天气还不错"

vol() { osascript -e 'get volume settings' 2>/dev/null; }
rms() { python3 -c "
import wave,struct,math,sys
w=wave.open(sys.argv[1]);n=w.getnframes()
if n==0: print('0.0'); raise SystemExit
d=struct.unpack(f'<{n}h',w.readframes(n))
print(f'{math.sqrt(sum(x*x for x in d)/len(d)):.1f}')" "$1"; }
asr() { speech transcribe --engine qwen3 -m 1.7B "$1" 2>/dev/null | grep '^Result:' | sed 's/^Result: //'; }

echo "开始音量状态: $(vol)"
echo

for i in $(seq 1 "$ROUNDS"); do
  # --- off 组（守卫）---
  ./vpio_bin "$TTS" "g${i}_off.wav" --no-vpio >/dev/null 2>&1
  ffmpeg -y -i "g${i}_off.wav" -ar 16000 -ac 1 "g${i}_off16.wav" >/dev/null 2>&1
  OFF_RMS=$(rms "g${i}_off16.wav"); OFF_TXT=$(asr "g${i}_off16.wav")

  # --- on 组 ---
  ./vpio_bin "$TTS" "g${i}_on.wav" >/dev/null 2>&1
  ffmpeg -y -i "g${i}_on.wav" -filter_complex "pan=mono|c0=c0" -ar 16000 "g${i}_on16.wav" >/dev/null 2>&1
  ON_RMS=$(rms "g${i}_on16.wav"); ON_TXT=$(asr "g${i}_on16.wav")

  # 守卫：off 组必须复现 TTS 内容，否则本轮无效
  if [[ "$OFF_TXT" == *"$REF"* ]]; then VALID="有效"; else VALID="❌无效(扬声器没响?)"; fi
  # 判据：on 组是否复现 TTS 内容
  if [[ "$ON_TXT" == *"$REF"* ]]; then LEAK="是"; else LEAK="否"; fi

  printf "r%s [%s]\n" "$i" "$VALID"
  printf "   off RMS=%-8s ASR=[%s]\n" "$OFF_RMS" "$OFF_TXT"
  printf "   on  RMS=%-8s ASR=[%s]  复现TTS内容=%s\n" "$ON_RMS" "$ON_TXT" "$LEAK"
done

echo
echo "结束音量状态: $(vol)"
