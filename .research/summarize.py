import json, os, glob
D="data"
rows=[]
for f in sorted(glob.glob(D+"/*.json")):
    try: j=json.load(open(f))
    except Exception as e: print("ERR",f,e); continue
    c=j.get("crate",{})
    vers=j.get("versions",[])
    name=c.get("name")
    rows.append(dict(
      name=name,
      max_stable=c.get("max_stable_version"),
      max_ver=c.get("max_version"),
      newest=c.get("newest_version"),
      downloads=c.get("downloads"),
      recent=c.get("recent_downloads"),
      updated=c.get("updated_at","")[:10],
      created=c.get("created_at","")[:10],
      homepage=(c.get("homepage") or "")[:60],
      repo=(c.get("repository") or "")[:70],
      desc=(c.get("description") or "").replace("\n"," ")[:95],
      rust_versions={v["num"]:v.get("rust_version") for v in vers[:6]},
      n_yanked=sum(1 for v in vers if v.get("yanked")),
      n_vers=len(vers),
    ))
w=lambda s,n: str(s).ljust(n)[:n]
print(w("crate",20),w("max_stable",12),w("newest",12),w("updated",11),w("MSRV(latest)",13),w("vers",5),w("dl",11))
print("-"*95)
for r in rows:
    msrv=r["rust_versions"].get(r["max_stable"]) if r["max_stable"] else None
    print(w(r["name"],20),w(r["max_stable"],12),w(r["newest"],12),w(r["updated"],11),w(msrv,13),w(r["n_vers"],5),w(f"{r['downloads']:,}",11))
