import re, sys, unicodedata

def norm(s):
    s = unicodedata.normalize('NFKC', s)
    s = s.lower()
    # 去标点与空白，保留中英文数字
    s = re.sub(r'[^\w一-鿿]', '', s)
    return s

def edit(a, b):
    if len(a) < len(b): a, b = b, a
    prev = list(range(len(b)+1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j]+1, cur[j-1]+1, prev[j-1]+(ca != cb)))
        prev = cur
    return prev[-1]

ref = norm(open(sys.argv[1]).read())
hyp = norm(open(sys.argv[2]).read())
d = edit(ref, hyp)
print(f"ref 长度: {len(ref)}  hyp 长度: {len(hyp)}")
print(f"编辑距离: {d}")
print(f"CER: {d/len(ref)*100:.2f}%")
