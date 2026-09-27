# Sandbox security experiments

Reproduces the measurements in [docs/sandbox-security-experiment.md](../../docs/sandbox-security-experiment.md):
real apps run under sandbox restrictions, with every claim probed rather than
assumed.

Point them at a daemon you can throw away — they create and delete sandboxes,
and one probe deliberately tries to escalate through the daemon's own API:

```bash
HOME=/tmp/sbx-exp-home SENCLAW_UI_PORT=18990 SENCLAW_WS_PORT=18991 ./target/debug/senclaw &
cd scripts/sandbox-experiment && python3 exp1_static_app.py
```

| Script | Subject | Asks |
|---|---|---|
| `exp1_static_app.py` | `python3 -m http.server` | do path + port limits hold while the app serves? |
| `exp2_node_app.py` | `apps/test-manager` (Express + sqlite3) | does a real Space App work read-only vs copied into the workspace? |
| `exp3_network.py` | — | what each network configuration actually permits, including loopback |
| `exp4_one_site.py` | allowlisting CONNECT proxy | can a sandbox be limited to exactly one website? |

`SBX_BASE` overrides the API root (default `http://127.0.0.1:18990/api/sandbox`).

Probes report `expected` vs `measured`, so a regression shows up as a
`MISMATCH` line and a non-zero mismatch count. Detect blocking by **exit code**,
never by grepping stdout for a marker: a Python traceback echoes the failing
source line, so a blocked call looks like a successful one (this harness had
that bug, and it briefly reported three passing restrictions as broken).
