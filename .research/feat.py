import json,sys
D="data"
for n in sys.argv[1:]:
    j=json.load(open(f"{D}/{n}.json")); c=j["crate"]
    print("="*90); print(n, c["max_stable_version"], "| repo:", c.get("repository"))
    print("  desc:", (c.get("description") or "").replace("\n"," ")[:200])
    for v in j["versions"]:
        if v["num"]==c["max_stable_version"]:
            print("  MSRV:",v.get("rust_version"),"| published:",v.get("created_at","")[:10])
            for k in sorted(v.get("features") or {}): print("    ",k,"=",(v["features"])[k])
            break
