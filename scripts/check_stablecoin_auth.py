import json
import urllib.request

RPC = "https://mainnet.helius-rpc.com/?api-key=8ad1f1ff-83b9-414f-a5c9-1b10ba12316f"


def rpc(method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(RPC, data=body, headers={"Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=45).read())["result"]


MINTS = [
    ("USDC", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
    ("USDT", "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"),
    ("wSOL", "So11111111111111111111111111111111111111112"),
    ("RAY", "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R"),
    ("JUP", "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN"),
    ("BONK", "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263"),
]

for name, mint in MINTS:
    v = rpc("getAccountInfo", [mint, {"encoding": "jsonParsed"}])["value"]
    if v is None:
        print(f"{name:6} -> account not found")
        continue
    info = v["data"]["parsed"]["info"]
    ma = info.get("mintAuthority")
    fa = info.get("freezeAuthority")
    risky = ma is not None or fa is not None
    print(f"{name:6} mintAuthority={ma} freezeAuthority={fa}  RISKY={risky}")
