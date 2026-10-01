#!/usr/bin/env python3
"""Acceptance metric for the SenBrowser v2 loop: prints the number of passing checks.

Each check (fixed in plan.md) maps to test names that must ALL pass. Components run
independently (`--only runtime|daemon|extension|e2e`); results are cached so a loop
iteration re-runs only what it touched and still reports the whole total.

    python3 acceptance.py                 # every component
    python3 acceptance.py --only runtime  # re-run one, total from cache
    python3 acceptance.py --report        # per-check table
"""
import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = Path("/Users/benji/Projects/SenClaw")
RUNTIME = ROOT / "sen-browser"
DAEMON = ROOT / ".worktrees/senclaw-browser"
EXTENSION = ROOT / "senclaw-extension"
CACHE = HERE / "acceptance-cache.json"

CHECKS = {
    # Runtime (sen-browser)
    "R1": ("runtime", ["serves_health_auth_info_status_and_scripts"]),
    "R2": ("runtime", ["observes_in_an_isolated_world_and_acts_with_trusted_input"]),
    "R3": ("runtime", ["observes_in_an_isolated_world_and_acts_with_trusted_input", "observation::tests::freshness_evidence_stays_inside_the_runtime"]),
    "R4": ("runtime", ["observes_in_an_isolated_world_and_acts_with_trusted_input"]),
    "R5": ("runtime", ["observes_in_an_isolated_world_and_acts_with_trusted_input"]),
    "R6": ("runtime", ["observes_in_an_isolated_world_and_acts_with_trusted_input"]),
    "R7": ("runtime", ["stale_and_covered_targets_are_refused_before_any_input"]),
    "R8": ("runtime", ["stale_and_covered_targets_are_refused_before_any_input"]),
    "R9": ("runtime", ["combobox_suggestions_and_dialogs"]),
    "R10": ("runtime", ["observes_in_an_isolated_world_and_acts_with_trusted_input"]),
    "R11": ("runtime", ["observes_in_an_isolated_world_and_acts_with_trusted_input"]),
    "R12": ("runtime", ["allowlist::tests::cookies_storage_and_arbitrary_js_are_refused",
                        "allowlist::tests::only_approved_scripts_run_and_only_in_a_context",
                        "scripts::tests::allowlist_accepts_exact_scripts_only"]),
    "R13": ("runtime", ["the_extension_driver_observes_and_acts_through_the_relay",
                        "an_extension_built_for_other_scripts_is_rejected",
                        "relay::tests::answers_resolve_requests_and_errors_map_to_blocked"]),
    "R14": ("runtime", ["navigate_read_screenshot_back_and_handover_gate",
                        "the_extension_driver_observes_and_acts_through_the_relay"]),
    "R15": ("runtime", ["navigate_read_screenshot_back_and_handover_gate"]),
    # Daemon (senclaw worktree)
    "D1": ("daemon", ["manifest::tests::browser_slot_round_trips"]),
    "D2": ("daemon", ["browser_agent::encoder::tests::jev_request_matches_upstream_choose"]),
    "D3": ("daemon", ["browser_agent::decide::tests::invalid_choices_are_rejected"]),
    "D4": ("daemon", ["browser_agent::encoder::tests::laya_v3_matches_upstream_format"]),
    "D5": ("daemon", ["browser_agent::budget::tests::budget_keeps_priority_candidates_within_limit"]),
    "D6": ("daemon", ["browser_agent::decide::tests::bands_use_joint_confidence"]),
    "D7": ("daemon", ["browser_agent::llm::tests::text_value_contract"]),
    "D8": ("daemon", ["browser_agent::run::tests::loop_completes_with_verified_done",
                      "browser_agent::run::tests::loop_stops_without_progress"]),
    "D9": ("daemon", ["browser_agent::policy::tests::risk_tiers"]),
    "D10": ("daemon", ["browser_agent::policy::tests::pii_is_redacted_for_hosted"]),
    "D11": ("daemon", ["browser_agent::policy::tests::driver_selection"]),
    "D12": ("daemon", ["browser_agent::extension::tests::origin_and_pairing"]),
    "D13": ("daemon", ["mcp::browser_agent_server::tests::tools_are_registered"]),
    "D14": ("daemon", ["browser_agent::rest::tests::task_endpoint_runs_the_loop"]),
    # Extension (senclaw-extension)
    "X1": ("extension", ["build"]),
    "X2": ("extension", ["tests/allowlist.test.ts"]),
    "X3": ("extension", ["tests/relay.test.ts"]),
    "X4": ("extension", ["tests/connection.test.ts"]),
    "X5": ("extension", ["tests/manifest.test.ts"]),
    # End to end
    "E1": ("e2e", ["E1"]),
    "E2": ("e2e", ["E2"]),
    # Security review fixes (reports/review-browser-engine-security.md)
    "S1": ("runtime", ["allowlist::tests::parameters_that_carry_code_or_reach_further_are_refused"]),
    "S2": ("daemon", ["browser_agent::policy::tests::final_purchase_labels_need_the_person",
                      "browser_agent::policy::tests::enter_is_judged_by_what_it_submits",
                      "browser_agent::policy::tests::risk_tiers"]),
    "S3": ("daemon", ["browser_agent::run::tests::a_decision_model_can_add_a_pause_but_never_remove_one"]),
    "S4": ("daemon", ["browser_agent::rest::tests::task_endpoint_runs_the_loop"]),
    "S5": ("daemon", ["zen_core::permissions::tests::skipping_prompts_never_releases_a_browser_approval"]),
    "S6": ("daemon", ["browser_agent::extension::tests::a_code_from_a_closed_socket_installs_nothing",
                      "browser_agent::extension::tests::shares_survive_until_the_person_takes_the_tab_back",
                      "browser_agent::extension::tests::only_the_pipe_that_ended_is_cleared",
                      "browser_agent::extension::tests::an_idle_pipe_stays_open_while_a_task_needs_it"]),
    "S7": ("daemon", ["runtime::proxy::tests::the_browser_namespace_is_stripped_for_its_runtime"]),
    "S8": ("daemon", ["browser_agent::run::tests::an_llm_only_loop_on_a_page_that_refuses_input_stops"]),
    "S9": ("runtime", ["relay::tests::notices_become_tab_events_and_close_fails_waiters"]),
    "E3": ("e2e", ["E3"]),
    # Second verification (reports/review-browser-engine-security-fixes.md) and runtime management
    "S10": ("daemon", ["gateway::ui_server::auth::tests::local_trust_needs_a_local_host_and_no_foreign_page"]),
    "S11": ("runtime", ["allowlist::tests::keys_and_clicks_that_paste_are_refused", "allowlist::tests::the_runtimes_own_input_passes"]),
    "S12": ("daemon", ["browser_agent::rest::tests::a_redirect_to_senclaw_itself_is_not_handed_out",
                       "browser_agent::rest::tests::a_chat_cannot_answer_another_chats_approval"]),
    "U1": ("daemon", ["runtime::index::tests::bundled_index_lists_the_browser_runtime",
                      "runtime::manager::tests::log_lines_lose_their_colour_codes"]),
    # Action latency (reports/research-260930-2059-jev-ultrafast-action-latency.md)
    "L1": ("runtime", ["a_page_is_observed_when_it_is_usable",
                       "tab::tests::the_page_being_left_does_not_answer_for_the_new_one",
                       "tab::tests::an_app_shell_is_a_sparse_page_that_loads_scripts"]),
    "L2": ("runtime", ["a_click_that_navigates_returns_the_new_page",
                       "tab::tests::a_clicked_link_is_followed_to_the_document_it_brings",
                       "tab::tests::the_history_api_is_not_a_new_document",
                       "tab::tests::a_restored_or_cancelled_navigation_has_nothing_to_wait_for",
                       "tab::tests::a_link_that_opens_elsewhere_is_not_a_navigation_of_this_page"]),
    "L3": ("runtime", ["a_toggle_button_says_whether_it_is_on"]),
    "L4": ("daemon", ["browser_agent::run::tests::criteria_are_written_while_the_task_runs",
                      "browser_agent::run::tests::an_unsure_done_is_checked_before_an_llm_is_asked",
                      "browser_agent::run::tests::decision_checkpoints_load_while_the_page_opens",
                      "browser_agent::llm::tests::a_verdict_is_needed_for_every_criterion"]),
    "L5": ("daemon", ["browser_agent::run::tests::a_missing_decision_model_is_named_in_the_outcome",
                      "browser_agent::run::tests::the_outcome_accounts_for_its_time",
                      "browser_agent::rest::tests::settings_say_whether_the_decision_model_is_installed",
                      "browser_agent::settings::tests::a_decision_checkpoint_counts_once_its_install_is_complete"]),
    "L6": ("daemon", ["browser_agent::rest::tests::read_opens_the_page_it_is_given"]),
    "L7": ("daemon", ["mcp::browser_agent_server::tests::the_engines_skills_name_only_its_tools",
                      "skills::scan::tests::a_skill_is_read_in_the_variant_the_install_needs"]),
    "L8": ("daemon", ["browser_agent::run::tests::a_toggle_that_already_satisfies_the_goal_is_not_clicked_off"]),
    "E4": ("e2e", ["E4"]),
}


def run(cmd, cwd, env=None, timeout=1800):
    # No incremental cache: the disk is nearly full and each worktree would keep its own.
    env = {**os.environ, "CARGO_INCREMENTAL": "0", **(env or {})}
    try:
        p = subprocess.run(cmd, cwd=cwd, shell=True, capture_output=True, text=True, env=env, timeout=timeout)
        return p.returncode, p.stdout + p.stderr
    except subprocess.TimeoutExpired as e:
        return 124, (e.stdout or "") + (e.stderr or "") if isinstance(e.stdout, str) else "timeout"


def cargo_passed(output):
    return {m.group(1) for m in re.finditer(r"^test (\S+) \.\.\. ok$", output, re.M)}


def component_runtime():
    code, out = run("cargo test 2>&1", RUNTIME)
    return sorted(cargo_passed(out))


def component_daemon():
    passed = set()
    code, out = run("cargo test --manifest-path crates/sen-runtime-sdk/Cargo.toml 2>&1", DAEMON)
    passed |= cargo_passed(out)
    code, out = run("cargo test --lib -- browser_agent mcp::browser_agent_server zen_core::permissions runtime:: gateway::ui_server::auth skills::scan 2>&1", DAEMON)
    passed |= cargo_passed(out)
    return sorted(passed)


def component_extension():
    passed = []
    code, out = run("npm run build 2>&1", EXTENSION)
    if code == 0:
        passed.append("build")
    code, out = run("npx vitest run --reporter=json --outputFile=.vitest-results.json 2>&1", EXTENSION)
    results = EXTENSION / ".vitest-results.json"
    if results.exists():
        data = json.loads(results.read_text())
        for f in data.get("testResults", []):
            name = os.path.relpath(f.get("name", ""), EXTENSION)
            ok = f.get("status") == "passed" and all(a.get("status") == "passed" for a in f.get("assertionResults", []))
            if ok and f.get("assertionResults"):
                passed.append(name)
        results.unlink()
    return passed


def component_e2e():
    passed = []
    for check, script in (("E1", "e2e/managed.sh"), ("E2", "e2e/extension.sh"), ("E3", "e2e/approval.sh"), ("E4", "e2e/feed.sh")):
        path = HERE / script
        if path.exists():
            code, out = run(f"bash {path} 2>&1", HERE, timeout=1200)
            if code == 0 and f"{check} PASS" in out:
                passed.append(check)
    return passed


COMPONENTS = {"runtime": component_runtime, "daemon": component_daemon, "extension": component_extension, "e2e": component_e2e}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", choices=COMPONENTS)
    ap.add_argument("--report", action="store_true")
    args = ap.parse_args()
    cache = json.loads(CACHE.read_text()) if CACHE.exists() else {}
    for name in ([args.only] if args.only else COMPONENTS):
        cache[name] = COMPONENTS[name]()
    CACHE.write_text(json.dumps(cache, indent=1))
    ok = {c for c, (comp, needs) in CHECKS.items() if all(n in cache.get(comp, []) for n in needs)}
    if args.report:
        for c, (comp, _) in CHECKS.items():
            print(f"{c:4} {comp:9} {'PASS' if c in ok else '----'}", file=sys.stderr)
    print(len(ok))


if __name__ == "__main__":
    main()
