#!/bin/zsh
# P0 (e): delete every mem_facts row of one bank; what follows? usage: exp-e.sh <db> <ON|OFF>
DB=$1; FK=$2
cnt() { sqlite3 "$DB" "
select 'facts_total', count(*) from mem_facts;
select 'facts[shared]', count(*) from mem_facts where bank='shared';
select 'facts[dshare]', count(*) from mem_facts where bank='dshare';
select 'entities_total', count(*) from mem_fact_entities;
select 'entities[shared]', count(*) from mem_fact_entities e join mem_facts f on f.id=e.fact_id where f.bank='shared';
select 'entities[dshare]', count(*) from mem_fact_entities e join mem_facts f on f.id=e.fact_id where f.bank='dshare';
select 'links_total', count(*) from mem_links;
select 'links[dshare]', count(*) from mem_links l join mem_facts f on f.id=l.from_id where f.bank='dshare';
select 'fts_total', count(*) from mem_facts_fts;
select 'fts[dshare]', count(*) from mem_facts_fts x join mem_facts f on f.id=x.fact_id where f.bank='dshare';
select 'ORPHAN entities', count(*) from mem_fact_entities where fact_id not in (select id from mem_facts);
select 'ORPHAN links', count(*) from mem_links where from_id not in (select id from mem_facts) or to_id not in (select id from mem_facts);
select 'ORPHAN fts', count(*) from mem_facts_fts where fact_id not in (select id from mem_facts);
"; }
echo "=== BEFORE (foreign_keys will be $FK)"; cnt | tr '\n' ' '; echo
sqlite3 "$DB" "PRAGMA foreign_keys=$FK; select 'PRAGMA foreign_keys ->', foreign_keys from pragma_foreign_keys; delete from mem_facts where bank='shared'; select 'deleted rows', changes();"
echo "=== AFTER"; cnt | tr '\n' ' '; echo
