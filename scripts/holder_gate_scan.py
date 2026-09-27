"""Holder kapısının (30/70) gerçek mainnet havuzlarında ne sıklıkla geçtiğini ölçer.

Kritik nokta: getTokenLargestAccounts SADECE 20 hesap döndürür. Bu yüzden
"top-20 toplamı" pratikte "en büyük 20 hesabın toplamı"dır ve dağıtılmış
arzı olan tokenlarda bile %70'i aşabilir.
"""
import json
import urllib.request

RPC = "https://mainnet.helius-rpc.com/?api-key=8ad1f1ff-83b9-414f-a5c9-1b10ba12316f"
MAX_SINGLE, MAX_TOP20 = 30.0, 70.0


def rpc(method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(RPC, data=body, headers={"Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=45).read())["result"]


MINTS = [
    ("RAY", "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R"),
    ("JUP", "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN"),
    ("BONK", "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263"),
    ("PYTH", "HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3"),
    ("JTO", "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL"),
    ("WIF", "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm"),
    ("ORCA", "orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE"),
    ("MNDE", "MNDEFzGvMt87ueuHvVU9VcTqsAP5b3fTGPsHuuPA5ey"),
    ("TNSR", "TNSRxcUxoT9xBG3de7PiJyTDYu7kskLqcpddxnEJAS6"),
    ("DRIFT", "DriFtupJYLTosbwoN8koMbEYSx54aFAVLddWsbksjwg7"),
]

print(f"{'TOKEN':8} {'top1%':>8} {'top20%':>8}  {'n':>3}  VERDICT")
print("-" * 50)
passed = 0
for name, mint in MINTS:
    try:
        supply = int(rpc("getTokenSupply", [mint])["value"]["amount"])
        largest = rpc("getTokenLargestAccounts", [mint])["value"]
        amts = sorted((int(a["amount"]) for a in largest), reverse=True)
        if not amts or supply == 0:
            print(f"{name:8} {'-':>8} {'-':>8}  {0:>3}  NO_DATA")
            continue
        top = amts[0] / supply * 100.0
        top20 = sum(amts[:20]) / supply * 100.0
        ok = top <= MAX_SINGLE and top20 <= MAX_TOP20
        passed += ok
        print(
            f"{name:8} {top:8.2f} {top20:8.2f}  {len(amts):>3}  "
            f"{'PASS' if ok else 'REJECT'}"
        )
    except Exception as e:  # noqa: BLE001
        print(f"{name:8} ERROR {e}")

print("-" * 50)
print(f"GEÇEN: {passed}/{len(MINTS)}")
