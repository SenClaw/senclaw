# Pattern library — provenance

`system.md` / `user.md` under this directory come from two places.

## Fabric (255 patterns) + `../strategies` (9 files)

Vendored from [danielmiessler/fabric](https://github.com/danielmiessler/fabric)
at tag **v1.4.470**, unmodified. Licensed **MIT** — see
[`LICENSE-fabric.txt`](LICENSE-fabric.txt).

They are vendored rather than cloned at runtime so a fresh SenClaw install has
a working pattern library offline, on the first launch, with no network and no
repository that has to still exist. The trade is that they go stale: the
**Fabric** entry in Plugins → Patterns → *Thêm nguồn* registers the upstream
repository as a git source, which then shadows nothing and updates on its own
schedule.

**Do not hand-edit these files.** Re-vendoring replaces them wholesale. To
change one, save a copy under the same name into the `user` source — it is
resolved first and survives every refresh (see
[docs/zen-patterns.md](../../docs/zen-patterns.md)).

## SenClaw (6 patterns)

`tom_tat`, `trich_y_chinh`, `viet_lai_gon`, `phan_tich_log`, `bien_ban_hop`,
`soan_email` are written for SenClaw and are Vietnamese-first: Fabric's library
is excellent and entirely in English, and the everyday jobs deserve prompts
that do not need the language overlay to produce a usable answer.

## Re-vendoring

```bash
git clone --depth 1 --branch <tag> https://github.com/danielmiessler/fabric /tmp/fabric
cp -R /tmp/fabric/data/patterns/*/ assets/patterns/
cp /tmp/fabric/data/strategies/*.json assets/strategies/
cp /tmp/fabric/LICENSE assets/patterns/LICENSE-fabric.txt
```

Then bump the tag recorded here and in `src/patterns/catalog.rs`.
