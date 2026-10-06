//! Compaction (DESIGN.md §14). A compaction's pruning replaces tool
//! results older than the most recent `PROTECT_TOKENS` with stubs in
//! what the model is sent: each names the call and where `history_read`
//! finds the result, which the log keeps whole.

use std::collections::{BTreeMap, BTreeSet};

use crate::store::{Call, Event, Kind, Purpose};
use td_json::Json;

/// Tool results within the most recent this many tokens are kept.
pub const PROTECT_TOKENS: u64 = 40_000;
/// A pruning that would free fewer tokens than this is not done.
pub const MINIMUM_TOKENS: u64 = 20_000;
/// The most characters of a tool's name or path a stub shows.
const SHOWN: usize = 200;

/// The tool results the log's compactions pruned, by sequence number.
pub fn pruned(events: &[Event]) -> BTreeSet<u64> {
    events
        .iter()
        .flat_map(|e| match &e.kind {
            Kind::Compaction { pruned } => pruned.as_slice(),
            _ => &[],
        })
        .copied()
        .collect()
}

/// The path the call `id` of the reply at `reply` names, if any: what
/// the classifier, too, names a call by beside its tool (§11).
pub fn path_of(replies: &BTreeMap<u64, &[Call]>, reply: u64, id: &str) -> Option<String> {
    let call = replies.get(&reply)?.iter().find(|c| c.id == id)?;
    td_json::parse_slice(call.arguments.as_bytes())
        .ok()
        .and_then(|a| a.get("path").and_then(Json::as_str).map(str::to_string))
}

/// What the model is sent in place of the `bytes`-long result at `seq`
/// of a call of `name`, naming `path`.
pub fn stub(name: &str, path: Option<&str>, bytes: usize, seq: u64) -> String {
    let shown = |text: &str| {
        let cut: String = text.chars().take(SHOWN).collect();
        crate::tools::visible(&cut)
    };
    let of = path.map_or(String::new(), |p| format!(" for {}", shown(p)));
    format!(
        "[The result of {}{of}, {bytes} bytes, was pruned when the conversation was compacted; history_read from {seq} reads it.]",
        shown(name)
    )
}

/// Whether a request of `estimate` tokens, its reply's `max_tokens`, is
/// past `percent` of a model's `context`, and if so the reply's room it
/// was counted with: `max_tokens`, or, where that alone is past the
/// threshold, what the threshold leaves of the context, so that a small
/// context is not past it before anything is said.
pub fn past(estimate: u64, max_tokens: u64, context: u64, percent: u8) -> Option<u64> {
    let percent = u64::from(percent);
    let threshold = context / 100 * percent + context % 100 * percent / 100;
    let room = if max_tokens > threshold {
        max_tokens.min(context.saturating_sub(threshold))
    } else {
        max_tokens
    };
    (estimate.saturating_add(room) > threshold).then_some(room)
}

/// The tool results to prune: those the model has been sent, older than
/// the most recent `protect` tokens of what it is sent, a quarter of a
/// token a byte, not pruned already, when replacing them by stubs frees
/// at least `minimum` tokens; none otherwise. A result logged after the
/// last turn request answered whole has not been seen, and is never
/// pruned.
pub fn prunable(events: &[Event], protect: u64, minimum: u64) -> Vec<u64> {
    let done = pruned(events);
    // The turn requests, the replies they were answered by whole, which
    // alone are sent again, and the last request answered whole: what
    // came after it the model has not seen, a request refused included.
    let mut turns: BTreeSet<u64> = BTreeSet::new();
    let mut replies: BTreeMap<u64, &[Call]> = BTreeMap::new();
    let mut sent = 0u64;
    for event in events {
        match &event.kind {
            Kind::Request {
                purpose: Purpose::Turn,
                ..
            } => {
                turns.insert(event.seq);
            }
            Kind::Assistant {
                request,
                incomplete: false,
                calls,
                ..
            } if turns.contains(request) => {
                replies.insert(event.seq, calls.as_slice());
                sent = sent.max(*request);
            }
            _ => {}
        }
    }
    let stub_of = |seq: u64, reply: u64, id: &str, name: &str, bytes: usize| {
        stub(name, path_of(&replies, reply, id).as_deref(), bytes, seq).len()
    };
    let protect = protect.saturating_mul(4);
    let (mut recent, mut freed) = (0u64, 0u64);
    let mut out = Vec::new();
    for event in events.iter().rev() {
        let bytes = match &event.kind {
            Kind::ToolResult {
                reply,
                id,
                name,
                content,
                ..
            } => {
                if done.contains(&event.seq) {
                    stub_of(event.seq, *reply, id, name, content.len())
                } else {
                    if recent > protect && event.seq < sent {
                        let stub = stub_of(event.seq, *reply, id, name, content.len());
                        let saved = content.len().saturating_sub(stub);
                        if saved > 0 {
                            freed = freed.saturating_add(saved as u64);
                            out.push(event.seq);
                        }
                    }
                    content.len()
                }
            }
            Kind::User { text, .. } | Kind::Notification { text } | Kind::Message { text, .. } => {
                text.len()
            }
            Kind::Ended { tail, .. } => tail.as_ref().map_or(0, String::len),
            Kind::Assistant {
                content,
                details,
                calls,
                ..
            } if replies.contains_key(&event.seq) => {
                content.as_ref().map_or(0, String::len)
                    + details.as_ref().map_or(0, String::len)
                    + calls.iter().map(|c| c.arguments.len()).sum::<usize>()
            }
            _ => 0,
        };
        recent = recent.saturating_add(bytes as u64);
    }
    if out.is_empty() || freed / 4 < minimum {
        return Vec::new();
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::store::Call;

    fn event(seq: u64, kind: Kind) -> Event {
        Event { seq, time: 0, kind }
    }

    /// A turn request at `seq`, its reply calling `id` on `path`, and
    /// the call's result of `bytes` bytes at `seq + 2`.
    fn call(seq: u64, id: &str, path: &str, bytes: usize) -> [Event; 3] {
        [
            event(
                seq,
                Kind::Request {
                    turn: 0,
                    purpose: Purpose::Turn,
                    prefix: 0,
                    head: String::new(),
                    bytes: 0,
                    reserved: 0,
                },
            ),
            event(
                seq + 1,
                Kind::Assistant {
                    request: seq,
                    content: None,
                    reasoning: None,
                    details: None,
                    finish: "tool_calls".into(),
                    incomplete: false,
                    calls: vec![Call {
                        id: id.into(),
                        name: "read_file".into(),
                        arguments: format!(r#"{{"path":"{path}"}}"#),
                    }],
                },
            ),
            event(
                seq + 2,
                Kind::ToolResult {
                    reply: seq + 1,
                    id: id.into(),
                    name: "read_file".into(),
                    call: 0,
                    content: "x".repeat(bytes),
                    error: false,
                    kept: None,
                    digest: None,
                },
            ),
        ]
    }

    /// The turn request that sends what came before it.
    /// A turn request at `seq` answered whole at `seq + 1`, which sends
    /// what came before it.
    fn sending(seq: u64) -> [Event; 2] {
        let [request, reply, _] = call(seq, "z", "z", 0);
        [request, reply]
    }

    /// Results older than the most recent 40,000 tokens are pruned when
    /// that frees 20,000; those within it, those pruned already and a
    /// pruning that frees too little are not.
    #[test]
    fn old_tool_results_are_pruned_when_that_frees_enough() {
        let mut events: Vec<Event> = Vec::new();
        events.extend(call(1, "a", "old.txt", 30_000 * 4));
        events.extend(call(4, "b", "small.txt", 100));
        events.extend(call(7, "c", "recent.txt", 45_000 * 4));
        events.extend(sending(10));
        assert_eq!(prunable(&events, PROTECT_TOKENS, MINIMUM_TOKENS), [3]);
        assert!(prunable(&events, PROTECT_TOKENS, 30_001).is_empty());
        // The small one is old too, but its stub saves nothing.
        assert_eq!(prunable(&events, PROTECT_TOKENS, 0), [3]);
        assert!(prunable(&events, 80_000, 0).is_empty());
        events.push(event(12, Kind::Compaction { pruned: vec![3] }));
        assert_eq!(pruned(&events).into_iter().collect::<Vec<_>>(), [3]);
        assert!(prunable(&events, PROTECT_TOKENS, 0).is_empty());
        // What came after the compaction ages the results before it.
        events.extend(call(13, "d", "newer.txt", 45_000 * 4));
        assert_eq!(prunable(&events, PROTECT_TOKENS, MINIMUM_TOKENS), [9]);
    }

    /// Results one reply asked for together, past the window between
    /// them, are not pruned before the model has seen them; nor does
    /// a reply never sent again, incomplete or not a turn's, age them.
    #[test]
    fn what_the_model_has_not_been_sent_neither_is_pruned_nor_ages() {
        let mut events: Vec<Event> = Vec::new();
        events.extend(call(1, "a", "one.txt", 25_000 * 4));
        for (seq, id) in [(4, "b"), (5, "c")] {
            let mut more = call(1, id, "more.txt", 25_000 * 4)[2].clone();
            more.seq = seq;
            events.push(more);
        }
        assert!(prunable(&events, PROTECT_TOKENS, 0).is_empty());
        // Nor by a request refused, which the model never answered.
        events.push(sending(6)[0].clone());
        assert!(prunable(&events, PROTECT_TOKENS, 0).is_empty());
        events.extend(sending(7));
        assert_eq!(prunable(&events, PROTECT_TOKENS, 0), [3]);
        // An incomplete reply's 200,000 bytes are never sent.
        let mut events: Vec<Event> = call(1, "a", "one.txt", 30_000 * 4).to_vec();
        events.extend(sending(4));
        let mut broken = call(6, "b", "b", 0);
        if let Kind::Assistant {
            incomplete,
            content,
            ..
        } = &mut broken[1].kind
        {
            *incomplete = true;
            *content = Some("y".repeat(200_000));
        }
        events.extend(broken[..2].iter().cloned());
        events.extend(sending(8));
        assert!(prunable(&events, PROTECT_TOKENS, 0).is_empty());
    }

    /// A request is past `compact_at` with its reply's room, that room
    /// cut to what the threshold leaves of a small context.
    #[test]
    fn the_threshold_counts_the_replys_room_within_what_it_leaves() {
        assert_eq!(past(63_617, 16_384, 100_000, 80), Some(16_384));
        assert_eq!(past(63_616, 16_384, 100_000, 80), None);
        // An 8,192 context at 0.8 leaves a reply 1,639 tokens.
        assert_eq!(past(100, 16_384, 8_192, 80), None);
        assert_eq!(past(4_915, 16_384, 8_192, 80), Some(1_639));
        // At 1, the reply's whole room: compacted once it will not fit.
        assert_eq!(past(183_617, 16_384, 200_000, 100), Some(16_384));
        assert_eq!(past(183_616, 16_384, 200_000, 100), None);
        assert_eq!(past(8_192, 16_384, 8_192, 100), None);
        assert_eq!(past(8_193, 16_384, 8_192, 100), Some(0));
    }

    /// The model is sent a pruned result as a stub naming the tool, the
    /// path, the bytes and where `history_read` reads it; the rest as
    /// they were.
    #[test]
    fn a_pruned_result_is_sent_as_a_stub() {
        let mut events: Vec<Event> = Vec::new();
        events.extend(call(1, "a", "old.txt", 1000));
        events.extend(call(4, "b", "kept.txt", 10));
        events.push(event(7, Kind::Compaction { pruned: vec![3] }));
        let sent = crate::client::messages(&events, false);
        let results: Vec<String> = sent
            .iter()
            .filter_map(|m| td_json::parse_slice(m.as_bytes()).ok())
            .filter(|m| m.get("role").and_then(Json::as_str) == Some("tool"))
            .map(|m| m.get("content").and_then(Json::as_str).unwrap().to_string())
            .collect();
        assert_eq!(
            results,
            [
                "[The result of read_file for old.txt, 1000 bytes, was pruned when the conversation was compacted; history_read from 3 reads it.]".to_string(),
                "x".repeat(10),
            ]
        );
        // Before the compaction, as the request then was.
        let before = crate::client::messages(&events[..6], false);
        assert!(before.iter().any(|m| m.contains(&"x".repeat(1000))));
    }
}
