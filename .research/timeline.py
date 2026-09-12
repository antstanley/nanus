import json
D="data"
def tl(n, keep=8, major=None):
    j=json.load(open(f"{D}/{n}.json"))
    vs=[v for v in j["versions"] if not v.get("yanked")]
    out=[]
    seen=set()
    for v in vs:
        num=v["num"]
        key=num.split(".")[0] if len(num.split("."))>1 else num
        # keep first release of each major/minor pair
        k=".".join(num.split(".")[:2]).split("+")[0]
        if k in seen: continue
        seen.add(k)
        out.append((num, v.get("created_at","")[:10], v.get("rust_version")))
    print(f"--- {n} (latest {j['crate']['max_stable_version']})")
    for num,d,msrv in out[-keep:]:
        print(f"    {num:<22} {d}  msrv={msrv}")
for n in ["ratatui","reqwest","toml","crossterm","tokio","clap","thiserror","serde","serde_json","tracing","tracing-subscriber","rustls","uuid","similar","diffy","globset","ignore","tempfile","duct","command-group","ulid","directories","etcetera","proptest"]:
    tl(n, keep=6)
