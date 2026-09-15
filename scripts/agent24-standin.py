#!/usr/bin/env python3
"""**假宿主**：证明「AgentEar 提出、宿主执行」这条路走得通。

它不是产品，是一份**可执行的接口验证**（ADR-0008 §4 的出口判据）。
把它想成 Agent24 那一侧的最小骨架：

    ① 问 AgentEar：这句话是什么意思？   → `agentear --match-command <文本> --json`
    ② 宿主自己决定要不要确认（**在这里就是它自己的 UI**）
    ③ 宿主自己执行（**AgentEar 全程不碰执行**）
    ④ 宿主自己展示回执

⚠️ 这里**故意不调用** `--run-command`。那个入口是给 AgentEar 自己验证用的，
宿主用它就等于把执行又塞回 AgentEar 里——那正是我们要分开的东西。

用法：
    scripts/agent24-standin.py "记到notion 明天要测 AEC"      # 交互式确认
    scripts/agent24-standin.py "搜索 rust async" --yes        # 跳过确认（给自动化用）
    scripts/agent24-standin.py "今天天气怎么样"               # 不命中 → 交给对话
"""

import argparse
import json
import pathlib
import subprocess
import sys
import urllib.request

AGENTEAR = pathlib.Path(__file__).resolve().parent.parent / "target/release/agentear"


def ask_agentear(text: str) -> dict:
    """① 只问「这是什么意思」。**不做任何执行。**"""
    out = subprocess.run(
        [str(AGENTEAR), "--match-command", text, "--json"],
        capture_output=True, text=True, check=True,
    ).stdout
    return json.loads(out)


def render_preview(proposal: dict) -> str:
    """② 宿主该显示给用户看的东西（问句由 AgentEar 给，因为**它和要执行的内容同源**）。"""
    action = proposal["action"]
    lines = [
        f"  将要执行：{action['type']}",
        f"  命中短语：{proposal['matched']}",
        f"  槽位（对象）：{proposal['rest']!r}",
        f"  需要确认：{'是' if proposal['needs_confirm'] else '否'}",
    ]
    # 契约：**有确认才有问句**（见 ADR-0008 §3）
    if proposal.get("prompt"):
        lines.append(f"  提示语：{proposal['prompt']}")
    return "\n".join(lines)


def execute(proposal: dict) -> str:
    """③ **宿主执行**。AgentEar 在这条路上没有任何执行代码被调用。"""
    action = proposal["action"]
    kind = action["type"]
    # 槽位填充规则与 AgentEar 一致：{rest} / {text}（宿主照抄这条约定即可）
    def fill(t: str) -> str:
        return t.replace("{rest}", proposal["rest"] or "").replace("{text}", proposal["text"])

    if kind == "open_url":
        url = fill(action["url"])
        subprocess.run(["/usr/bin/open", url], check=True)
        return f"已打开 {url}"
    if kind == "http_post":
        url = fill(action["url"])
        body = fill(action.get("body") or "")
        req = urllib.request.Request(
            url, data=body.encode(),
            headers={"Content-Type": "application/json"}, method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                payload = resp.read().decode("utf-8", "replace").strip()
            # ④ 回执由**宿主**展示（AgentEar 不负责界面）
            return f"HTTP {resp.status}；返回：{payload[:200]}"
        except urllib.error.HTTPError as e:
            detail = e.read().decode("utf-8", "replace").strip()
            return f"失败 HTTP {e.code}：{detail[:200]}"
    if kind == "builtin":
        # 本机动作：壳子可以直接给用户一个提示，或者调 AgentEar 的 builtin 入口。
        # 这里只演示「宿主可以自己决定怎么处理」，不假装它已经接好了。
        return f"（本机动作 {action['name']}={action.get('value')!r}：由宿主决定怎么落地）"
    return f"（宿主不认识的动作类型 {kind!r}——这不该发生，动作集合是封闭的）"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("text")
    ap.add_argument("--yes", action="store_true", help="跳过确认（自动化用）")
    args = ap.parse_args()

    print(f"① 问 AgentEar：{args.text!r}")
    proposal = ask_agentear(args.text)

    if proposal["matched"] is None:
        print("   → 没命中任何指令。**宿主此时该走对话那条路**（AgentEar 的 LLM）：")
        print("       agentear --ask " + json.dumps(args.text, ensure_ascii=False))
        return 0

    print("② 宿主展示给用户：")
    print(render_preview(proposal))

    if proposal["needs_confirm"] and not args.yes:
        answer = input("③ 确认执行？[y/N] ").strip().lower()
        if answer not in ("y", "yes"):
            print("   → 用户没确认，**什么都没执行**")
            return 0

    print("③ 宿主执行……")
    receipt = execute(proposal)
    print(f"④ 回执（由宿主展示）：{receipt}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
