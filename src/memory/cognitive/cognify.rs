//! Cognify pipeline — port of cognee `cognify(text)`.
//!
//! ```text
//!   text  ──chunk──▶  Chunk DataPoints
//!                          │
//!                          ▼
//!                   LLM triplet extraction
//!                          │
//!                          ▼
//!                  entity resolution
//!                  (exact → fuzzy → vector)
//!                          │
//!                          ▼
//!              upsert nodes + Hebbian edges
//!              (chunk -[MENTIONS]→ entity,
//!               entity -[pred]→ entity)
//! ```
//!
//! Idempotent: re-running on the same text **strengthens** existing edges
//! (Hebbian) and dedupes chunks via content-hash, instead of duplicating.

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::memory::chunker::{chunk_text, ChunkerOptions};

use super::data_point::DataPoint;
use super::embed::CognitiveEmbedder;
use super::llm::{parse_triplets, LlmClient, RawTriplet};
use super::node_set::NodeSet;
use super::tiers::EdgeTier;
use super::triplet::RelationshipEdge;

// System prompt for the triplet-extraction LLM call.
//
// Design notes:
//   * Multilingual on purpose — chats arrive in Vietnamese, English, etc.
//     Earlier prompt only said "extract triplets" and small models would
//     return `{"triplets":[]}` on Vietnamese first-person statements like
//     "tôi tên là Sen" because no English noun was present. We now spell
//     out that everyday self-introductions count as facts and that entity
//     names MUST stay in the source script (no transliteration).
//   * One-shot example anchors the schema for instruct models that drift
//     on JSON formatting (Qwen, smaller Llamas, etc.).
//   * `response_format=json_object` is already set by OpenAiCompatLlm, so
//     we don't need defensive "JSON only" reminders past the example.
const SYSTEM_PROMPT: &str = "\
You are a knowledge-graph builder. Read the text and extract every (subject, predicate, object) \
factual relationship — including everyday claims like names, locations, preferences, ownerships, \
roles, and identities, even when the sentence is short or first-person. Treat first-person pronouns \
(I/tôi/我/etc.) as a concrete subject when an identity statement is being made. Keep entity names \
in the SAME script as the source (don't translate or transliterate). Use compact, lowercase, \
English predicates (e.g. `name`, `lives_in`, `likes`, `is_a`, `works_at`, `owns`). Also tag each \
subject and object with `subject_type` / `object_type`: a short, lowercase, English category for \
the entity (e.g. `person`, `city`, `country`, `organization`, `food`, `animal`, `concept`). Use an \
empty string when the category is genuinely unclear. Skip filler words and questions. Return JSON \
only.\n\
The text may be a conversation transcript with lines like `alice: message`. Attribute each line's \
first-person claims (I/tôi/我) to that line's speaker when the speaker is a plain name, and resolve \
pronouns or references (it/he/nó/anh ấy/dự án đó) using earlier lines. Facts may span lines — a \
question on one line answered on the next still yields a triplet, with the subject taken from the \
question's context.\n\n\
Schema: {\"triplets\":[{\"subject\":\"...\",\"subject_type\":\"...\",\"predicate\":\"...\",\"object\":\"...\",\"object_type\":\"...\"}]}\n\n\
Example input: \"tôi tên là Sen, sống ở Hà Nội và thích cà phê đen.\"\n\
Example output: {\"triplets\":[\
{\"subject\":\"tôi\",\"subject_type\":\"person\",\"predicate\":\"name\",\"object\":\"Sen\",\"object_type\":\"person\"},\
{\"subject\":\"tôi\",\"subject_type\":\"person\",\"predicate\":\"lives_in\",\"object\":\"Hà Nội\",\"object_type\":\"city\"},\
{\"subject\":\"tôi\",\"subject_type\":\"person\",\"predicate\":\"likes\",\"object\":\"cà phê đen\",\"object_type\":\"food\"}\
]}";

/// Cap on entity names injected into the extraction prompt as the
/// "already known" hint — ~24 names ≈ a few dozen tokens per call.
const KNOWN_ENTITIES_HINT_MAX: usize = 24;

fn content_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

/// Pre-process text before it enters the cognify pipeline.
///
/// Some chat platforms wrap user messages in an envelope before they reach
/// the agent (the channel adapters used by groups, plus our own
/// `<messages><message sender="...">…</message></messages>` history
/// envelope). When such a wrapper is fed straight into cognify it lands
/// as a chunk node whose `summary` is the entire raw envelope — pure
/// noise in DataPoints view, and triplet extraction on it produces
/// "(messages, contain, message)" nonsense.
///
/// This pass:
///   * Strips known envelope tags (`<messages>`, `<message …>`, closing
///     variants). The XML attributes themselves get dropped — we only
///     keep the inner text payload.
///   * Returns `None` when, after cleanup, the remaining text is too
///     markup-heavy (>40% tag chars) or too short. The caller skips
///     cognify in that case.
///
/// Pure function — exhaustive test coverage in the module's `tests`
/// submodule. Used by both [`CognifyPipeline::cognify`] and by
/// `agent_pool::cognitive_reflect` as a defence-in-depth: even if a
/// caller forgets to sanitise, the pipeline catches it.
pub fn sanitize_for_cognify(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Strip well-known envelope tags. We do this in one pass with simple
    // string ops; a real XML parser is overkill (the envelopes are
    // half-XML at best — message bodies may contain unescaped angle
    // brackets we'd choke on).
    let mut cleaned = trimmed.to_string();
    for tag in ["</messages>", "<messages>"] {
        cleaned = cleaned.replace(tag, " ");
    }
    // Keep who-said-what: `<message sender="X" …>body</message>` becomes a
    // `X: body` transcript line instead of dropping the attribution — the
    // extractor's transcript guidance uses the speaker to resolve pronouns
    // and first-person claims in group chats.
    cleaned = cleaned.replace("</message>", "\n");
    cleaned = speakerize_message_tags(&cleaned);
    // Drop <thinking> blocks if any made it through.
    cleaned = strip_block(&cleaned, "<thinking>", "</thinking>");
    cleaned = strip_block(&cleaned, "<think>", "</think>");

    // Collapse whitespace per line but keep the line structure — speaker
    // turns must stay on separate lines for the transcript guidance.
    let cleaned: String = cleaned
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if cleaned.is_empty() {
        return None;
    }

    // Markup-ratio guard. Count remaining angle brackets — if the cleaned
    // text still has many `<…>` constructs the caller probably handed us
    // raw HTML / a JSON dump / something else not made of sentences.
    let total = cleaned.chars().count() as f32;
    let markup = cleaned.chars().filter(|c| *c == '<' || *c == '>').count() as f32;
    // 0.4 = 40% (matches the doc comment above). HTML / JSON dumps run
    // 50–60%+ angle-bracket chars and stay rejected; ordinary prose with the
    // odd comparison ("giá < 100k", "a -> b") clears the bar instead of being
    // silently dropped before it ever reaches triplet extraction.
    if total < 10.0 || markup / total > 0.4 {
        return None;
    }

    Some(cleaned)
}

/// Replace every `<message …>` opening tag with a `sender: ` speaker
/// prefix on its own line (attribution extracted from the `sender="…"`
/// attribute). Tags without a sender degrade to plain stripping; a
/// malformed tag (no closing `>`) drops the rest to avoid an infinite
/// loop, mirroring the old strip behavior.
fn speakerize_message_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("<message") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + "<message".len()..];
        if let Some(close) = after_open.find('>') {
            if let Some(sender) = attr_value(&after_open[..close], "sender=\"") {
                let sender = sender.trim();
                if !sender.is_empty() {
                    out.push('\n');
                    out.push_str(sender);
                    out.push_str(": ");
                }
            }
            rest = &after_open[close + 1..];
        } else {
            // Malformed — drop the rest to avoid infinite loop.
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

/// Value of `key…"` inside a tag's attribute string, e.g.
/// `attr_value(r#"sender="alice" time="t""#, "sender=\"")` → `Some("alice")`.
fn attr_value<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let start = attrs.find(key)? + key.len();
    let rest = &attrs[start..];
    rest.find('"').map(|end| &rest[..end])
}

/// Remove every `<open>…</close>` span (inclusive of the delimiters).
fn strip_block(s: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        if let Some(end) = after_open.find(close) {
            rest = &after_open[end + close.len()..];
        } else {
            // Unclosed block — drop the rest.
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

fn build_user_prompt(text: &str, known_entities: &[String]) -> String {
    let entity_hint = if known_entities.is_empty() {
        String::new()
    } else {
        format!(
            "\nPotential entities already known (reuse when applicable): {}\n",
            known_entities.join(", ")
        )
    };
    format!("Extract triplets from the following text.{entity_hint}\nText:\n{text}\n")
}

/// Options controlling the cognify run.
#[derive(Debug, Clone)]
pub struct CognifyOptions {
    pub chunker: ChunkerOptions,
    /// Node-set tags applied to every node produced by this run.
    pub node_sets: Vec<NodeSet>,
    /// Cap on triplets per chunk — defensive against runaway LLM output.
    pub max_triplets_per_chunk: usize,
    /// Importance signal forwarded to `RelationshipEdge::strengthen` for
    /// every new edge produced during this run. Values in (0, 1].
    pub importance: f32,
}

impl Default for CognifyOptions {
    fn default() -> Self {
        Self {
            chunker: ChunkerOptions::default(),
            node_sets: Vec::new(),
            max_triplets_per_chunk: 32,
            importance: 0.8,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct CognifyReport {
    pub chunks_added: usize,
    pub chunks_deduped: usize,
    pub entities_added: usize,
    pub entities_reused: usize,
    /// True when the LLM call itself failed (no client wired, rate-limit,
    /// auth, etc.) — distinct from "LLM ran but returned 0 triplets".
    /// Lets callers (CogAdd, agent_pool reflection) tell the user "set up
    /// an LLM" vs "your message had no facts to extract".
    pub llm_skipped: bool,
    pub edges_added: usize,
    pub edges_strengthened: usize,
    /// Older facts this run superseded — a new `sell_price` closing the
    /// previous one. Not a deletion: the row stays as history and is still
    /// reachable through an `as_of` query.
    pub edges_invalidated: usize,
}

pub struct CognifyPipeline {
    embedder: CognitiveEmbedder,
    llm: Arc<dyn LlmClient>,
}

impl CognifyPipeline {
    pub fn new(embedder: CognitiveEmbedder, llm: Arc<dyn LlmClient>) -> Self {
        Self { embedder, llm }
    }

    /// Run the pipeline on a single document. The same `source` string ties
    /// repeated runs together (chunks dedupe by content hash regardless,
    /// `source` is just for provenance / future filtering).
    pub async fn cognify(
        &self,
        text: &str,
        _source: &str,
        opts: &CognifyOptions,
    ) -> Result<CognifyReport> {
        let now = Utc::now().timestamp();
        let mut report = CognifyReport::default();
        let chunks = chunk_text(text, opts.chunker);

        // Prompt hint: entity names the graph already knows, so the
        // extractor reuses them instead of minting aliases ("HN" vs
        // "Hà Nội"). Scoped to the first node-set (the chat group) when
        // present. Best-effort — an empty hint degrades to old behavior.
        let known_entities = self
            .embedder
            .graph
            .top_entity_names(opts.node_sets.first(), KNOWN_ENTITIES_HINT_MAX)
            .unwrap_or_default();

        for ch in chunks {
            // Sanitize before hashing so identical messages wrapped in
            // varying envelopes (different `time="..."` attributes) dedupe
            // to the same content_hash. Senders are preserved as speaker
            // prefixes, so the same text from two speakers is deliberately
            // two different chunks.
            // Drops the chunk entirely when sanitize returns None — saves
            // an embedding call + an LLM call for pure-markup junk.
            let cleaned = match sanitize_for_cognify(&ch.text) {
                Some(s) => s,
                None => {
                    tracing::debug!(
                        len = ch.text.len(),
                        "[cognify] dropping chunk — sanitize rejected (envelope/markup-heavy)"
                    );
                    continue;
                }
            };
            let hash = content_hash(&cleaned);
            // Dedupe by content hash. `was_deduped` carries forward so we
            // can decide whether the LLM still has work to do on this chunk
            // — see the "skip if already extracted" gate below.
            let (chunk_node, was_deduped) = match self
                .embedder
                .graph
                .find_node_by_content_hash(&hash)
                .context("find_node_by_content_hash")?
            {
                Some(existing) => {
                    report.chunks_deduped += 1;
                    (existing, true)
                }
                None => {
                    // Store the *cleaned* text so DataPoints view shows the
                    // payload, not the envelope.
                    let node = DataPoint::chunk(cleaned.clone(), Some(hash), now);
                    // Persist + embed in one shot.
                    self.embedder
                        .add_node(&node)
                        .await
                        .context("embed chunk node")?;
                    for set in &opts.node_sets {
                        let _ = self.embedder.graph.tag_node(node.id, set);
                    }
                    report.chunks_added += 1;
                    (node, false)
                }
            };

            // Stronger dedupe via the persisted extraction_state column.
            // Beats the old "does this chunk have neighbors?" probe because:
            //   * One column read vs. a neighbors() scan.
            //   * Disambiguates `SkippedNoFacts` (LLM said nothing to
            //     extract — don't retry) from `SkippedNoLlm` (LLM was
            //     dormant — DO retry next time).
            if was_deduped {
                match chunk_node.extraction_state {
                    super::ExtractionState::Done | super::ExtractionState::SkippedNoFacts => {
                        tracing::debug!(
                            chunk_id = %chunk_node.id,
                            state = ?chunk_node.extraction_state,
                            "[cognify] skip re-extraction — chunk already processed"
                        );
                        continue;
                    }
                    super::ExtractionState::Pending | super::ExtractionState::SkippedNoLlm => {
                        // Either fresh (race?) or back-fill needed — fall
                        // through and let the LLM try again.
                    }
                }
            }

            let (triplets, skipped) = self
                .extract_triplets(&cleaned, &known_entities)
                .await
                .context("extract triplets")?;
            // Latch the skipped flag across chunks — one bad chunk shouldn't
            // mask the rest, but we want the report to flag the run as a
            // whole when the LLM never came up.
            if skipped {
                report.llm_skipped = true;
            }
            let new_state = if skipped {
                super::ExtractionState::SkippedNoLlm
            } else if triplets.is_empty() {
                super::ExtractionState::SkippedNoFacts
            } else {
                super::ExtractionState::Done
            };

            for raw in triplets.into_iter().take(opts.max_triplets_per_chunk) {
                self.upsert_triplet(&chunk_node, &raw, opts, &mut report, now)
                    .await?;
            }

            // Persist the state only AFTER the triplets landed, so the next
            // call can short-circuit at the dedupe gate above. Writing it
            // earlier marked chunks `Done` even when a mid-loop error threw
            // their edges away — those chunks then never re-extracted.
            let _ = self
                .embedder
                .graph
                .set_extraction_state(chunk_node.id, new_state, now);
        }

        Ok(report)
    }

    /// Extract triplets from one chunk.
    ///
    /// Returns `(triplets, llm_skipped)`:
    ///   * `llm_skipped=true` means the LLM call itself failed (no client,
    ///     network, auth, etc.) — caller distinguishes "user needs to
    ///     configure an LLM" from "your text had no facts".
    ///   * `llm_skipped=false` + empty Vec means the LLM ran and produced
    ///     nothing useful (parse error or empty triplet list).
    async fn extract_triplets(
        &self,
        text: &str,
        known_entities: &[String],
    ) -> Result<(Vec<RawTriplet>, bool)> {
        let user = build_user_prompt(text, known_entities);
        let raw = match self.llm.complete(SYSTEM_PROMPT, &user).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "[cognify] LLM call failed; skipping triplet extraction");
                return Ok((Vec::new(), true));
            }
        };
        match parse_triplets(&raw) {
            Ok(t) => Ok((t, false)),
            Err(e) => {
                tracing::warn!(error = %e, "[cognify] triplet parse failed; skipping chunk");
                Ok((Vec::new(), false))
            }
        }
    }

    async fn upsert_triplet(
        &self,
        chunk_node: &DataPoint,
        raw: &RawTriplet,
        opts: &CognifyOptions,
        report: &mut CognifyReport,
        now: i64,
    ) -> Result<()> {
        if raw.subject.trim().is_empty()
            || raw.object.trim().is_empty()
            || raw.predicate.trim().is_empty()
        {
            return Ok(());
        }

        let subj = self
            .resolve_or_create_entity(&raw.subject, &raw.subject_type, opts, report, now)
            .await?;
        let obj = self
            .resolve_or_create_entity(&raw.object, &raw.object_type, opts, report, now)
            .await?;

        // chunk -[MENTIONS]→ subject / object (provenance edges).
        for ent in [&subj, &obj] {
            let mention = RelationshipEdge::new(chunk_node.id, ent.id, "MENTIONS", now)
                .with_tier(EdgeTier::L2Episodic);
            let existed = self
                .embedder
                .graph
                .neighbors(chunk_node.id, 256)?
                .into_iter()
                .find(|e| e.dst == ent.id && e.predicate == "MENTIONS");
            match existed {
                Some(mut e) => {
                    e.strengthen(opts.importance, now);
                    self.embedder.graph.upsert_edge(&e)?;
                    report.edges_strengthened += 1;
                }
                None => {
                    let mut m = mention;
                    m.strengthen(opts.importance, now);
                    self.embedder.graph.upsert_edge(&m)?;
                    report.edges_added += 1;
                }
            }
        }

        // subject -[predicate]→ object (semantic edge).
        let existing = self
            .embedder
            .graph
            .neighbors(subj.id, 256)?
            .into_iter()
            .find(|e| e.dst == obj.id && e.predicate.eq_ignore_ascii_case(&raw.predicate));
        match existing {
            Some(mut e) => {
                // Seeing the fact asserted again is the one thing that may
                // re-open a closed interval — the price came back, the person
                // moved back. Retrieval-time strengthening must not.
                e.reassert(now);
                e.strengthen(opts.importance, now);
                self.embedder.graph.upsert_edge(&e)?;
                report.edges_strengthened += 1;
            }
            None => {
                let mut e = RelationshipEdge::new(subj.id, obj.id, raw.predicate.clone(), now)
                    .with_tier(EdgeTier::L2Episodic);
                e.context = chunk_node.id.to_string();
                e.source_episode_id = Some(chunk_node.id);
                e.strengthen(opts.importance, now);
                self.embedder.graph.upsert_edge(&e)?;
                report.edges_added += 1;
            }
        }

        report.edges_invalidated +=
            self.supersede_conflicting(&subj, &obj, &raw.predicate, chunk_node.id, now)?;

        Ok(())
    }

    /// Close older facts that the one just written replaces.
    ///
    /// Only for predicates the registry marks `single` — `sell_price` has one
    /// value at a time, `has_task` has several, and treating them alike is how
    /// a graph ends up holding three simultaneous gold prices four days apart
    /// (measured; see docs/temporal-graph-research.md). Unknown predicates are
    /// `multi`, so the default behaviour is exactly what it was before.
    ///
    /// Nothing is deleted: the old edge keeps its row with `valid_to` stamped,
    /// stays reachable through `as_of`, and is simply no longer the answer to
    /// "what is true now".
    fn supersede_conflicting(
        &self,
        subj: &DataPoint,
        obj: &DataPoint,
        predicate: &str,
        chunk_id: Uuid,
        now: i64,
    ) -> Result<usize> {
        if !self
            .embedder
            .graph
            .predicate_cardinality(predicate)?
            .supersedes()
        {
            return Ok(0);
        }
        let mut closed = 0usize;
        for mut old in self.embedder.graph.current_edges_for(subj.id, predicate)? {
            if old.dst == obj.id {
                continue; // same fact, just re-asserted above
            }
            // Ingesting an OLD document must not overthrow the present. A
            // fact may only be superseded by one that starts no earlier than
            // it does — without this guard, uploading a 2019 archive would
            // silently rewrite today's answers.
            if old.valid_from > now {
                continue;
            }
            old.invalidate(now, chunk_id);
            self.embedder.graph.upsert_edge(&old)?;
            closed += 1;
        }
        if closed > 0 {
            tracing::debug!(
                subject = %subj.name,
                predicate,
                superseded = closed,
                "[cognitive] newer fact closed older ones"
            );
        }
        Ok(closed)
    }

    /// Entity resolution: exact-name match first, then fall through to
    /// creating a new entity. Vector-based fuzzy matching is left to P4
    /// (needs a similarity threshold + benchmark before we trust it).
    ///
    /// `type_hint` is the cognee-style category from the extractor (may be
    /// empty). When present and non-empty, the resolved entity is wired to a
    /// shared [`DataPoint::entity_type`] node via an `is_a` edge so the graph
    /// carries `entity -is_a-> type` exactly like cognee's
    /// `expand_with_nodes_and_edges`.
    async fn resolve_or_create_entity(
        &self,
        name: &str,
        type_hint: &str,
        opts: &CognifyOptions,
        report: &mut CognifyReport,
        now: i64,
    ) -> Result<DataPoint> {
        let canonical = name.trim();
        let type_name = type_hint.trim().to_lowercase();

        let entity = match self.embedder.graph.find_entity_by_name(canonical)? {
            Some(existing) => {
                report.entities_reused += 1;
                existing
            }
            None => {
                let mut node = DataPoint::entity(canonical, now);
                node.type_name = type_name.clone();
                self.embedder.add_node(&node).await?;
                for set in &opts.node_sets {
                    let _ = self.embedder.graph.tag_node(node.id, set);
                }
                report.entities_added += 1;
                node
            }
        };

        // entity -[is_a]→ type (cognee EntityType). Folded into the existing
        // entities_added / edges_added counters to keep the report shape
        // stable for downstream callers.
        if !type_name.is_empty() {
            self.link_entity_type(&entity, &type_name, opts, report, now)
                .await?;
        }

        Ok(entity)
    }

    /// Ensure the shared `EntityType` node exists and the `entity -is_a-> type`
    /// edge is present (creating or Hebbian-strengthening it). The type node's
    /// id is deterministic (UUIDv5 of the name), so repeated runs dedupe
    /// without a name lookup.
    async fn link_entity_type(
        &self,
        entity: &DataPoint,
        type_name: &str,
        opts: &CognifyOptions,
        report: &mut CognifyReport,
        now: i64,
    ) -> Result<()> {
        let type_node = DataPoint::entity_type(type_name, now);
        // Idempotent: ON CONFLICT(id) upsert. Only count it as "added" the
        // first time the node is new to the graph.
        if self.embedder.graph.get_node(type_node.id)?.is_none() {
            self.embedder.add_node(&type_node).await?;
            for set in &opts.node_sets {
                let _ = self.embedder.graph.tag_node(type_node.id, set);
            }
            report.entities_added += 1;
        }

        let existing = self
            .embedder
            .graph
            .neighbors(entity.id, 256)?
            .into_iter()
            .find(|e| e.dst == type_node.id && e.predicate.eq_ignore_ascii_case("is_a"));
        match existing {
            Some(mut e) => {
                e.strengthen(opts.importance, now);
                self.embedder.graph.upsert_edge(&e)?;
                report.edges_strengthened += 1;
            }
            None => {
                let mut e = RelationshipEdge::new(entity.id, type_node.id, "is_a", now)
                    .with_tier(EdgeTier::L2Episodic);
                e.strengthen(opts.importance, now);
                self.embedder.graph.upsert_edge(&e)?;
                report.edges_added += 1;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod sanitize_tests {
    use super::sanitize_for_cognify;

    #[test]
    fn strips_messages_envelope_keeps_inner_text() {
        let raw = r#"<messages> <message sender="ext:default" time="2026-05-21T09:21:46+00:00">tôi tên là Sen, sống ở Hà Nội</message></messages>"#;
        let out = sanitize_for_cognify(raw).expect("envelope should yield clean text");
        assert!(out.contains("tôi tên là Sen"));
        assert!(out.contains("Hà Nội"));
        // Wrapper artefacts gone.
        assert!(!out.contains("<message"));
        assert!(!out.contains("sender="));
        assert!(!out.contains("time="));
        // Attribution preserved as a speaker prefix.
        assert!(
            out.contains("ext:default: tôi tên là Sen"),
            "speaker prefix expected, got: {out:?}"
        );
    }

    #[test]
    fn multi_speaker_envelope_becomes_transcript_lines() {
        let raw = concat!(
            r#"<messages><message sender="an" time="t1">SemaClaw deadline khi nào?</message>"#,
            r#"<message sender="binh" time="t2">tháng 8 nhé</message></messages>"#,
        );
        let out = sanitize_for_cognify(raw).expect("transcript should survive");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "one line per turn: {out:?}");
        assert_eq!(lines[0], "an: SemaClaw deadline khi nào?");
        assert_eq!(lines[1], "binh: tháng 8 nhé");
    }

    #[test]
    fn message_tag_without_sender_still_strips_cleanly() {
        let raw = r#"<message time="t">nội dung không có sender</message>"#;
        let out = sanitize_for_cognify(raw).expect("clean text");
        assert_eq!(out, "nội dung không có sender");
    }

    #[test]
    fn drops_thinking_blocks() {
        let raw = "<think>Okay let me reason</think>The user says they like coffee.";
        let out = sanitize_for_cognify(raw).expect("non-think part should survive");
        assert!(out.contains("user says they like coffee"));
        assert!(!out.contains("Okay let me reason"));
    }

    #[test]
    fn rejects_pure_envelope_with_no_inner_text() {
        // Just the wrapper, no content — caller should skip cognify.
        let raw = r#"<messages><message sender="x" time="t"></message></messages>"#;
        assert_eq!(sanitize_for_cognify(raw), None);
    }

    #[test]
    fn rejects_markup_heavy_payload() {
        // 5 chars of text, lots of brackets → markup ratio fails.
        assert_eq!(sanitize_for_cognify("hi <a><b><c><d><e><f><g><h><i>"), None);
    }

    #[test]
    fn keeps_prose_with_occasional_angle_brackets() {
        // Regression: the markup guard is 40%, not 4%. A normal chat line
        // with one comparison operator is ~4–5% brackets and MUST survive —
        // dropping it here loses the fact before triplet extraction ever runs.
        let out = sanitize_for_cognify("tôi nghĩ giá sẽ < 100k và lãi > 5%")
            .expect("prose with a couple of comparisons should pass");
        assert!(out.contains("100k"));
        assert!(out.contains("5%"));
    }

    #[test]
    fn passes_plain_sentence_unchanged_modulo_whitespace() {
        let out = sanitize_for_cognify("  Ada invented the compiler.  ").unwrap();
        assert_eq!(out, "Ada invented the compiler.");
    }

    #[test]
    fn rejects_empty_and_whitespace_only_input() {
        assert_eq!(sanitize_for_cognify(""), None);
        assert_eq!(sanitize_for_cognify("   \n\t  "), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::memory::cognitive::graph_store::SqliteGraphStore;
    use crate::memory::cognitive::llm::test_support::StubLlm;
    use crate::memory::cognitive::vector_store::SqliteVectorStore;
    use crate::memory::embedding::EmbeddingProvider;
    use async_trait::async_trait;
    use std::sync::Arc;

    // Re-import the traits that were removed from the main module
    use crate::memory::cognitive::graph_store::GraphStore;
    use crate::memory::cognitive::vector_store::VectorStore;

    /// Deterministic fake embedder — hashes input text into f32s so we get
    /// stable but distinct vectors without touching the network or MLX.
    struct FakeEmbedder;

    #[async_trait]
    impl EmbeddingProvider for FakeEmbedder {
        fn name(&self) -> &str {
            "fake"
        }
        fn model(&self) -> &str {
            "fake-model"
        }
        fn dimensions(&self) -> u32 {
            8
        }
        async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; 8];
                    for (i, b) in t.bytes().enumerate() {
                        v[i % 8] += b as f32;
                    }
                    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
                    v.iter().map(|x| x / norm).collect()
                })
                .collect())
        }
    }

    fn build_pipeline(replies: Vec<String>) -> CognifyPipeline {
        let cfg = Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let graph: Arc<dyn GraphStore> = Arc::new(SqliteGraphStore::new(Arc::clone(&db)));
        let vector: Arc<dyn VectorStore> = Arc::new(SqliteVectorStore::new(Arc::clone(&db)));
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbedder);
        let embedder = CognitiveEmbedder::new(graph, vector, provider);
        let llm = Arc::new(StubLlm::new(replies));
        CognifyPipeline::new(embedder, llm)
    }

    // ── Temporal: a new fact supersedes the one it replaces ──────────────
    //
    // Modelled on the measured failure (docs/temporal-graph-research.md):
    // BTMC's sell_price arrived three times in five days and all three sat in
    // the graph as equally current, so "giá vàng BTMC bao nhiêu?" was answered
    // by vector similarity rather than by recency.

    fn price_reply(shop: &str, value: &str) -> String {
        format!(
            r#"{{"triplets":[{{"subject":"{shop}","subject_type":"organization","predicate":"sell_price","object":"{value}","object_type":"price"}}]}}"#
        )
    }

    async fn current_objects(pipe: &CognifyPipeline, subject: &str, predicate: &str) -> Vec<String> {
        let subj = pipe
            .embedder
            .graph
            .find_entity_by_name(subject)
            .unwrap()
            .expect("subject entity");
        pipe.embedder
            .graph
            .current_edges_for(subj.id, predicate)
            .unwrap()
            .into_iter()
            .map(|e| {
                pipe.embedder
                    .graph
                    .get_node(e.dst)
                    .unwrap()
                    .map(|n| n.name)
                    .unwrap_or_default()
            })
            .collect()
    }

    #[tokio::test]
    async fn a_newer_single_valued_fact_supersedes_the_old_one() {
        let pipe = build_pipeline(vec![
            price_reply("BTMC", "149.900"),
            price_reply("BTMC", "141.500"),
        ]);
        pipe.cognify("BTMC bán 149.900", "d1", &CognifyOptions::default())
            .await
            .unwrap();
        let second = pipe
            .cognify("BTMC bán 141.500", "d2", &CognifyOptions::default())
            .await
            .unwrap();

        assert_eq!(second.edges_invalidated, 1);
        assert_eq!(
            current_objects(&pipe, "BTMC", "sell_price").await,
            vec!["141.500".to_string()],
            "only the latest price may answer as current"
        );

        // Nothing was destroyed: the old price is still on the timeline.
        let subj = pipe
            .embedder
            .graph
            .find_entity_by_name("BTMC")
            .unwrap()
            .unwrap();
        let history = pipe
            .embedder
            .graph
            .edge_history(subj.id, Some("sell_price"), 20)
            .unwrap();
        assert_eq!(history.len(), 2);
        assert!(history.iter().any(|e| e.valid_to.is_some() && e.invalidated_by.is_some()));
    }

    #[tokio::test]
    async fn multi_valued_predicates_accumulate_untouched() {
        // Three tasks are three tasks — the supersession rule must not fire
        // for a predicate that is legitimately many-valued.
        let reply = |task: &str| {
            format!(
                r#"{{"triplets":[{{"subject":"tôi","subject_type":"person","predicate":"has_task","object":"{task}","object_type":"task"}}]}}"#
            )
        };
        let pipe = build_pipeline(vec![reply("viết báo cáo"), reply("phân tích giá vàng")]);
        pipe.cognify("giao việc 1", "d1", &CognifyOptions::default())
            .await
            .unwrap();
        let second = pipe
            .cognify("giao việc 2", "d2", &CognifyOptions::default())
            .await
            .unwrap();

        assert_eq!(second.edges_invalidated, 0);
        let mut tasks = current_objects(&pipe, "tôi", "has_task").await;
        tasks.sort();
        assert_eq!(tasks.len(), 2, "both tasks stay current");
    }

    #[tokio::test]
    async fn re_asserting_the_same_fact_only_strengthens_it() {
        let pipe = build_pipeline(vec![
            price_reply("BTMC", "149.900"),
            price_reply("BTMC", "149.900"),
        ]);
        pipe.cognify("BTMC bán 149.900", "d1", &CognifyOptions::default())
            .await
            .unwrap();
        let again = pipe
            .cognify("BTMC vẫn bán 149.900", "d2", &CognifyOptions::default())
            .await
            .unwrap();

        assert_eq!(again.edges_invalidated, 0, "a fact cannot supersede itself");
        assert!(again.edges_strengthened > 0);
        assert_eq!(
            current_objects(&pipe, "BTMC", "sell_price").await,
            vec!["149.900".to_string()]
        );
    }

    // Someone moves away and back: the fact must be current again, not
    // stuck closed.
    #[tokio::test]
    async fn a_returning_fact_is_re_opened_by_extraction() {
        let lives = |city: &str| {
            format!(
                r#"{{"triplets":[{{"subject":"Sen","subject_type":"person","predicate":"lives_in","object":"{city}","object_type":"city"}}]}}"#
            )
        };
        let pipe = build_pipeline(vec![lives("Hà Nội"), lives("Đà Nẵng"), lives("Hà Nội")]);
        for (i, text) in ["Sen sống ở Hà Nội", "Sen chuyển vào Đà Nẵng", "Sen quay lại Hà Nội"]
            .iter()
            .enumerate()
        {
            let r = pipe
                .cognify(text, &format!("d{i}"), &CognifyOptions::default())
                .await
                .unwrap();
            assert!(!r.llm_skipped, "step {i} did not reach the extractor");
        }
        assert_eq!(
            current_objects(&pipe, "Sen", "lives_in").await,
            vec!["Hà Nội".to_string()],
            "moving back must make the original fact current again — and alone"
        );
    }

    #[tokio::test]
    async fn recall_returns_the_current_fact_not_the_superseded_one() {
        // The whole point, end to end: retrieval walks the graph and must not
        // hand an outdated value to the agent.
        let pipe = build_pipeline(vec![
            price_reply("BTMC", "149.900"),
            price_reply("BTMC", "141.500"),
        ]);
        pipe.cognify("BTMC bán 149.900", "d1", &CognifyOptions::default())
            .await
            .unwrap();

        let subj = pipe
            .embedder
            .graph
            .find_entity_by_name("BTMC")
            .unwrap()
            .unwrap();

        // Backdate the first price by two days. Both cognify runs happen in
        // the same second here, and a fact whose interval is zero seconds
        // wide is — correctly — reachable at no point in time at all; the
        // real timeline this models spans days.
        let two_days_ago = chrono::Utc::now().timestamp() - 2 * 86_400;
        let mut first = pipe
            .embedder
            .graph
            .neighbors(subj.id, 32)
            .unwrap()
            .into_iter()
            .find(|e| e.predicate.eq_ignore_ascii_case("sell_price"))
            .unwrap();
        first.valid_from = two_days_ago;
        pipe.embedder.graph.upsert_edge(&first).unwrap();

        pipe.cognify("BTMC bán 141.500", "d2", &CognifyOptions::default())
            .await
            .unwrap();

        let reachable = |as_of: Option<i64>| -> Vec<String> {
            pipe.embedder
                .graph
                .neighbors_at(subj.id, 32, as_of)
                .unwrap()
                .into_iter()
                .filter(|e| e.predicate.eq_ignore_ascii_case("sell_price"))
                .map(|e| {
                    pipe.embedder
                        .graph
                        .get_node(e.dst)
                        .unwrap()
                        .map(|n| n.name)
                        .unwrap_or_default()
                })
                .collect()
        };

        assert_eq!(
            reachable(None),
            vec!["141.500".to_string()],
            "a recall about now must not reach the superseded price"
        );
        // Time travel still reaches yesterday's answer.
        assert_eq!(
            reachable(Some(chrono::Utc::now().timestamp() - 86_400)),
            vec!["149.900".to_string()],
            "as_of must return the price that was in force then"
        );
    }

    #[tokio::test]
    async fn cognify_creates_nodes_and_edges() {
        let canned =
            r#"{"triplets":[{"subject":"Ada","predicate":"invented","object":"compiler"}]}"#
                .to_string();
        let pipe = build_pipeline(vec![canned]);
        let report = pipe
            .cognify(
                "Ada invented the compiler.",
                "doc1",
                &CognifyOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(report.chunks_added, 1);
        assert_eq!(report.entities_added, 2);
        // 1 semantic edge + 2 MENTIONS provenance edges
        assert_eq!(report.edges_added, 3);
    }

    #[tokio::test]
    async fn cognify_edges_start_in_episodic_tier() {
        // Extracted facts must NOT start in L1Working — its 2.9%/hour decay
        // + 1-day max age deletes a once-mentioned fact within hours, which
        // is how the graph degenerated into thousands of edge-less entities.
        let canned = r#"{"triplets":[{"subject":"Sen","subject_type":"person","predicate":"lives_in","object":"Hà Nội","object_type":"city"}]}"#.to_string();
        let pipe = build_pipeline(vec![canned]);
        pipe.cognify("Sen sống ở Hà Nội.", "doc", &CognifyOptions::default())
            .await
            .unwrap();

        let edges = pipe.embedder.graph.scan_edges(100, 0).unwrap();
        assert!(!edges.is_empty());
        for e in &edges {
            assert_eq!(
                e.tier,
                EdgeTier::L2Episodic,
                "edge {} should start episodic",
                e.predicate
            );
        }
    }

    #[tokio::test]
    async fn cognify_creates_entity_types_and_is_a_edges() {
        // Typed extraction (cognee parity): each subject/object carries a
        // category → an EntityType node + an `is_a` edge per typed entity.
        let canned = r#"{"triplets":[{"subject":"Sen","subject_type":"person","predicate":"lives_in","object":"Hà Nội","object_type":"city"}]}"#.to_string();
        let pipe = build_pipeline(vec![canned]);
        let report = pipe
            .cognify("Sen sống ở Hà Nội.", "doc", &CognifyOptions::default())
            .await
            .unwrap();

        // 2 entities (Sen, Hà Nội) + 2 type nodes (person, city).
        assert_eq!(report.entities_added, 4);
        // 1 semantic (lives_in) + 2 MENTIONS + 2 is_a.
        assert_eq!(report.edges_added, 5);

        // Type node exists under its deterministic id, with the right kind.
        let person = DataPoint::entity_type("person", 0);
        let fetched = pipe
            .embedder
            .graph
            .get_node(person.id)
            .unwrap()
            .expect("person type node should be persisted");
        assert_eq!(
            fetched.kind,
            crate::memory::cognitive::data_point::NodeKind::EntityType
        );
        assert_eq!(fetched.name, "person");

        // The entity carries the denormalised type for single-read access.
        let sen = pipe
            .embedder
            .graph
            .find_entity_by_name("Sen")
            .unwrap()
            .expect("Sen entity");
        assert_eq!(sen.type_name, "person");

        // And the is_a edge is wired Sen -> person.
        let has_is_a = pipe
            .embedder
            .graph
            .neighbors(sen.id, 64)
            .unwrap()
            .into_iter()
            .any(|e| e.dst == person.id && e.predicate.eq_ignore_ascii_case("is_a"));
        assert!(has_is_a, "expected Sen -is_a-> person edge");
    }

    #[tokio::test]
    async fn cognify_untyped_triplet_creates_no_type_nodes() {
        // Backward-compat: a payload without *_type fields parses fine and
        // produces no EntityType nodes / is_a edges — same shape as before.
        let canned =
            r#"{"triplets":[{"subject":"Ada","predicate":"invented","object":"compiler"}]}"#
                .to_string();
        let pipe = build_pipeline(vec![canned]);
        let report = pipe
            .cognify(
                "Ada invented the compiler.",
                "doc",
                &CognifyOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(report.entities_added, 2);
        assert_eq!(report.edges_added, 3);
    }

    #[tokio::test]
    async fn cognify_dedupe_skips_re_extraction_when_edges_exist() {
        // P-cognify-skip: re-cognify on the same text MUST short-circuit
        // the LLM call entirely once the chunk has edges from a prior
        // extraction. Hebbian strengthen happens via CogRecall, not via
        // wasted LLM passes — there's no reason to re-extract the same
        // sentence on every restart / reflection / SOUL ingest.
        let r1 = r#"{"triplets":[{"subject":"Ada","predicate":"invented","object":"compiler"}]}"#
            .to_string();
        // Second reply MUST stay unused — if it gets pulled, the gate
        // didn't fire and we're wasting tokens on duplicate work.
        let r2_unused = r1.clone();
        let pipe = build_pipeline(vec![r1, r2_unused]);

        let opts = CognifyOptions::default();
        let first = pipe
            .cognify("Ada invented the compiler.", "doc1", &opts)
            .await
            .unwrap();
        let second = pipe
            .cognify("Ada invented the compiler.", "doc1", &opts)
            .await
            .unwrap();

        assert_eq!(first.chunks_added, 1);
        assert!(first.edges_added > 0);

        // Second pass dedupes the chunk and then bails out before the LLM.
        assert_eq!(second.chunks_added, 0);
        assert_eq!(second.chunks_deduped, 1);
        assert_eq!(second.entities_added, 0);
        // No edges added, no strengthen, no LLM skip — clean no-op.
        assert_eq!(second.edges_added, 0);
        assert_eq!(second.edges_strengthened, 0);
        assert!(!second.llm_skipped);
    }

    #[tokio::test]
    async fn cognify_marks_llm_skipped_when_client_fails() {
        // StubLlm with empty replies → first complete() call returns
        // "StubLlm exhausted" Err → extract_triplets soft-fails → flag set.
        // Chunk should still be embedded though.
        let pipe = build_pipeline(vec![]);
        let report = pipe
            .cognify(
                "Ada invented the compiler.",
                "doc",
                &CognifyOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(report.chunks_added, 1, "chunk still stored when LLM fails");
        assert_eq!(report.entities_added, 0);
        assert_eq!(report.edges_added, 0);
        assert!(report.llm_skipped, "llm_skipped must latch on LLM error");
    }

    #[tokio::test]
    async fn cognify_does_not_flag_skipped_on_empty_triplets() {
        // LLM runs but returns 0 triplets — that's a content issue, not an
        // infrastructure issue. The flag must stay false so callers don't
        // misreport a quiet day as a config error.
        let canned = r#"{"triplets":[]}"#.to_string();
        let pipe = build_pipeline(vec![canned]);
        let report = pipe
            .cognify("Hôm nay trời đẹp.", "doc", &CognifyOptions::default())
            .await
            .unwrap();
        assert!(!report.llm_skipped);
    }

    #[tokio::test]
    async fn cognify_skips_empty_triplets() {
        let canned = r#"{"triplets":[{"subject":"","predicate":"x","object":"y"}]}"#.to_string();
        let pipe = build_pipeline(vec![canned]);
        let r = pipe
            .cognify("noise", "doc", &CognifyOptions::default())
            .await
            .unwrap();
        assert_eq!(r.edges_added, 0);
        assert_eq!(r.entities_added, 0);
    }
}
