"""Holder yoğunlaşma kapısını (30/70) aday havuzlar için hesaplar."""
import json
import urllib.request

RPC = "https://mainnet.helius-rpc.com/?api-key=8ad1f1ff-83b9-414f-a5c9-1b10ba12316f"
MAX_SINGLE, MAX_TOP20 = 30.0, 70.0


def rpc(method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(RPC, data=body, headers={"Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=45).read())["result"]


CANDIDATES = [
    ("WSOL/RAY", "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
     ["9Jgp8NpqEDFd5d3RQPfuRY7gMgRFByTNFmi68Ph1yvVb"]),
    ("WSOL/CAPX", "7AoBuYcGKQYadxc9wmGxpuu29bpC1EDQezkoXACWZRFF",
     ["8XAz8JCHaVM2NXXp83yNkc4R3AHxmCimoabVVZCpGUos"]),
]

for name, mint, vaults in CANDIDATES:
    print("=" * 70)
    print(name, mint)
    supply = int(rpc("getTokenSupply", [mint])["value"]["amount"])
    largest = rpc("getTokenLargestAccounts", [mint])["value"]
    print(f"  supply = {supply}")
    print(f"  getTokenLargestAccounts döndürdü: {len(largest)} hesap")
    ex = set(vaults)
    amts = []
    for a in largest:
        tag = "  <-- HAVUZ VAULT (hariç)" if a["address"] in ex else ""
        print(f"    {a['address']}  {a['amount']}{tag}")
        if a["address"] not in ex:
            amts.append(int(a["amount"]))
    amts.sort(reverse=True)
    if not amts:
        print("  >>> ÖLÇÜLEBİLİR HOLDER YOK -> fail-closed REJECT")
        continue
    top = amts[0] / supply * 100.0
    top20 = sum(amts[:20]) / supply * 100.0
    print(f"  ölçülen holder sayısı = {len(amts)}")
    print(f"  top1  = %{top:.2f}   (<=30 : {top <= MAX_SINGLE})")
    print(f"  top20 = %{top20:.2f}  (<=70 : {top20 <= MAX_TOP20})")
    verdict = "PASS" if top <= MAX_SINGLE and top20 <= MAX_TOP20 else "REJECT"
    print(f"  >>> HOLDER KAPISI: {verdict}")
