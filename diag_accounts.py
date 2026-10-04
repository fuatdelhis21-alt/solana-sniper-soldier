import json, urllib.request, base64

env = open('/opt/solana-sniper-soldier/.env').read()
rpc = [l.split('=',1)[1].strip() for l in env.splitlines() if l.startswith('RPC_URL=')][0]

def post(method, params):
    req = urllib.request.Request(rpc, json.dumps({'jsonrpc':'2.0','id':1,'method':method,'params':params}).encode(), {'Content-Type':'application/json','User-Agent':'diag/1.0'})
    return json.load(urllib.request.urlopen(req, timeout=30))

SIG = '2cqNMUD5Tei5g6UyE4qFEFWiUmWRvSo5JBGYrk1usf6BPZxhmwAK8rdiof5yXyrDYffLvjN35F16yr3nDZ4oLH5Z'
r = post('getTransaction', [SIG, {'maxSupportedTransactionVersion':0,'encoding':'json'}])['result']
keys = r['transaction']['message']['accountKeys']
print('=== account keys ===')
for i, k in enumerate(keys):
    print(i, k)
print('=== instructions ===')
for ix in r['transaction']['message']['instructions']:
    prog = keys[ix['programIdIndex']]
    print('prog:', prog[:9], 'accounts:', [keys[i] for i in ix['accounts']])
    print('   data:', ix['data'][:80])
print('=== logs ===')
for l in (r['meta'].get('logMessages') or [])[:20]:
    print(l[:160])
