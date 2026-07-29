import re, unicodedata
def norm(s, strip_filler=False):
    s = unicodedata.normalize('NFKC', s).lower()
    s = re.sub(r'[^\w一-鿿]', '', s)
    if strip_filler:
        s = re.sub(r'[嗯呃啊哦呀吧对了的]', '', s)
    return s
def edit(a,b):
    if len(a)<len(b): a,b=b,a
    prev=list(range(len(b)+1))
    for i,ca in enumerate(a,1):
        cur=[i]
        for j,cb in enumerate(b,1):
            cur.append(min(prev[j]+1,cur[j-1]+1,prev[j-1]+(ca!=cb)))
        prev=cur
    return prev[-1]
ref_raw=open('ref.txt').read(); hyp_raw=open('hyp.txt').read()
for label,sf in [("原始(含语气词)",False),("剔除语气词/助词",True)]:
    r,h=norm(ref_raw,sf),norm(hyp_raw,sf)
    d=edit(r,h)
    print(f"{label:20s} ref={len(r):4d} hyp={len(h):4d} 距离={d:4d} CER={d/len(r)*100:5.2f}%")
