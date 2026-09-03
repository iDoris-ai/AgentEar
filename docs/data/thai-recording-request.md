# 泰语录音请求 / Thai Recording Request

**用途**:测试几个本地语音识别模型在「泰语夹英文技术词」场景下的准确率。
**Purpose**: Evaluating local speech-recognition models on Thai speech containing
English technical terms.

---

## ⚠️ 请先做这一步 / Please do this FIRST

下面的泰语句子是**非母语者写的,可能有语法错误或不自然的地方**。

**请先随意修改成自然的泰语**,然后:

1. 照**你改完的版本**朗读并录音
2. **把改完的文本一起发回来** ← 这一步很重要

回传的文本会被当作评分标准答案。如果录音内容和文本对不上,整个测试就没有意义了。

---

The Thai sentences below were written by a **non-native speaker** and may contain
grammatical errors or unnatural phrasing.

**Please feel free to rewrite them into natural Thai first**, then:

1. Read aloud and record **your corrected version**
2. **Send back the corrected text as well** ← this matters

The returned text will be used as the scoring reference. If the recording and the
text don't match, the whole test is meaningless.

**English technical terms should stay in English** — please pronounce them the way
you normally would when speaking Thai (a Thai accent is exactly what we want to test;
please don't force a native-English pronunciation).

---

## 录音方式 / How to record

**两条路都行,任选一条。**

### A. 网页录音工具(有电平表,自动出 16 kHz WAV)

<https://idoris-ai.github.io/AgentEar/thai-recorder.html>

在手机或电脑的浏览器里打开,按页面提示走。它会在你录完之后自己检查音量、
时长、有没有丢帧,并把「你实际念的文本」和录音绑在一起 ——
这几件事手机录音机做不到,是这条路唯一多出来的价值。
⚠️ 录完**必须自己按「保存」**,页面不会自动存;关掉页面就没了。

### B. 手机自带录音机(更简单,一样能用)

页面里也写着这条退路。**格式、采样率都不用管**,我这边转。
如果 A 打不开、麦克风权限拿不到、或者你就是懒得折腾,直接用 B ——
两条路产出的语料我们一样用。

- 手机自带录音机就行,不需要专业设备
  A phone voice recorder is fine — no special equipment needed
- **安静的房间**,正常语速,像平常说话一样
  A **quiet room**, normal speaking pace, just talk as you normally would
- 三段**分开录三个文件**
  Please record the three passages as **three separate files**
- 格式随意(m4a / mp3 / wav 都可以)
  Any format is fine (m4a / mp3 / wav)
- 念错了不用重来,**接着念下去就行**——但如果偏离了文本,请在回传时把文本改成
  你实际念的内容
  If you misspeak, just keep going — but if you deviated from the text, please
  edit the text to match what you actually said

---

## 第 1 段:DevOps(英文技术词密集)

> สวัสดีครับ วันนี้ผมจะอธิบายวิธี deploy ระบบของเราขึ้น production
>
> เราใช้ Docker สร้าง container แล้วส่งขึ้น Kubernetes cluster
>
> ก่อน merge pull request ทุกครั้ง ต้องรัน unit test ให้ผ่านก่อน
>
> ถ้า build ไม่ผ่าน ระบบ CI จะแจ้งเตือนใน Slack ทันที
>
> ฐานข้อมูลเราใช้ PostgreSQL ส่วน cache ใช้ Redis
>
> ตอนนี้เรามี server ทั้งหมด 12 เครื่อง แบ่งเป็น 3 zone
>
> ปัญหาที่เจอบ่อยที่สุดคือ memory leak ใน service ตัวเก่า
>
> พรุ่งนี้เช้าเก้าโมงครึ่ง เราจะ release version ใหม่ครับ

**要测的英文词**:deploy, production, Docker, container, Kubernetes, cluster,
merge, pull request, unit test, build, CI, Slack, PostgreSQL, cache, Redis,
server, zone, memory leak, service, release, version
（另含数字 12 / 3 / 九点半）

---

## 第 2 段:日常协作(技术词密度中等)

> เมื่อวานผม review code ของทีมแล้ว เจอ bug อยู่สองสามจุด
>
> ช่วย fix แล้ว push ขึ้น branch develop ภายในวันนี้ด้วยนะครับ
>
> ผมจะ comment รายละเอียดไว้ใน GitHub ให้ดู
>
> เรื่อง performance ตอนนี้ API ตอบกลับช้ากว่า 2 วินาที
>
> ต้องหาวิธี optimize query ก่อนที่ลูกค้าจะร้องเรียน
>
> บ่ายนี้มีประชุมกับทีม design ตอนบ่าย 2 โมง
>
> อย่าลืมอัปเดต document ใน Notion ด้วยนะครับ
>
> ขอบคุณมากครับ ไว้เจอกันสัปดาห์หน้า

**要测的英文词**:review, code, bug, fix, push, branch, develop, comment,
GitHub, performance, API, optimize, query, design, document, Notion
（另含数字 2 秒 / 下午 2 点）

---

## 第 3 段:纯泰语对照组(**不含任何英文**)

这一段是对照用的:同一个人、同样的录音条件,只是没有英文词。
用来分离「模型的泰语能力」和「模型处理夹杂英文的能力」。

> เมื่อเช้าฝนตกหนักมาก ทำให้การจราจรติดขัดกว่าปกติ
>
> ผมออกจากบ้านตั้งแต่หกโมงครึ่ง แต่ยังไปถึงที่ทำงานสาย
>
> ช่วงนี้อากาศเปลี่ยนแปลงบ่อย หลายคนในทีมไม่สบาย
>
> สัปดาห์หน้าผมจะลาพักร้อนสามวัน ไปเที่ยวกับครอบครัวที่เชียงใหม่
>
> ถ้ามีเรื่องด่วนติดต่อผมทางโทรศัพท์ได้ตลอดเวลา
>
> ขอให้ทุกคนดูแลสุขภาพด้วยนะครับ

---

## 可选加分项 / Optional bonus

如果方便的话,再录**第 4 段:即兴说**——不看稿子,随便讲两三分钟你最近在做的技术工作。

这一段**不需要文字稿**(我们不拿它算分),但它是最接近真实使用场景的样本:
有停顿、有「呃」、有重复、语速不均匀。念稿子念得再自然,也和真的说话不一样。

If convenient, please also record a **4th passage: unscripted** — just talk for
two or three minutes about what you're working on, without a script.

No transcript needed for this one (we won't score it), but it's the closest thing
to real usage: pauses, filler words, repetitions, uneven pace. Read speech is always
cleaner than actual speech, no matter how naturally you read.

---

## 回传清单 / What to send back

- [ ] 三个录音文件(第 1、2、3 段)
- [ ] **修改后的三段文本**← 别忘了这个
      - 走网页那条路:页面底部按「下载修改后的文本」,得到 `thai-passages-<你的名字>.txt`
      - 走手机那条路:把本文档里的三段文本改好,直接发文字回来就行
- [ ] (可选)第 4 段即兴录音

- [ ] Three audio files (passages 1, 2, 3)
- [ ] **The corrected text of all three passages** ← don't forget
      - Web tool: press "Download corrected text" at the bottom → `thai-passages-<your-name>.txt`
      - Phone route: just edit the three passages in this document and send the text back
- [ ] (Optional) Passage 4, unscripted

全部发回给把这份文档给你的人就行,微信 / LINE / 邮件都可以。
Send everything back to whoever gave you this document — WeChat, LINE or email, whatever we already use.

ขอบคุณมากครับ 🙏
