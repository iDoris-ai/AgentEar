import re
files={'参考(另一ASR)':'ref.txt','Nano':'hyp.txt','SenseVoice':'hyp_sensevoice.txt','Paraformer':'hyp_paraformer.txt'}
txt={k:open(v).read().lower().replace('，','').replace('。','').replace(' ','') for k,v in files.items()}
# (正确词, 各系统实际输出的判定正则)
terms=[
 ('MacBook',    ['macbook']),
 ('Mac mini',   ['macmini']),
 ('raw 目录',    ['raw']),
 ('knowledge base',['knowledgebase']),
 ('24小时',      ['24小时','24r']),
 ('AI',         ['ai']),
 ('idea',       ['idea']),
 ('report',     ['report']),
 ('wifi',       ['wifi']),
 ('爱国者',      ['爱国者']),
 ('闲鱼',        ['闲鱼']),
 ('8G',         ['8g','八g']),
 ('本地模型',     ['本地模型']),
]
hdr=f"{'正确词':<16}"+"".join(f"{k:<14}" for k in files)
print(hdr); print('-'*len(hdr.encode('gbk','ignore')))
score={k:0 for k in files}
for word,pats in terms:
    row=f"{word:<16}"
    for k in files:
        hit=any(p in txt[k] for p in pats)
        score[k]+=hit
        row+=f"{'✓':<14}" if hit else f"{'✗':<14}"
    print(row)
print('-'*40)
print(f"{'命中数':<16}"+"".join(f"{score[k]}/{len(terms):<12}" for k in files))
