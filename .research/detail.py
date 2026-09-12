import json, glob, sys
D="data"
def load(n):
    return json.load(open(f"{D}/{n}.json"))
for n in ["ratatui","reqwest","tokio","crossterm","toml","thiserror","rustls"]:
    j=load(n); c=j["crate"]
    print("="*100); print(n, c["max_stable_version"], "| repo:", c.get("repository"), "| docs:", c.get("documentation"))
    print("  desc:", (c.get("description") or "").replace("\n"," ")[:300])
    for v in j["versions"]:
        if v["num"]==c["max_stable_version"]:
            print("  MSRV:", v.get("rust_version"), "| published:", v.get("created_at","")[:10], "| size:", v.get("crate_size"))
            feats=v.get("features") or {}
            print(f"  FEATURES ({len(feats)}):")
            for k in sorted(feats): print("    ",k,"=",feats[k])
            break
