import re,unicodedata,glob,os
def norm(s,sf=False):
    s=unicodedata.normalize('NFKC',s).lower()
    s=re.sub(r'[^\w一-鿿]','',s)
    if sf: s=re.sub(r'[嗯呃啊哦呀吧对了的]','',s)
    return s
def edit(a,b):
    if len(a)<len(b): a,b=b,a
    prev=list(range(len(b)+1))
    for i,ca in enumerate(a,1):
        cur=[i]
        for j,cb in enumerate(b,1): cur.append(min(prev[j]+1,cur[j-1]+1,prev[j-1]+(ca!=cb)))
        prev=cur
    return prev[-1]
ref=open('ref.txt').read()
print(f"{'模型':<22}{'CER(原始)':>12}{'CER(剔语气词)':>16}")
print("-"*52)
for name,f in [("Fun-ASR-Nano q4km","hyp.txt"),("SenseVoiceSmall q8","hyp_sensevoice.txt"),("Paraformer q8","hyp_paraformer.txt")]:
    if not os.path.exists(f): continue
    h=open(f).read()
    a=edit(norm(ref),norm(h))/len(norm(ref))*100
    b=edit(norm(ref,1),norm(h,1))/len(norm(ref,1))*100
    print(f"{name:<22}{a:>11.2f}%{b:>15.2f}%")
