import json
import sys

path = sys.argv[1] if len(sys.argv) > 1 else "pool_candidates_wide.json"
d = json.load(open(path, encoding="utf-8"))

print("--- HAREKET EDEN HAVUZLAR ---")
for r in d["all_moved"]:
    print(f"{r['api_name']:16} delta={r['delta_bps_20s']:8.2f}bps  {r.get('verdict')}")

print()
print("--- DETAY (likidite / holder) ---")
for r in d["all_moved"]:
    hs = r.get("holder_stats")
    hs_s = (
        f"top1=%{hs['top_pct']:.1f} top20=%{hs['top20_pct']:.1f} n={hs['sampled']}"
        if hs
        else "n/a"
    )
    print(
        f"{r['api_name']:16} sol={r.get('input_vault_sol')} "
        f"auth_in={r.get('authority_in')} auth_out={r.get('authority_out')} {hs_s}"
    )
