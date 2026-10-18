// ===== Tests =====

use super::*;

#[test]
fn group_info_conversion() {
    let g = crate::types::GroupBinding {
        jid: "tg:group:1".into(),
        folder: "team-a".into(),
        name: "Team A".into(),
        channel: "telegram".into(),
        group_type: "group".into(),
        requires_trigger: true,
        allowed_tools: Some(vec!["Read".into(), "Write".into()]),
        allowed_paths: None,
        allowed_work_dirs: Some(vec!["/tmp".into()]),
        bot_token: Some("tok123".into()),
        max_messages: Some(50),
        llm_config_id: Some("llm-1".into()),
        last_active: None,
        added_at: "2026-01-01T00:00:00Z".into(),
        project_id: None,
    };
    let info = wire::to_group_info(&g);
    assert_eq!(info.jid, "tg:group:1");
    assert_eq!(info.folder, "team-a");
    assert_eq!(
        info.allowed_tools.as_deref(),
        Some(&["Read".into(), "Write".into()][..])
    );
    assert_eq!(info.max_messages, Some(50));
}

#[test]
fn gateway_new_defaults() {
    let gw = gateway::WebSocketGateway::new(18789, Some("secret".into()));
    assert_eq!(gw.port, 18789);
    assert_eq!(gw.token.as_deref(), Some("secret"));
}

#[test]
fn gateway_no_token() {
    let gw = gateway::WebSocketGateway::new(18789, None);
    assert_eq!(gw.token, None);
}

// ===== Sessions in a project =====

mod projects {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::projects::{FactsWrite, NewProject, Project};
    use crate::types::GroupBinding;
    use serde_json::json;

    use super::handlers::{check_update_keeps_project, decide_project};

    fn db() -> Db {
        Db::open_in_memory(&Config::from_env()).unwrap()
    }

    fn project(db: &Db, name: &str, root: &str) -> Project {
        db.create_project(
            NewProject {
                name: name.into(),
                root: root.into(),
                description: String::new(),
                facts_write: FactsWrite::Project,
                inherit_common: true,
            },
            1,
        )
        .unwrap()
    }

    fn session(project: Option<&Project>) -> GroupBinding {
        GroupBinding {
            jid: "web:main:1".into(),
            folder: "main".into(),
            name: "Chat".into(),
            channel: String::new(),
            group_type: "chat".into(),
            requires_trigger: false,
            allowed_tools: None,
            allowed_paths: None,
            allowed_work_dirs: None,
            bot_token: None,
            max_messages: None,
            llm_config_id: None,
            last_active: None,
            added_at: "2026-10-18T00:00:00Z".into(),
            project_id: project.map(|p| p.id.clone()),
        }
    }

    fn register(
        db: &Db,
        existing: Option<&GroupBinding>,
        msg: serde_json::Value,
        channel: &str,
    ) -> Result<Option<String>, &'static str> {
        decide_project(db, existing, &msg, channel).map(|p| p.map(|p| p.id))
    }

    #[test]
    fn a_new_session_joins_a_project_only_when_the_request_names_a_usable_one() {
        let db = db();
        let p = project(&db, "Sen", "/work/sen");
        assert_eq!(
            register(&db, None, json!({ "projectId": p.id }), "").unwrap(),
            Some(p.id.clone())
        );
        assert_eq!(
            register(
                &db,
                None,
                json!({ "groupType": "code", "projectId": p.id }),
                ""
            )
            .unwrap(),
            Some(p.id.clone())
        );
        // No mention, null and blank all mean "no project".
        for msg in [
            json!({}),
            json!({ "projectId": null }),
            json!({ "projectId": "  " }),
        ] {
            assert_eq!(register(&db, None, msg, "").unwrap(), None);
        }
        assert_eq!(
            register(&db, None, json!({ "projectId": "prj_00000000" }), "").unwrap_err(),
            "no such project"
        );
        assert_eq!(
            register(&db, None, json!({ "projectId": 7 }), "").unwrap_err(),
            "projectId must be a string"
        );

        db.update_project(
            &p.id,
            crate::projects::ProjectPatch {
                archived: Some(true),
                ..Default::default()
            },
            2,
        )
        .unwrap();
        assert!(register(&db, None, json!({ "projectId": p.id }), "")
            .unwrap_err()
            .contains("archived"));
    }

    #[test]
    fn only_web_only_chat_and_code_sessions_may_join() {
        let db = db();
        let p = project(&db, "Sen", "/work/sen");
        for channel in ["telegram", "feishu", "wechat", "app"] {
            assert!(
                register(&db, None, json!({ "projectId": p.id }), channel)
                    .unwrap_err()
                    .contains("web-only"),
                "{channel}"
            );
        }
        for group_type in ["cowork", "group", "schedule"] {
            assert!(
                register(
                    &db,
                    None,
                    json!({ "projectId": p.id, "groupType": group_type }),
                    ""
                )
                .unwrap_err()
                .contains("chat and code"),
                "{group_type}"
            );
        }
    }

    #[test]
    fn registering_an_existing_session_again_cannot_move_it() {
        let db = db();
        let (a, b) = (project(&db, "A", "/work/a"), project(&db, "B", "/work/b"));
        let inside = session(Some(&a));
        // Leaving the key out, or echoing the same project, keeps it.
        assert_eq!(
            register(&db, Some(&inside), json!({}), "").unwrap(),
            Some(a.id.clone())
        );
        assert_eq!(
            register(&db, Some(&inside), json!({ "projectId": a.id }), "").unwrap(),
            Some(a.id.clone())
        );
        // Another project, or none, is refused.
        for msg in [
            json!({ "projectId": b.id }),
            json!({ "projectId": null }),
            json!({ "projectId": "" }),
        ] {
            assert!(register(&db, Some(&inside), msg, "")
                .unwrap_err()
                .contains("cannot be changed"));
        }
        // A session outside a project cannot be put in one after the fact.
        assert!(
            register(&db, Some(&session(None)), json!({ "projectId": a.id }), "")
                .unwrap_err()
                .contains("cannot be changed")
        );
        assert_eq!(
            register(&db, Some(&session(None)), json!({ "projectId": null }), "").unwrap(),
            None
        );
    }

    #[test]
    fn an_archived_project_keeps_the_sessions_it_has() {
        let db = db();
        let p = project(&db, "Sen", "/work/sen");
        let inside = session(Some(&p));
        db.update_project(
            &p.id,
            crate::projects::ProjectPatch {
                archived: Some(true),
                ..Default::default()
            },
            2,
        )
        .unwrap();
        assert_eq!(
            register(&db, Some(&inside), json!({ "projectId": p.id }), "").unwrap(),
            Some(p.id.clone())
        );
        assert_eq!(
            register(&db, Some(&inside), json!({}), "").unwrap(),
            Some(p.id)
        );
    }

    #[test]
    fn an_update_may_echo_the_project_but_not_change_or_drop_it() {
        let db = db();
        let (a, b) = (project(&db, "A", "/work/a"), project(&db, "B", "/work/b"));
        let inside = session(Some(&a));
        let outside = session(None);
        // Ordinary edits, and a client that sends the whole group back.
        assert!(check_update_keeps_project(
            &inside,
            &json!({ "name": "Renamed", "modelId": "m1" })
        )
        .is_ok());
        assert!(
            check_update_keeps_project(&inside, &json!({ "projectId": a.id, "name": "x" })).is_ok()
        );
        assert!(check_update_keeps_project(&outside, &json!({ "projectId": null })).is_ok());
        // The two things that are refused: another project, and none.
        for msg in [
            json!({ "projectId": b.id }),
            json!({ "projectId": null }),
            json!({ "projectId": "" }),
        ] {
            let why = check_update_keeps_project(&inside, &msg).unwrap_err();
            assert!(why.contains("cannot be changed or removed"), "{msg}: {why}");
        }
        // Joining one afterwards is a change too.
        assert!(check_update_keeps_project(&outside, &json!({ "projectId": a.id })).is_err());
        // A session in a project stays a web-only chat or code session.
        assert!(check_update_keeps_project(&inside, &json!({ "channel": "telegram" })).is_err());
        assert!(check_update_keeps_project(&inside, &json!({ "groupType": "cowork" })).is_err());
        assert!(check_update_keeps_project(&inside, &json!({ "groupType": "code" })).is_ok());
        assert!(
            check_update_keeps_project(&outside, &json!({ "channel": "telegram" })).is_ok(),
            "ordinary sessions are not restricted"
        );
    }

    #[test]
    fn a_groups_wire_shape_carries_the_project_only_when_there_is_one() {
        let db = db();
        let p = project(&db, "Sen", "/work/sen");
        let inside = serde_json::to_value(wire::to_group_info(&session(Some(&p)))).unwrap();
        assert_eq!(inside["projectId"], p.id.as_str());
        let outside = serde_json::to_value(wire::to_group_info(&session(None))).unwrap();
        assert!(outside.get("projectId").is_none(), "{outside}");
    }
}
