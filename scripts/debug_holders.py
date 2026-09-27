import json
import urllib.request

RPC = "https://mainnet.helius-rpc.com/?api-key=8ad1f1ff-83b9-414f-a5c9-1b10ba12316f"


def rpc(method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(RPC, data=body, headers={"Content-Type": "application/json"})
    try:
        return json.loads(urllib.request.urlopen(req, timeout=45).read())
    except Exception as e:
        return {"transport_error": str(e)}


for name, mint in [
    ("RAY", "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R"),
    ("CAPX", "7AoBuYcGKQYadxc9wmGxpuu29bpC1EDQezkoXACWZRFF"),
]:
    print("=" * 70)
    print(name, mint)
    print("getTokenSupply ->", json.dumps(rpc("getTokenSupply", [mint]))[:400])
    print("getTokenLargestAccounts ->", json.dumps(rpc("getTokenLargestAccounts", [mint]))[:600])
