# TTS 模块开发任务 / TTS Module Development Task / งานพัฒนาโมดูล TTS

**开发者 / Developer / ผู้พัฒนา**: Arm
**任务编号 / Task ID**: T3.3.1
**日期 / Date**: 2026-09-03

> 本文档分三部分：**中文 / English / ไทย**。内容相同，选你最舒服的读。
> Three sections: **中文 / English / ไทย**. Same content — read whichever you prefer.
> เอกสารนี้มี 3 ภาษา: **中文 / English / ไทย** เนื้อหาเหมือนกัน อ่านภาษาที่สะดวกที่สุด

---
---

# 第一部分：中文

## 1. 先看清楚你的模块在哪

AgentEar 现在能做的是**语音 → 文字**。完整的目标链路是：

```
说话 → [ASR 已完成] → 文字 → [外部大模型，本项目不做] → 回答文字
                                                            ↓
                                                    【你的模块：TTS】
                                                            ↓
                                                        说出来
```

**中间那个「大模型回答问题」这一段，本项目不做**（依赖外部服务）。
你要做的是**最后一段**：拿到一段文字，用声音把它说出来。

### 一条不可让步的约束：全本地，不许联网

AgentEar 的整个设计前提是**录音和文字不经过任何第三方服务器**。所以：

- ❌ **不能用** Google TTS / Azure TTS / ElevenLabs / OpenAI TTS —— 任何要把文字发到别人服务器的方案，直接出局
- ✅ 必须是**跑在本机**的方案

这不是偏好，是产品的根本卖点。任何联网的实现都会被退回。

### 另一条约束：独立进程

项目规矩（ADR-0002）：**Rust 主程序不内嵌 Python 运行时**；外部推理服务
用什么语言写都行，**但必须是独立进程 + 明确的协议边界**——能单独重启、
单独崩溃，崩了不能把主程序带下去。

**所以你的模块是一个独立的 HTTP 服务。** 用 Python 完全可以。

## 2. 你要交付什么

一个 HTTP 服务，接口就三个：

```
POST /speak
Content-Type: application/json
{ "text": "明天早上会下雨，记得带伞", "lang": "zh" }

→ 200 OK
   Content-Type: audio/wav
   <WAV 字节流>
```

```
GET /health   → 200 {"ok": true}          用于探活
GET /voices   → 200 {"zh": "...", "en": "...", "th": "..."}   当前用的是哪些声音
```

**`lang` 只认三个值：`zh` / `en` / `th`。** 其他值返回 `400` 并说明原因，
**不要猜、不要默认成中文**——猜错了用户听到的是一段听不懂的话，
而他不知道为什么。

### 为什么是 HTTP

因为项目里已经有一个同样形态的边车（LLM 那个），配置里就一行
`llm_url`。你的服务做成一样的形态，主程序加一行 `tts_url` 就能接上。
**不要发明新的通信方式**（不要 stdin/stdout、不要文件轮询、不要 socket 自定义协议）。

## 3. 技术建议：分两步，先跑通再谈好听

### V1（先做这个，目标是**今天就能跑通**）

**用 macOS 自带的 `say` 命令。** 我在目标机器上实测过，三种语言的声音都在：

| 语言 | 声音名 | 实测 |
|---|---|---|
| 中文 | `Tingting` | ✅ 可用 |
| 英文 | `Samantha` | ✅ 可用 |
| **泰语** | **`Kanya`** | ✅ 可用（`สวัสดี! ฉันชื่อกันยา`） |

你可以先在终端里直接试：

```bash
say -v Kanya "สวัสดีครับ วันนี้ฝนจะตก"
say -v Tingting "明天早上会下雨"
say -v Samantha "It will rain tomorrow morning"

# 存成 wav 文件（你的服务要返回的就是这个）
say -v Kanya -o out.aiff "สวัสดีครับ"
afconvert out.aiff out.wav -d LEI16 -f WAVE   # 转成标准 16-bit WAV
```

**为什么先用它**：零安装、零下载、完全离线、三种语言都有。
你可以把全部精力放在**把服务跑通**上，而不是跟模型下载和依赖打架。

**先让整条链路能跑，再去换更好听的声音。** 这个顺序很重要——
反过来做的人通常两头都没做完。

### V2（V1 跑通并合并之后再做）

`say` 的声音是「能听懂但一听就是机器」。要更自然，换成神经网络 TTS：

| 候选 | 泰语支持 | 说明 |
|---|---|---|
| **MMS-TTS**（`facebook/mms-tts-tha`） | ✅ 有专门的泰语模型 | Meta 出的，覆盖 1000+ 语言，HuggingFace 上能直接下 |
| **Piper** | ⚠️ 需要确认有没有泰语音色 | 很快、很小、ONNX 格式 |

**关键点：V2 换实现的时候，第 2 节那个 HTTP 接口一个字都不用改。**
这就是先定接口的价值——你 V1 写的东西不会白费。

⚠️ **V2 不要现在做。** 先把 V1 做完、合并、能用。

## 4. 验收标准（你自己先按这个检查一遍）

写完之后，请你自己逐条跑一遍，**把结果发给我**：

| # | 检查什么 | 期望 |
|---|---|---|
| 1 | 三种语言各发一次请求 | 都返回能播放的 WAV，声音是对应语言 |
| 2 | `lang` 传 `"jp"` | 返回 400 + 清楚的错误信息，**不崩** |
| 3 | `text` 传空字符串 | 返回 400，**不返回一个 0 字节的 wav** |
| 4 | `text` 传 500 个字 | 要么正常返回，要么明确报错。**不能卡住不响应** |
| 5 | 同时发 5 个请求 | 5 个都正确返回，音频内容不串台 |
| 6 | `Ctrl+C` 停掉服务 | 干净退出，不留僵尸进程（`ps aux \| grep say`） |
| 7 | **测一下速度** | 冷启动几秒？一句话（约 20 字）要几秒？**报数字** |

第 7 条特别重要。这个项目里**所有结论都要有实测数字**——
「挺快的」不算结论，「一句话 0.4 秒」才算。

## 5. 怎么开始

```bash
# 1. clone 仓库
git clone git@github.com:iDoris-ai/AgentEar.git && cd AgentEar

# 2. 你的代码放这里（新建目录）
mkdir -p services/tts

# 3. 先在终端里手动试通 say 命令（见第 3 节）
# 4. 再写最小的 HTTP 服务包住它
# 5. 用 curl 逐条过第 4 节的验收表
```

**Python 的话，最小依赖就够了**（标准库的 `http.server` 都行，
或者 `fastapi` + `uvicorn`）。不要一开始就搭框架。

### 提交方式

- **开一个分支**，不要直接推 `main`
- 提 Pull Request，在描述里贴上第 4 节那张表的**实测结果**
- 会有代码评审，改几轮是正常的，不代表做得不好

## 6. 卡住了就问，不要猜

**任何一步觉得说得不清楚，直接问。** 特别是：

- 接口的某个细节不确定 → 问，不要自己定一个
- `say` 在你的机器上行为不一样 → 说出来，可能是 macOS 版本差异
- 觉得某个要求不合理 → **说出来**。你可能是对的，我写的时候没考虑到

**猜错方向做三天，比问一个问题贵得多。**

---
---

# Part 2: English

## 1. Where your module sits

AgentEar today does **speech → text**. The full target pipeline is:

```
speak → [ASR: done] → text → [external LLM: NOT part of this project] → answer text
                                                                              ↓
                                                              【YOUR MODULE: TTS】
                                                                              ↓
                                                                        spoken aloud
```

**The "LLM answers the question" part is out of scope** (external service).
Your job is the **last leg**: take text, speak it out loud.

### Hard constraint #1: fully local, no network

AgentEar's entire premise is that **audio and text never touch a third-party
server**. Therefore:

- ❌ **Cannot use** Google TTS / Azure TTS / ElevenLabs / OpenAI TTS — anything
  that sends text to someone else's server is out
- ✅ Must run **on the local machine**

This is not a preference, it's the product's core value. Any networked
implementation will be sent back.

### Hard constraint #2: separate process

Project rule (ADR-0002): **the Rust daemon embeds no Python runtime**. External
inference services can be written in any language, **but must be a separate
process with a clear protocol boundary** — independently restartable,
independently crashable. Its crash must not take the main program down.

**So your module is a standalone HTTP service.** Python is perfectly fine.

## 2. What you deliver

An HTTP service with three endpoints:

```
POST /speak
Content-Type: application/json
{ "text": "It will rain tomorrow morning, bring an umbrella", "lang": "en" }

→ 200 OK
   Content-Type: audio/wav
   <WAV bytes>
```

```
GET /health   → 200 {"ok": true}
GET /voices   → 200 {"zh": "...", "en": "...", "th": "..."}
```

**`lang` accepts exactly three values: `zh` / `en` / `th`.** Anything else
returns `400` with a clear reason. **Do not guess, do not silently fall back to
Chinese** — a wrong guess means the user hears speech they can't understand and
has no idea why.

### Why HTTP

The project already has a sidecar of exactly this shape (the LLM one), reached
via a single `llm_url` config line. Make yours the same shape and the main
program only needs one `tts_url` line. **Don't invent a new transport**
(no stdin/stdout, no file polling, no custom socket protocol).

## 3. Technical advice: two stages — working first, beautiful later

### V1 (do this first — the goal is **working today**)

**Use macOS's built-in `say` command.** I verified on the target machine that
all three languages have voices installed:

| Language | Voice | Verified |
|---|---|---|
| Chinese | `Tingting` | ✅ works |
| English | `Samantha` | ✅ works |
| **Thai** | **`Kanya`** | ✅ works (`สวัสดี! ฉันชื่อกันยา`) |

Try it in your terminal right now:

```bash
say -v Kanya "สวัสดีครับ วันนี้ฝนจะตก"
say -v Tingting "明天早上会下雨"
say -v Samantha "It will rain tomorrow morning"

# Save to a file (this is what your service returns)
say -v Kanya -o out.aiff "สวัสดีครับ"
afconvert out.aiff out.wav -d LEI16 -f WAVE   # convert to standard 16-bit WAV
```

**Why start here**: zero install, zero download, fully offline, all three
languages present. You can spend all your effort on **making the service work**
instead of fighting model downloads and dependencies.

**Get the whole path working before making it sound better.** This order
matters — people who do it the other way around usually finish neither.

### V2 (only after V1 is merged and working)

`say` sounds understandable but obviously robotic. For natural speech, swap in a
neural TTS:

| Candidate | Thai support | Notes |
|---|---|---|
| **MMS-TTS** (`facebook/mms-tts-tha`) | ✅ dedicated Thai model | From Meta, covers 1000+ languages, downloadable from HuggingFace |
| **Piper** | ⚠️ need to confirm a Thai voice exists | Fast, small, ONNX |

**The key point: when you swap the implementation in V2, the HTTP interface from
section 2 does not change at all.** That's the value of fixing the interface
first — your V1 work is not wasted.

⚠️ **Do not start V2 now.** Finish V1, get it merged, get it usable.

## 4. Acceptance criteria (check these yourself first)

When you're done, run through these yourself and **send me the results**:

| # | Check | Expected |
|---|---|---|
| 1 | One request per language | All three return playable WAV in the right language |
| 2 | `lang: "jp"` | 400 with a clear message, **no crash** |
| 3 | `text: ""` | 400, **not a 0-byte wav** |
| 4 | `text` of 500 characters | Either works or errors clearly. **Must not hang** |
| 5 | 5 concurrent requests | All 5 correct, audio not mixed up between them |
| 6 | `Ctrl+C` the service | Clean exit, no zombie processes (`ps aux \| grep say`) |
| 7 | **Measure speed** | Cold start how long? One sentence (~20 words) how long? **Report numbers** |

Item 7 matters especially. In this project **every conclusion needs measured
numbers** — "it's pretty fast" is not a conclusion, "0.4 s per sentence" is.

## 5. How to start

```bash
git clone git@github.com:iDoris-ai/AgentEar.git && cd AgentEar
mkdir -p services/tts
# 1. Get `say` working by hand in the terminal first (section 3)
# 2. Then write the smallest HTTP service that wraps it
# 3. Then go through section 4 with curl
```

**In Python, minimal dependencies are enough** (even the standard library's
`http.server` works, or `fastapi` + `uvicorn`). Don't start by building a framework.

### How to submit

- **Work on a branch**, don't push to `main`
- Open a Pull Request, and paste the **measured results** of the section-4 table
  into the description
- There will be code review, and a few rounds of changes is normal — it doesn't
  mean you did badly

## 6. Ask when stuck, don't guess

**If any step is unclear, ask.** Especially:

- Unsure about an interface detail → ask, don't decide it yourself
- `say` behaves differently on your machine → say so, it may be a macOS version difference
- A requirement seems unreasonable → **say so.** You may be right and I may have
  missed something

**Three days spent in the wrong direction costs far more than one question.**

---
---

# ส่วนที่ 3: ภาษาไทย

## 1. โมดูลของคุณอยู่ตรงไหน

ตอนนี้ AgentEar ทำได้แค่ **เสียงพูด → ข้อความ** เป้าหมายทั้งเส้นทางคือ:

```
พูด → [ASR: เสร็จแล้ว] → ข้อความ → [LLM ภายนอก: ไม่อยู่ในโปรเจกต์นี้] → ข้อความคำตอบ
                                                                              ↓
                                                            【โมดูลของคุณ: TTS】
                                                                              ↓
                                                                        พูดออกมา
```

**ส่วนที่ LLM ตอบคำถาม ไม่อยู่ในขอบเขตงานนี้** (ใช้บริการภายนอก)
งานของคุณคือ **ส่วนสุดท้าย**: รับข้อความมา แล้วพูดออกมาเป็นเสียง

### ข้อจำกัดที่ต่อรองไม่ได้ ข้อ 1: ทำงานในเครื่องทั้งหมด ห้ามต่อเน็ต

หลักการพื้นฐานของ AgentEar คือ **เสียงและข้อความต้องไม่ผ่าน server ของคนอื่น**
ดังนั้น:

- ❌ **ใช้ไม่ได้**: Google TTS / Azure TTS / ElevenLabs / OpenAI TTS —
  อะไรที่ต้องส่งข้อความไป server คนอื่น ตัดออกทั้งหมด
- ✅ ต้องรัน **ในเครื่องเท่านั้น**

นี่ไม่ใช่ความชอบส่วนตัว แต่เป็นจุดขายหลักของสินค้า
งานที่ต่อเน็ตจะถูกส่งกลับให้แก้

### ข้อจำกัดข้อ 2: ต้องเป็น process แยก

กฎของโปรเจกต์ (ADR-0002): **ตัวหลักที่เขียนด้วย Rust จะไม่ฝัง Python runtime**
บริการ inference ภายนอกเขียนด้วยภาษาอะไรก็ได้ **แต่ต้องเป็น process แยก
พร้อมขอบเขต protocol ที่ชัดเจน** — restart เองได้ crash เองได้
และเวลามันพังต้องไม่ทำให้โปรแกรมหลักพังไปด้วย

**ดังนั้นโมดูลของคุณคือ HTTP service ที่แยกออกมา** ใช้ Python ได้เลย

## 2. สิ่งที่ต้องส่งมอบ

HTTP service ที่มี 3 endpoint:

```
POST /speak
Content-Type: application/json
{ "text": "พรุ่งนี้เช้าฝนจะตก อย่าลืมเอาร่มไปด้วย", "lang": "th" }

→ 200 OK
   Content-Type: audio/wav
   <ข้อมูล WAV>
```

```
GET /health   → 200 {"ok": true}
GET /voices   → 200 {"zh": "...", "en": "...", "th": "..."}
```

**`lang` รับแค่ 3 ค่า: `zh` / `en` / `th`** ค่าอื่นให้ตอบ `400`
พร้อมบอกเหตุผลชัดเจน **อย่าเดา และอย่าเปลี่ยนไปใช้ภาษาจีนเงียบ ๆ** —
ถ้าเดาผิด ผู้ใช้จะได้ยินเสียงภาษาที่เขาฟังไม่เข้าใจ และไม่รู้ว่าทำไม

### ทำไมต้องเป็น HTTP

โปรเจกต์นี้มี sidecar แบบเดียวกันอยู่แล้ว (ตัว LLM) เชื่อมต่อผ่าน config
บรรทัดเดียวคือ `llm_url` ถ้าคุณทำให้เหมือนกัน โปรแกรมหลักก็เพิ่ม `tts_url`
บรรทัดเดียวก็ต่อได้ **อย่าคิดวิธีสื่อสารใหม่** (ไม่เอา stdin/stdout,
ไม่เอาการวน poll ไฟล์, ไม่เอา socket protocol ที่เขียนเอง)

## 3. คำแนะนำทางเทคนิค: แบ่ง 2 ขั้น — ให้ใช้งานได้ก่อน แล้วค่อยทำให้เสียงเพราะ

### V1 (ทำอันนี้ก่อน — เป้าหมายคือ **ให้รันได้วันนี้**)

**ใช้คำสั่ง `say` ที่มาพร้อม macOS** ผมทดสอบบนเครื่องเป้าหมายแล้ว
มีเสียงครบทั้ง 3 ภาษา:

| ภาษา | ชื่อเสียง | ทดสอบแล้ว |
|---|---|---|
| จีน | `Tingting` | ✅ ใช้ได้ |
| อังกฤษ | `Samantha` | ✅ ใช้ได้ |
| **ไทย** | **`Kanya`** | ✅ ใช้ได้ (`สวัสดี! ฉันชื่อกันยา`) |

ลองใน terminal ได้เลยตอนนี้:

```bash
say -v Kanya "สวัสดีครับ วันนี้ฝนจะตก"
say -v Tingting "明天早上会下雨"
say -v Samantha "It will rain tomorrow morning"

# บันทึกเป็นไฟล์ (นี่คือสิ่งที่ service ของคุณต้องส่งกลับ)
say -v Kanya -o out.aiff "สวัสดีครับ"
afconvert out.aiff out.wav -d LEI16 -f WAVE   # แปลงเป็น WAV 16-bit มาตรฐาน
```

**ทำไมเริ่มจากอันนี้**: ไม่ต้องติดตั้งอะไร ไม่ต้องดาวน์โหลด ทำงานออฟไลน์
และมีครบ 3 ภาษา คุณจะได้ทุ่มแรงไปกับ **การทำให้ service ทำงานได้**
ไม่ต้องไปสู้กับการดาวน์โหลดโมเดลและ dependency

**ทำให้เส้นทางทั้งหมดใช้งานได้ก่อน แล้วค่อยไปทำให้เสียงเพราะขึ้น**
ลำดับนี้สำคัญ — คนที่ทำสลับลำดับ มักจะทำไม่เสร็จทั้งสองอย่าง

### V2 (เริ่มหลังจาก V1 merge เข้าไปและใช้งานได้แล้วเท่านั้น)

เสียงจาก `say` ฟังรู้เรื่องแต่ฟังออกว่าเป็นเครื่อง ถ้าต้องการเสียงธรรมชาติ
ให้เปลี่ยนไปใช้ neural TTS:

| ตัวเลือก | รองรับภาษาไทย | หมายเหตุ |
|---|---|---|
| **MMS-TTS** (`facebook/mms-tts-tha`) | ✅ มีโมเดลภาษาไทยเฉพาะ | ของ Meta ครอบคลุม 1000+ ภาษา ดาวน์โหลดจาก HuggingFace ได้ |
| **Piper** | ⚠️ ต้องเช็คว่ามีเสียงภาษาไทยไหม | เร็ว เล็ก รูปแบบ ONNX |

**จุดสำคัญ: เวลาเปลี่ยนไปใช้ V2 interface HTTP ในหัวข้อ 2 ไม่ต้องแก้แม้แต่ตัวเดียว**
นั่นคือคุณค่าของการกำหนด interface ไว้ก่อน — งาน V1 ของคุณจะไม่เสียเปล่า

⚠️ **อย่าเริ่ม V2 ตอนนี้** ทำ V1 ให้เสร็จ merge ให้ได้ ใช้งานให้ได้ก่อน

## 4. เกณฑ์การตรวจรับ (ลองเช็คเองก่อน)

พอทำเสร็จ ให้ลองทำตามนี้ทีละข้อ แล้ว **ส่งผลลัพธ์มาให้ผม**:

| # | เช็คอะไร | ผลที่ควรได้ |
|---|---|---|
| 1 | ส่ง request ภาษาละ 1 ครั้ง | ได้ WAV ที่เล่นได้ทั้ง 3 และเสียงตรงภาษา |
| 2 | ส่ง `lang: "jp"` | ตอบ 400 พร้อมข้อความชัดเจน **ไม่ crash** |
| 3 | ส่ง `text: ""` | ตอบ 400 **ไม่ใช่ wav ขนาด 0 byte** |
| 4 | ส่ง `text` ยาว 500 ตัวอักษร | ทำงานได้ หรือแจ้ง error ชัดเจน **ต้องไม่ค้าง** |
| 5 | ส่ง 5 request พร้อมกัน | ถูกต้องทั้ง 5 เสียงไม่สลับกัน |
| 6 | กด `Ctrl+C` หยุด service | ปิดสะอาด ไม่เหลือ zombie process (`ps aux \| grep say`) |
| 7 | **วัดความเร็ว** | cold start กี่วินาที? 1 ประโยค (~20 คำ) กี่วินาที? **บอกเป็นตัวเลข** |

ข้อ 7 สำคัญเป็นพิเศษ ในโปรเจกต์นี้ **ทุกข้อสรุปต้องมีตัวเลขที่วัดได้** —
"เร็วอยู่นะ" ไม่นับเป็นข้อสรุป แต่ "0.4 วินาทีต่อประโยค" นับ

## 5. เริ่มต้นอย่างไร

```bash
git clone git@github.com:iDoris-ai/AgentEar.git && cd AgentEar
mkdir -p services/tts
# 1. ลองใช้ `say` ด้วยมือใน terminal ให้ได้ก่อน (หัวข้อ 3)
# 2. แล้วเขียน HTTP service เล็กที่สุดมาครอบมัน
# 3. แล้วใช้ curl ไล่ตามตารางในหัวข้อ 4
```

**ถ้าใช้ Python ใช้ dependency น้อยที่สุดก็พอ** (`http.server` จาก
standard library ก็ได้ หรือ `fastapi` + `uvicorn`) อย่าเริ่มด้วยการสร้าง framework

### วิธีส่งงาน

- **ทำบน branch แยก** อย่า push เข้า `main`
- เปิด Pull Request และแปะ **ผลการวัดจริง** จากตารางหัวข้อ 4 ลงในคำอธิบาย
- จะมีการ review code และแก้หลายรอบเป็นเรื่องปกติ ไม่ได้หมายความว่าทำไม่ดี

## 6. ติดตรงไหนให้ถาม อย่าเดา

**ถ้าตรงไหนอธิบายไม่ชัด ถามได้เลย** โดยเฉพาะ:

- ไม่แน่ใจรายละเอียดของ interface → ถาม อย่าตัดสินใจเอง
- `say` ทำงานไม่เหมือนกันบนเครื่องคุณ → บอกมา อาจเป็นเพราะ macOS เวอร์ชันต่างกัน
- รู้สึกว่าข้อกำหนดบางข้อไม่สมเหตุสมผล → **บอกมา** คุณอาจถูก
  และผมอาจคิดไม่ถึงตอนเขียน

**ทำผิดทางไป 3 วัน แพงกว่าถาม 1 คำถามเยอะ**
