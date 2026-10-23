use super::*;
use serde_json::{json, Value};

/// What `McpBridgeTool::call` hands the model today: the whole JSON-RPC result,
/// pretty-printed, with the tool's JSON inside a string inside a text block.
fn mcp_envelope(payload: &Value) -> String {
    let inner = serde_json::to_string_pretty(payload).unwrap();
    serde_json::to_string_pretty(&json!({
        "content": [{"type": "text", "text": inner}],
        "isError": false
    }))
    .unwrap()
}

fn email(i: usize) -> Value {
    json!({
        "id": format!("msg-{i:05}"),
        "threadId": 9_000_000_000_000_000_000u64 + i as u64,
        "from": {"name": format!("Sender {}", i % 13), "address": format!("sender{}@example.com", i % 13)},
        "subject": format!("Weekly report number {i}"),
        "snippet": "Hi team, attached is the summary of last week's numbers and the plan for the next sprint. Let me know if anything looks off.",
        "labels": ["INBOX", "CATEGORY_UPDATES"],
        "date": format!("2026-10-{:02}T09:{:02}:00Z", 1 + i % 28, i % 60),
        "unread": i % 3 == 0,
        "sizeBytes": 18_000 + (i * 37) % 900,
    })
}

fn emails(n: usize) -> Value {
    json!({"total": n, "emails": (0..n).map(email).collect::<Vec<_>>(), "nextPageToken": null})
}

fn lossy(outcome: Outcome) -> Lossy {
    match outcome {
        Outcome::Lossy(l) => l,
        Outcome::Lossless(_) => panic!("expected rows to be left out, got a lossless result"),
        Outcome::Unchanged => panic!("expected rows to be left out, got nothing"),
    }
}

#[test]
fn a_long_list_from_an_mcp_tool_shrinks_to_a_fraction_and_names_what_was_left_out() {
    let original = mcp_envelope(&emails(150));
    let l = lossy(compress("mcp__core__email_list", &original));

    assert!(
        l.text.len() * 8 < original.len(),
        "{} -> {}",
        original.len(),
        l.text.len()
    );
    assert_eq!((l.kept, l.total), (15, 150));
    assert!(
        l.text.contains("\"total\":150") && l.text.contains("\"nextPageToken\":null"),
        "the rest of the document stays"
    );
    assert!(
        l.text.contains("msg-00000") && l.text.contains("msg-00149"),
        "the start and the end"
    );
    assert!(l.text.contains("items omitted"));
    assert!(
        !l.text.contains("msg-00075") && !l.text.contains("msg-00100"),
        "the middle is cut"
    );
    assert!(
        json::is_json_container(&l.text),
        "still one valid JSON document: {}",
        &l.text[..200]
    );
}

#[test]
fn rows_that_survive_are_the_tools_own_bytes_ids_included() {
    let l = lossy(compress(
        "mcp__core__email_list",
        &mcp_envelope(&emails(150)),
    ));
    // 9e18 + i is above 2^53: a float round trip would change it.
    assert!(
        l.text.contains("\"threadId\":9000000000000000003"),
        "{}",
        &l.text[..400]
    );
}

#[test]
fn the_saved_file_has_every_row_on_its_own_line_so_grep_finds_one() {
    let l = lossy(compress(
        "mcp__core__email_list",
        &mcp_envelope(&emails(150)),
    ));
    for i in 0..150 {
        let id = format!("\"id\":\"msg-{i:05}\"");
        assert_eq!(
            l.full.lines().filter(|line| line.contains(&id)).count(),
            1,
            "row {i}"
        );
    }
    assert!(l.full.contains("\"total\":150"));
    let inner: Value =
        serde_json::from_str(&l.full).expect("the saved file is the same document, just laid out");
    assert_eq!(inner["emails"].as_array().unwrap().len(), 150);
}

#[test]
fn the_footer_names_the_file_and_what_it_holds() {
    let l = lossy(compress(
        "mcp__core__email_list",
        &mcp_envelope(&emails(150)),
    ));
    let shown = l.with_footer(Path::new("/tmp/ws/artifacts/c1-full"));
    assert!(shown.ends_with("]"));
    assert!(shown.contains("kept 15 of 150 items"));
    assert!(shown.contains("Read or Grep /tmp/ws/artifacts/c1-full"));
    assert!(shown.contains("before answering"));
}

#[test]
fn an_unusual_row_in_the_middle_is_not_among_those_left_out() {
    let mut payload = emails(150);
    payload["emails"][97]["subject"] = json!("Delivery failed: mailbox full");
    payload["emails"][97]["labels"] = json!(["INBOX", "BOUNCE"]);
    let l = lossy(compress("mcp__core__email_list", &mcp_envelope(&payload)));
    assert!(
        l.text.contains("msg-00097"),
        "the row that names a failure stays"
    );
}

#[test]
fn a_short_list_is_only_made_compact() {
    let original = mcp_envelope(&emails(12));
    match compress("mcp__core__email_list", &original) {
        Outcome::Lossless(t) => {
            assert!(
                t.len() * 4 < original.len() * 3,
                "{} -> {}",
                original.len(),
                t.len()
            );
            assert!(
                t.contains("msg-00005") && t.contains("msg-00011"),
                "every row is still there"
            );
            assert!(!t.contains("omitted"));
        }
        _ => panic!("12 rows are kept whole and printed compactly"),
    }
}

#[test]
fn compact_json_means_the_same_data() {
    let payload = emails(12);
    let original = mcp_envelope(&payload);
    let Outcome::Lossless(t) = compress("mcp__core__email_list", &original) else {
        panic!("expected a lossless result");
    };
    assert_eq!(serde_json::from_str::<Value>(&t).unwrap(), payload);
}

#[test]
fn an_mcp_text_answer_loses_its_envelope_and_keeps_its_words() {
    // Printed as JSON, every quote and line break of the text costs an escape.
    let words: String = (0..100).map(|i| format!("Row {i}: \"ok\"\n")).collect();
    let original = serde_json::to_string_pretty(&json!({
        "content": [{"type": "text", "text": words}],
        "isError": false
    }))
    .unwrap();
    let Outcome::Lossless(t) = compress("mcp__core__wiki_read", &original) else {
        panic!("the envelope around plain text should go");
    };
    assert_eq!(t, words.trim());
}

#[test]
fn an_error_answer_or_one_with_an_image_is_never_touched() {
    let big = "x".repeat(3000);
    let error = serde_json::to_string_pretty(
        &json!({"content": [{"type": "text", "text": big}], "isError": true}),
    )
    .unwrap();
    assert!(matches!(
        compress("mcp__core__anything", &error),
        Outcome::Unchanged
    ));
    let image = serde_json::to_string_pretty(&json!({"content": [
        {"type": "text", "text": big}, {"type": "image", "data": "AAAA", "mimeType": "image/png"}]}))
    .unwrap();
    assert!(matches!(
        compress("mcp__core__anything", &image),
        Outcome::Unchanged
    ));
}

#[test]
fn tools_whose_output_is_edited_against_are_never_touched() {
    let original = mcp_envelope(&emails(150));
    for tool in [
        "Read",
        "Edit",
        "Write",
        "NotebookEdit",
        "Grep",
        "Glob",
        "Skill",
        "Task",
        "ToolSearch",
        "AskUserQuestion",
        // A page snapshot lists elements to click by index.
        "mcp__core__browser_observe",
        "mcp__mini-browser-mcp__browser_navigate",
    ] {
        assert!(
            matches!(compress(tool, &original), Outcome::Unchanged),
            "{tool}"
        );
        assert!(!eligible(tool), "{tool}");
    }
}

#[test]
fn shell_output_is_never_reformatted_as_json() {
    // `cat package.json` then an Edit that quotes the file: it must stay a file.
    let pretty = serde_json::to_string_pretty(&emails(150)).unwrap();
    assert!(matches!(compress("Bash", &pretty), Outcome::Unchanged));
}

#[test]
fn shell_output_that_is_a_long_log_is_shortened_and_whole_text_is_kept_for_the_file() {
    let log: String = (0..1500)
        .map(|i| {
            if i == 800 {
                "2026-10-23 10:00:00 ERROR payment 800 failed: card declined\n".to_string()
            } else {
                format!("2026-10-23 10:00:00 INFO payment {i} processed in 12ms\n")
            }
        })
        .collect();
    let l = lossy(compress("Bash", &log));
    assert!(l.text.contains("payment 800 failed: card declined"));
    assert_eq!(l.full, log, "the file is the log as the command printed it");
    assert!(l.lossless.is_none());
    assert_eq!(l.unit, "lines");
}

#[test]
fn an_array_nested_in_an_object_is_cut_and_its_siblings_stay() {
    let l = lossy(compress(
        "mcp__core__email_list",
        &mcp_envelope(&emails(150)),
    ));
    let doc: Value = serde_json::from_str(&l.text).unwrap();
    assert_eq!(doc["total"], 150);
    let rows = doc["emails"].as_array().unwrap();
    assert!(
        rows.len() > 15,
        "rows plus the omission markers: {}",
        rows.len()
    );
    assert!(rows
        .iter()
        .any(|r| r.as_str().is_some_and(|s| s.contains("items omitted"))));
}

#[test]
fn a_big_object_with_no_long_array_is_only_made_compact() {
    let settings: serde_json::Map<String, Value> = (0..60)
        .map(|i| {
            (
                format!("key_{i}"),
                json!({"value": i, "label": "A fairly long label to make this worth looking at"}),
            )
        })
        .collect();
    let object = json!({ "settings": settings });
    let original = mcp_envelope(&object);
    assert!(matches!(
        compress("mcp__core__config", &original),
        Outcome::Lossless(_)
    ));
}

#[test]
fn small_results_and_unparseable_ones_are_left_alone() {
    assert!(matches!(
        compress("mcp__core__x", "{\"ok\":true}"),
        Outcome::Unchanged
    ));
    let broken = format!("{{\"a\": [{}", "1, ".repeat(2000));
    assert!(matches!(
        compress("mcp__core__x", &broken),
        Outcome::Unchanged
    ));
}

#[test]
fn a_document_that_barely_shrinks_is_not_cut() {
    // 25 rows of 4 KB each, all different and all ordinary: keeping 15 of 25
    // saves under half of the array, so nothing is left out.
    let rows: Vec<Value> = (0..25)
        .map(|i| json!({"id": i, "blob": format!("{i}-{}", "z".repeat(4000))}))
        .collect();
    assert!(matches!(
        compress(
            "mcp__core__x",
            &serde_json::to_string(&Value::Array(rows)).unwrap()
        ),
        Outcome::Unchanged | Outcome::Lossless(_)
    ));
}

#[test]
fn the_same_input_always_gives_the_same_output() {
    // The history keeps this text and every later request repeats it: any
    // variation between runs would break a provider's prefix cache.
    let original = mcp_envelope(&emails(150));
    let a = lossy(compress("mcp__core__email_list", &original));
    let b = lossy(compress("mcp__core__email_list", &original));
    assert_eq!((a.text, a.full), (b.text, b.full));
}

mod apply_tests {
    use super::*;
    use crate::control_plane::env_test_guard;

    struct Scratch {
        _guard: std::sync::MutexGuard<'static, ()>,
        config: std::path::PathBuf,
        workspace: tempfile::TempDir,
        _config_dir: tempfile::TempDir,
    }

    fn scratch() -> Scratch {
        let guard = env_test_guard();
        let config_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let config = config_dir.path().join("config.json");
        std::env::set_var("SENCLAW_CONFIG_PATH", &config);
        std::env::set_var("SENCLAW_CONTROL_PLANE_WORKSPACE_DIR", workspace.path());
        Scratch {
            _guard: guard,
            config,
            workspace,
            _config_dir: config_dir,
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::env::remove_var("SENCLAW_CONFIG_PATH");
            std::env::remove_var("SENCLAW_CONTROL_PLANE_WORKSPACE_DIR");
        }
    }

    #[test]
    fn it_works_with_no_settings_file_because_the_default_is_on() {
        let s = scratch();
        let original = mcp_envelope(&emails(150));
        let shown = apply(
            "mcp__core__email_list",
            "call-1",
            "chat-a",
            original.clone(),
        );
        assert!(shown.len() * 8 < original.len());
        let path_line = shown.lines().last().unwrap();
        assert!(path_line.starts_with("[context-compress:"), "{path_line}");
        // The path in the footer is a real file with every row in it.
        let at = path_line
            .split("Read or Grep ")
            .nth(1)
            .unwrap()
            .split(" (the full ")
            .next()
            .unwrap();
        let saved = std::fs::read_to_string(at).unwrap_or_else(|e| panic!("{at}: {e}"));
        assert!(saved.contains("msg-00075") && saved.contains("msg-00149"));
        assert!(at.starts_with(s.workspace.path().to_str().unwrap()));
    }

    #[test]
    fn switched_off_it_changes_nothing() {
        let s = scratch();
        crate::gateway::group_manager::save_context_compression_enabled(&s.config, false).unwrap();
        let original = mcp_envelope(&emails(150));
        assert_eq!(
            apply(
                "mcp__core__email_list",
                "call-2",
                "chat-a",
                original.clone()
            ),
            original
        );
        assert!(
            std::fs::read_dir(s.workspace.path())
                .unwrap()
                .next()
                .is_none(),
            "and writes nothing"
        );
    }

    #[test]
    fn switching_it_back_on_takes_effect_on_the_next_result() {
        let s = scratch();
        let original = mcp_envelope(&emails(150));
        crate::gateway::group_manager::save_context_compression_enabled(&s.config, false).unwrap();
        assert_eq!(
            apply("mcp__core__email_list", "c1", "chat-a", original.clone()),
            original
        );
        crate::gateway::group_manager::save_context_compression_enabled(&s.config, true).unwrap();
        assert!(
            apply("mcp__core__email_list", "c2", "chat-a", original.clone()).len() < original.len()
        );
    }

    #[test]
    fn when_the_full_text_cannot_be_saved_nothing_is_left_out() {
        let s = scratch();
        // A file where the workspace folder should be: every write fails.
        let blocker = s.workspace.path().join("blocked");
        std::fs::write(&blocker, "x").unwrap();
        std::env::set_var("SENCLAW_CONTROL_PLANE_WORKSPACE_DIR", &blocker);
        let original = mcp_envelope(&emails(150));
        let shown = apply(
            "mcp__core__email_list",
            "call-3",
            "chat-a",
            original.clone(),
        );
        assert!(shown.len() < original.len(), "still printed compactly");
        assert!(
            !shown.contains("omitted") && !shown.contains("context-compress"),
            "no cut without a way back"
        );
        assert_eq!(serde_json::from_str::<Value>(&shown).unwrap(), emails(150));
    }

    #[test]
    fn a_log_cannot_fall_back_so_it_is_returned_whole_when_the_file_is_not_written() {
        let s = scratch();
        let blocker = s.workspace.path().join("blocked");
        std::fs::write(&blocker, "x").unwrap();
        std::env::set_var("SENCLAW_CONTROL_PLANE_WORKSPACE_DIR", &blocker);
        let log: String = (0..1500)
            .map(|i| format!("2026-10-23 10:00:00 INFO job {i} done\n"))
            .collect();
        assert_eq!(apply("Bash", "call-4", "chat-a", log.clone()), log);
    }

    #[test]
    fn a_tool_that_is_not_eligible_never_reads_the_settings() {
        let _s = scratch();
        // No config file exists; an ineligible tool must return before looking.
        let text = "y".repeat(5000);
        assert_eq!(apply("Read", "call-5", "chat-a", text.clone()), text);
    }
}

/// Run with `--nocapture` for the table quoted in the research report. The
/// assertions are floors, not targets: they fail if a change makes the
/// compressor stop doing its job on shapes real tools return.
#[test]
fn measured_on_the_shapes_tools_really_return() {
    let kanban: Vec<Value> = (0..400)
        .map(|i| {
            let column = ["Backlog", "Ready", "Doing", "Done"][i % 4];
            json!({
                "id": format!("card-{i}"), "title": format!("Task {i}: update the onboarding checklist"),
                "column": column, "assignee": format!("user{}", i % 9),
                "labels": ["ops", "q4"], "createdAt": format!("2026-09-{:02}T08:00:00Z", 1 + i % 28),
                "description": "Walk through the checklist with the new hire, confirm accounts exist, and file anything missing.",
            })
        })
        .collect();
    let service_log: String = (0..3000)
        .map(|i| {
            if i % 900 == 450 {
                format!("2026-10-23 11:{:02}:{:02} ERROR upstream {i} timed out after 30000ms\n    at proxy.forward (proxy.js:88:13)\n", i / 60 % 60, i % 60)
            } else {
                format!("2026-10-23 11:{:02}:{:02} INFO GET /api/items/{i} 200 {}ms\n", i / 60 % 60, i % 60, 8 + i % 40)
            }
        })
        .collect();
    let mut test_run = String::from("running 900 tests\n");
    for i in 0..900 {
        test_run.push_str(&if i == 421 {
            "test billing::refund_rounds_up ... FAILED\n".to_string()
        } else {
            format!("test module_{}::case_{i} ... ok\n", i % 20)
        });
    }
    test_run.push_str("\nfailures:\n    billing::refund_rounds_up\n\ntest result: FAILED. 899 passed; 1 failed; 0 ignored\n");

    let cases: Vec<(&str, &str, String, f64)> = vec![
        (
            "emails ×150 (MCP, nested array)",
            "mcp__core__email_list",
            mcp_envelope(&emails(150)),
            0.85,
        ),
        (
            "emails ×12 (MCP, under the cut limit)",
            "mcp__core__email_list",
            mcp_envelope(&emails(12)),
            0.15,
        ),
        (
            "kanban cards ×400 (MCP, bare array)",
            "mcp__kanban__kanban_list",
            mcp_envelope(&Value::Array(kanban)),
            0.85,
        ),
        ("service log ×3000 lines (Bash)", "Bash", service_log, 0.9),
        ("cargo test output ×900 (Bash)", "Bash", test_run, 0.9),
    ];
    println!(
        "\n{:<44} {:>9} {:>9} {:>7}",
        "result", "before", "after", "saved"
    );
    for (name, tool, text, floor) in cases {
        let after = match compress(tool, &text) {
            Outcome::Unchanged => text.len(),
            Outcome::Lossless(t) => t.len(),
            // The footer is ~180 bytes; count it.
            Outcome::Lossy(l) => l.with_footer(Path::new("/Users/someone/.senclaw/control-plane/workspace/chat_x/artifacts/call_0123456789-full")).len(),
        };
        let saved = 1.0 - after as f64 / text.len() as f64;
        println!(
            "{name:<44} {:>9} {after:>9} {:>6.1}%",
            text.len(),
            saved * 100.0
        );
        assert!(saved >= floor, "{name}: saved only {:.1}%", saved * 100.0);
    }
}
