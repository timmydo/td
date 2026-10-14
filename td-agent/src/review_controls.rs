//! Review-only context limits and repeat diagnostics; full results stay in the trace.

use std::collections::BTreeMap;
use td_json::Json;

pub(crate) const OUTPUT_DEFAULT: usize = 8192;
pub(crate) const OUTPUT_MAX: usize = 65_536;
pub(crate) const CONTEXT_DEFAULT: usize = 512 * 1024;
pub(crate) const CONTEXT_MAX: usize = 8 * 1024 * 1024;

pub(crate) struct Controls {
    pub output_limit: usize,
    pub context_limit: usize,
    pub delivered: usize,
    pub calls: u64,
    repeats: BTreeMap<String, u64>,
}

impl Controls {
    pub fn new(options: &crate::review::Options) -> Self {
        Self {
            output_limit: options.tool_output_bytes.unwrap_or(OUTPUT_DEFAULT),
            context_limit: options.tool_context_bytes.unwrap_or(CONTEXT_DEFAULT),
            delivered: 0,
            calls: 0,
            repeats: BTreeMap::new(),
        }
    }

    pub fn arguments(&mut self, name: &str, arguments: &str) -> Result<(String, u64), String> {
        self.calls = self.calls.saturating_add(1);
        let mut value = td_json::parse(arguments).map_err(|e| e.to_string())?;
        if name == "grep" {
            if let Json::Obj(fields) = &mut value {
                if !fields.iter().any(|(key, _)| key == "extended") {
                    fields.push(("extended".into(), Json::Bool(true)));
                }
            }
        }
        if name == "read_file" {
            if let Json::Obj(fields) = &mut value {
                for (name, default) in [("limit", 128u64), ("offset", 1u64)] {
                    if !fields.iter().any(|(key, _)| key == name) {
                        fields.push((name.into(), Json::from(default)));
                    }
                }
            }
        }
        let signature = format!("{name}:{}", value.to_canonical());
        let repeated = self.repeats.entry(signature).or_default();
        *repeated = repeated.saturating_add(1);
        Ok((value.to_string(), *repeated))
    }

    pub fn remaining(&self) -> usize {
        self.context_limit.saturating_sub(self.delivered)
    }

    pub fn shortened(&self, full: &str, repeats: u64) -> bool {
        full.len().saturating_add(repeat_notice(repeats).len())
            > self.output_limit.min(self.remaining())
    }

    pub fn display(&mut self, full: &str, path: Option<&std::path::Path>, repeats: u64) -> String {
        let limit = self.output_limit.min(self.remaining());
        let mut suffix = repeat_notice(repeats);
        if full.len().saturating_add(suffix.len()) > limit {
            suffix.push_str(&format!("\n[Harness: output shortened for context. Full retained result: {}. Read selected sections or search that file only while tool context remains. Tool context remaining before this result: {} bytes.]", path.map_or_else(|| "session trace".into(), |p| p.display().to_string()), self.remaining()));
        }
        if suffix.len() > limit {
            suffix = [
                "[Harness: output shortened; tool context low. Finalize with limitations.]",
                "[context low]",
                "",
            ]
            .into_iter()
            .find(|notice| notice.len() <= limit)
            .unwrap_or_default()
            .to_string();
        }
        // Include all notices in the same byte allowance as the content.
        let allowance = limit.saturating_sub(suffix.len());
        let marker = "\n[... omitted ...]\n";
        let mut text = if full.len() > allowance && allowance >= marker.len() {
            let body = allowance - marker.len();
            let head = boundary(full, body / 3);
            let mut tail = full.len().saturating_sub(body - body / 3);
            while !full.is_char_boundary(tail) {
                tail = tail.saturating_add(1);
            }
            format!(
                "{}{}{}",
                full.get(..head).unwrap_or_default(),
                marker,
                full.get(tail..).unwrap_or_default()
            )
        } else {
            full.get(..boundary(full, allowance))
                .unwrap_or_default()
                .to_string()
        };
        text.push_str(&suffix);
        let end = boundary(&text, limit);
        text.truncate(end);
        self.delivered = self.delivered.saturating_add(text.len());
        text
    }

    /// Source windows keep consecutive complete lines, unlike process output.
    pub fn display_read(
        &mut self,
        full: &str,
        path: Option<&std::path::Path>,
        repeats: u64,
    ) -> String {
        let limit = self.output_limit.min(self.remaining());
        let repeat = repeat_notice(repeats);
        if full.len().saturating_add(repeat.len()) <= limit {
            return self.display(full, path, repeats);
        }
        let mut text = String::new();
        let mut next = None;
        for line in full.split_inclusive('\n') {
            let Some(number) = line
                .split_once('\t')
                .and_then(|(n, _)| n.trim().parse::<u64>().ok())
            else {
                break;
            };
            let notice = read_notice(number.saturating_add(1), false, path, &repeat);
            if text
                .len()
                .saturating_add(line.len())
                .saturating_add(notice.len())
                > limit
            {
                next = Some(number);
                break;
            }
            text.push_str(line);
            next = Some(number.saturating_add(1));
        }
        // A first line too large for the allowance must never be skipped.
        let notice = next.map_or_else(
            || "[Harness: read result does not fit; tool context low.]".into(),
            |n| read_notice(n, text.is_empty(), path, &repeat),
        );
        if text.len().saturating_add(notice.len()) <= limit {
            text.push_str(&notice);
            self.delivered = self.delivered.saturating_add(text.len());
            text
        } else {
            self.display(
                "[Harness: tool context low; finalize with limitations.]",
                None,
                repeats,
            )
        }
    }
}

fn read_notice(next: u64, empty: bool, path: Option<&std::path::Path>, repeat: &str) -> String {
    let action = if empty {
        format!("line {next} exceeds this view's allowance; no lines skipped. Use shell to inspect a smaller section of that line")
    } else {
        format!("source window shortened; read on with offset {next}")
    };
    let artifact = path.map_or(String::new(), |p| {
        format!(". Full retained result for searching: {}", p.display())
    });
    format!("[Harness: {action}{artifact}.]\n{repeat}")
}

fn repeat_notice(repeats: u64) -> String {
    if repeats > 1 {
        format!("\n[Harness: identical arguments used {repeats} times. Rerun only for changed inputs or a distinct hypothesis.]")
    } else {
        String::new()
    }
}

fn boundary(text: &str, limit: usize) -> usize {
    let mut cut = limit.min(text.len());
    while !text.is_char_boundary(cut) {
        cut = cut.saturating_sub(1);
    }
    cut
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn review_search_defaults_to_extended_but_keeps_explicit_basic() {
        let mut controls = Controls::new(&Default::default());
        let (args, _) = controls.arguments("grep", r#"{"pattern":"a|b"}"#).unwrap();
        assert_eq!(
            td_json::parse(&args).unwrap().get("extended"),
            Some(&Json::Bool(true))
        );
        let (args, _) = controls
            .arguments("grep", r#"{"pattern":"a|b","extended":false}"#)
            .unwrap();
        assert_eq!(
            td_json::parse(&args).unwrap().get("extended"),
            Some(&Json::Bool(false))
        );
    }

    #[test]
    fn source_pagination_has_no_middle_gap_and_names_the_first_unshown_line() {
        let mut controls = Controls::new(&crate::review::Options {
            tool_output_bytes: Some(1024),
            ..Default::default()
        });
        let full = (129..180)
            .map(|n| format!("{n:6}\t{}\n", "例🐈".repeat(10)))
            .collect::<String>();
        let text = controls.display_read(&full, None, 1);
        assert!(text.len() <= 1024);
        assert!(!text.contains("omitted"));
        let last = text
            .lines()
            .filter_map(|s| {
                s.split_once('\t')
                    .and_then(|(n, _)| n.trim().parse::<u64>().ok())
            })
            .next_back()
            .unwrap();
        assert!(text.contains(&format!("read on with offset {}", last + 1)));
        assert!(!text.contains(&format!("{}\t", last + 1)));
        assert!(text.starts_with("   129\t"));
        let first = controls.delivered;
        let tail = controls.display_read(&full, None, 1);
        assert_eq!(controls.delivered, first + tail.len());
    }

    #[test]
    fn oversized_first_line_names_the_artifact_without_skipping_or_finalizing() {
        let mut controls = Controls::new(&crate::review::Options {
            tool_output_bytes: Some(1024),
            ..Default::default()
        });
        let text = controls.display_read(
            &format!("    19\t{}\n", "x".repeat(2000)),
            Some(std::path::Path::new("/scratch/full")),
            1,
        );
        assert!(text.contains("line 19") && text.contains("/scratch/full"));
        assert!(text.contains("no lines skipped") && text.contains("Use shell"));
        assert!(!text.contains("finalize") && !text.contains("offset 20"));
        assert!(text.len() <= 1024);
    }

    #[test]
    fn unicode_and_notices_share_per_call_and_total_limits() {
        let options = crate::review::Options {
            tool_output_bytes: Some(1024),
            tool_context_bytes: Some(1800),
            ..Default::default()
        };
        let mut control = Controls::new(&options);
        let full = "例🐈".repeat(1000);
        let first = control.display(&full, Some(std::path::Path::new("/scratch/result")), 2);
        assert!(first.len() <= 1024);
        assert!(first.contains("identical arguments") && first.contains("output shortened"));
        let next = control.display(&full, None, 1);
        assert!(next.len() <= 1800 - first.len());
        assert!(control.remaining() < 8);
        control.delivered = control.context_limit;
        assert!(control.display(&full, None, 1).is_empty());
    }

    #[test]
    fn repeat_detection_normalizes_json_and_default_read_windows() {
        let mut controls = Controls::new(&Default::default());
        assert_eq!(
            controls
                .arguments("read_file", r#"{"path":"/a"}"#)
                .unwrap()
                .1,
            1
        );
        assert_eq!(
            controls
                .arguments("read_file", r#"{ "limit":128, "path":"/a", "offset":1 }"#)
                .unwrap()
                .1,
            2
        );
        assert_eq!(
            controls
                .arguments("read_file", r#"{"path":"/a","offset":129}"#)
                .unwrap()
                .1,
            1
        );
    }
    #[test]
    fn tiny_remaining_allowances_do_not_emit_broken_artifact_paths() {
        for remaining in [0, 7, 20, 100] {
            let mut controls = Controls::new(&Default::default());
            controls.delivered = controls.context_limit - remaining;
            let result = controls.display(
                &"x".repeat(1000),
                Some(std::path::Path::new("/scratch/full-result")),
                2,
            );
            assert!(result.len() <= remaining);
            assert!(!result.contains("/scratch"));
            if result.contains('[') {
                assert!(result.ends_with(']'));
            }
        }
    }
    #[test]
    fn shortened_results_keep_failure_summaries_in_the_tail() {
        let options = crate::review::Options {
            tool_output_bytes: Some(1024),
            ..Default::default()
        };
        let mut controls = Controls::new(&options);
        let full = format!(
            "[exit status 101]\n{}\nFAILURE_SUMMARY",
            "build output\n".repeat(1000)
        );
        let result = controls.display(&full, Some(std::path::Path::new("/scratch/result")), 1);
        assert!(result.starts_with("[exit status 101]"));
        assert!(result.contains("FAILURE_SUMMARY") && result.contains("[... omitted ...]"));
        assert!(result.len() <= 1024);
    }
}
