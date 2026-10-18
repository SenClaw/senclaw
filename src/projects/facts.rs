//! Where the facts of a session in a project are written, and which bank its
//! recall reads besides the agent's.
//!
//! Pure functions of the project row, so the agent pool, the reflection
//! windows and the tests all arrive at the same answer.

use crate::memory::engine::facts::FactTarget;
use crate::memory::engine::{ProjectRecall, RecallQuery};

use super::session::project_bank;
use super::types::{FactsWrite, Project};

/// Where facts retained from this session's conversation go, or `None` when
/// nothing is to be retained.
///
/// | session | `facts_write` | target |
/// |---|---|---|
/// | outside a project | — | the profile's bank, tagged with the chat |
/// | in a project | `project` | the project's bank, no chat |
/// | in a project | `chat` | the profile's bank, tagged with the chat |
/// | in a project | `off` | nowhere |
///
/// `project` is the project the session was created in: `None` for a session
/// outside one and for one whose project no longer exists, which then behaves
/// like any other chat.
pub fn fact_target(
    project: Option<&Project>,
    folder: &str,
    chat_jid: &str,
    by_chat: bool,
) -> Option<FactTarget> {
    let chat = || FactTarget::chat(folder, chat_jid, by_chat);
    match project.map(|p| (p, p.facts_write)) {
        None | Some((_, FactsWrite::Chat)) => Some(chat()),
        Some((p, FactsWrite::Project)) => Some(FactTarget::project(&project_bank(&p.id))),
        Some((_, FactsWrite::Off)) => None,
    }
}

/// What recall reads for this session besides the agent's own facts. Whatever
/// `facts_write` says: it decides where conversations are written, not what a
/// project's chats are allowed to know.
pub fn recall_scope(project: Option<&Project>) -> Option<ProjectRecall> {
    project.map(|p| ProjectRecall {
        bank: project_bank(&p.id),
        include_common: p.inherit_common,
    })
}

/// Where what a kanban worker finished is kept, or `None` when nothing is.
///
/// A worker has no chat to tag a fact with and no profile whose bank could
/// hold one, so the project's own bank is the only place there is — and only
/// for a project that keeps its conversations' facts there. `chat` and `off`
/// both say the person wants nothing kept in the project's bank.
pub fn worker_fact_target(project: &Project) -> Option<FactTarget> {
    match project.facts_write {
        FactsWrite::Project => fact_target(Some(project), "", "", false),
        FactsWrite::Chat | FactsWrite::Off => None,
    }
}

/// What a kanban worker's recall reads: the project's bank and nothing else.
/// A worker has no profile, so the agent-wide facts are not its to read
/// whatever the project's `inherit_common` says.
pub fn worker_recall_scope(project_id: &str) -> ProjectRecall {
    ProjectRecall {
        bank: project_bank(project_id),
        include_common: false,
    }
}

/// The question a worker about to start a card asks of the project's facts.
/// No folder and no chat: the four arms over the agent's bank have nothing to
/// look at, and the four over the project's bank are the ones that answer.
pub fn worker_recall_query(project_id: &str, text: &str, limit: usize, now: i64) -> RecallQuery {
    RecallQuery {
        text: text.to_string(),
        folder: String::new(),
        chat_jid: String::new(),
        facts_by_chat: false,
        limit,
        now,
        project: Some(worker_recall_scope(project_id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(facts_write: FactsWrite, inherit_common: bool) -> Project {
        Project {
            id: "prj_0a1b2c3d".into(),
            name: "Alpha".into(),
            root: "/tmp/alpha".into(),
            description: String::new(),
            facts_write,
            inherit_common,
            created_at: 1,
            updated_at: 1,
            archived_at: None,
        }
    }

    #[test]
    fn outside_a_project_facts_are_the_chats_own_in_the_profiles_bank() {
        let t = fact_target(None, "main", "web:main:1", true).unwrap();
        assert_eq!((t.bank.as_str(), t.origin.as_str()), ("main", "web:main:1"));
        // The agent-wide switch keeps one pool for all chats.
        let t = fact_target(None, "main", "web:main:1", false).unwrap();
        assert_eq!((t.bank.as_str(), t.origin.as_str()), ("main", ""));
        assert_eq!(recall_scope(None), None);
    }

    #[test]
    fn facts_write_project_puts_every_chat_of_the_project_in_one_bank() {
        let p = project(FactsWrite::Project, true);
        let a = fact_target(Some(&p), "main", "web:main:1", true).unwrap();
        let b = fact_target(Some(&p), "main", "web:main:2", true).unwrap();
        assert_eq!(a, b, "two chats of one project write to the same place");
        assert_eq!(
            (a.bank.as_str(), a.origin.as_str()),
            ("project:prj_0a1b2c3d", "")
        );
        // And not the profile's bank, whichever profile the chat uses.
        assert_ne!(a.bank, "main");
    }

    #[test]
    fn facts_write_chat_is_as_if_the_chat_were_outside_the_project() {
        let p = project(FactsWrite::Chat, true);
        let t = fact_target(Some(&p), "main", "web:main:1", true).unwrap();
        assert_eq!((t.bank.as_str(), t.origin.as_str()), ("main", "web:main:1"));
    }

    #[test]
    fn facts_write_off_retains_nothing() {
        let p = project(FactsWrite::Off, true);
        assert_eq!(fact_target(Some(&p), "main", "web:main:1", true), None);
    }

    #[test]
    fn a_worker_keeps_what_it_finished_in_the_projects_bank_and_only_when_the_project_keeps_facts_there(
    ) {
        let t = worker_fact_target(&project(FactsWrite::Project, true)).unwrap();
        assert_eq!(
            (t.bank.as_str(), t.origin.as_str()),
            ("project:prj_0a1b2c3d", "")
        );
        // The same place a chat of the project writes to, so the two read each other's.
        assert_eq!(
            Some(t),
            fact_target(
                Some(&project(FactsWrite::Project, true)),
                "main",
                "web:main:1",
                true
            )
        );
        // `chat` would put a worker's facts in a profile bank it does not have,
        // tagged with a chat it does not have.
        assert_eq!(worker_fact_target(&project(FactsWrite::Chat, true)), None);
        assert_eq!(worker_fact_target(&project(FactsWrite::Off, true)), None);
    }

    #[test]
    fn a_worker_reads_the_projects_bank_alone() {
        // The scope takes no project row: `inherit_common` has no say in it.
        let q = worker_recall_query("prj_0a1b2c3d", "fix login", 5, 1_700_000_000);
        assert_eq!(
            q.project,
            Some(ProjectRecall {
                bank: "project:prj_0a1b2c3d".into(),
                include_common: false
            })
        );
        assert_eq!((q.folder.as_str(), q.chat_jid.as_str()), ("", ""));
        assert_eq!(
            (q.text.as_str(), q.limit, q.now),
            ("fix login", 5, 1_700_000_000)
        );
        assert_eq!(
            worker_recall_scope("prj_0a1b2c3d").bank,
            project_bank("prj_0a1b2c3d")
        );
    }

    #[test]
    fn recall_reads_the_project_bank_whatever_facts_write_says() {
        for w in [FactsWrite::Project, FactsWrite::Chat, FactsWrite::Off] {
            let r = recall_scope(Some(&project(w, true))).unwrap();
            assert_eq!(r.bank, "project:prj_0a1b2c3d");
            assert!(r.include_common);
        }
        assert!(
            !recall_scope(Some(&project(FactsWrite::Project, false)))
                .unwrap()
                .include_common
        );
    }
}
