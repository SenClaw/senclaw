import re,sys
STOP=set('kanban task nhiệm vụ nhiem vu đã được hoàn thành thành công và không có vấn đề gì mọi thứ đều ổn theo yêu cầu hoàn completed the was has a của cho với assistant người dùng'.split())
def parse(path):
    cards=[];cur=None
    for line in open(path,encoding='utf8'):
        line=line.rstrip('\n')
        m=re.match(r'\[(c\d+)\] (.*?) \((.*?), expect (\w+)\): ',line)
        if m:
            cur={'id':m.group(1),'title':m.group(2),'expect':m.group(4),'facts':[]};cards.append(cur);continue
        if cur is None: continue
        m=re.match(r'\s+stored \[(JUNK\?|useful)\] (.*)',line)
        if m: cur['facts'].append({'auto':m.group(1),'text':m.group(2)})
        if line.startswith('project bank after'): cur=None
    return cards
def content_words(card,text):
    t=re.sub(r'(["\'“”‘’]).*?\1','',text)   # drop the quoted title
    words=[w for w in re.findall(r"[\wÀ-ỹ]+",t.lower()) if w not in STOP]
    return words
# hand labels, fixed BEFORE run 3 was read (rules: junk = tautology, duplicate of another live fact, assistant chatter, question-state, trivial-by-design)
for p in sys.argv[1:]:
    cards=parse(p); n=0; taut=[]; review=[]
    for c in cards:
        for f in c['facts']:
            n+=1
            cw=content_words(c,f['text'])
            f['taut']= len(cw)<=2
            if f['taut']: taut.append((c['id'],f['text']))
            elif f['auto']=='JUNK?': review.append((c['id'],f['text']))
    none_with=[c['id'] for c in cards if c['expect']=='none' and c['facts']]
    print(f"== {p.split('/')[-1]}: stored {n}, tautological {len(taut)}, none-cards-with-fact {len(none_with)}/8")
    for cid,t in taut: print('   TAUT  ',cid,t[:120])
    for cid,t in review: print('   REVIEW',cid,t[:220])
