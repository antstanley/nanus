import json,sys
D="data"
for n in sys.argv[1:]:
    j=json.load(open(f"{D}/{n}.json"))
    vs=[v for v in j["versions"] if not v.get("yanked")]
    print(f"--- {n} (max_stable={j['crate']['max_stable_version']})")
    for v in vs[:8]:
        print(f"    {v['num']:<24} {v.get('created_at','')[:10]}  msrv={v.get('rust_version')}")
