import base64
import json
import urllib.request

RPC = "https://mainnet.helius-rpc.com/?api-key=8ad1f1ff-83b9-414f-a5c9-1b10ba12316f"
PID = "9n3dSLrERZQp95dHXywft7xV8D8xnGFLaUHtEhQVaXaC"


def rpc(method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(RPC, data=body, headers={"Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=45).read())["result"]


def b58e(b):
    al = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
    n = int.from_bytes(b, "big")
    out = ""
    while n:
        n, r = divmod(n, 58)
        out = al[r] + out
    return "1" * (len(b) - len(b.lstrip(b"\x00"))) + (out or "")


v = rpc("getAccountInfo", [PID, {"encoding": "base64"}])["value"]
d = base64.b64decode(v["data"][0])
mint0 = b58e(d[73:105])
mint1 = b58e(d[105:137])
vault0 = b58e(d[137:169])
vault1 = b58e(d[169:201])
print("mint0 =", mint0)
print("mint1 =", mint1)
print("vault0 =", vault0)
print("vault1 =", vault1)

WSOL = "So11111111111111111111111111111111111111112"
in_mint = mint0 if mint0 == WSOL else mint1
print("\ninput_mint (wSOL tarafi) =", in_mint)

print("\n--- getTokenSupply ---")
print(json.dumps(rpc("getTokenSupply", [in_mint]))[:300])

print("\n--- getTokenLargestAccounts ---")
res = rpc("getTokenLargestAccounts", [in_mint])
print(json.dumps(res)[:1500])
print("\ndönen hesap sayısı:", len(res["value"]))
