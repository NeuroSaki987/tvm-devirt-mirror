//! The `unresolved` command: per-block diagnostics for a function that did not close.
//!
//! Recovery reports a failure as a sentence attached to a block. This command runs the
//! same recovery with diagnostics switched on and writes the block list of the returned
//! graph next to everything that was still known about each failure when it happened: the
//! destination expression, the candidate predicates and what pinning each of them folds
//! to, the values a narrow index enumerates and where each of them lands.
//!
//! The graph is validated at the same time, because a failure count means nothing if the
//! graph around it is malformed.

use crate::binary::pe;
use crate::cli::fmt::resolve_start;
use crate::ir::{self, Terminator};
use crate::vm::{self, diag, explore};
use anyhow::Result;
use std::fmt::Write as _;
use std::path::Path;

pub fn cmd_unresolved(
    path: &Path,
    va: u64,
    stack_base: u64,
    steps: usize,
    max_blocks: usize,
    timeout: u64,
    json: Option<&Path>,
) -> Result<()> {
    let pe = pe::PeFile::load(path)?;
    let start = resolve_start(&pe, va);
    let mut ex = explore::Explorer::new(&pe, stack_base);
    ex.step_budget = steps;
    ex.block_budget = max_blocks;
    ex.time_budget = std::time::Duration::from_secs(timeout);
    diag::begin(start);
    let cfg = ex.recover(start);
    let sink = diag::take();
    let issues = explore::validate(&cfg);
    let non_executable = non_executable_targets(&pe, &cfg);

    let text = render_json(
        &pe,
        start,
        stack_base,
        &cfg,
        &sink,
        &issues,
        &non_executable,
    );
    match json {
        Some(p) => {
            std::fs::write(p, text.as_bytes())?;
            println!(
                "recovered {} blocks ({} unresolved) in {:.0?}; issues: {}; wrote {}",
                cfg.blocks.len(),
                count_unresolved(&cfg),
                cfg.total_steps,
                issue_summary(&issues, &non_executable),
                p.display()
            );
        }
        None => print!("{text}"),
    }
    Ok(())
}

fn count_unresolved(cfg: &ir::Cfg) -> usize {
    cfg.blocks
        .iter()
        .filter(|b| matches!(&b.terminator, Terminator::Unresolved { .. }))
        .count()
}

/// Block identities a terminator can hand control to.
fn targets(t: &Terminator) -> Vec<ir::BlockId> {
    match t {
        Terminator::Jump(b) => vec![*b],
        Terminator::Branch {
            taken, not_taken, ..
        } => vec![*taken, *not_taken],
        Terminator::Switch { targets, .. } | Terminator::AdoptedSwitch { targets } => {
            targets.clone()
        }
        Terminator::Backedge { target } => vec![*target],
        Terminator::Adopted { taken, not_taken } => vec![*taken, *not_taken],
        _ => Vec::new(),
    }
}

fn hex(v: u64) -> String {
    format!("\"0x{v:x}\"")
}

fn opt_hex(v: Option<u64>) -> String {
    v.map_or_else(|| "null".to_string(), hex)
}

fn q(s: &str) -> String {
    format!("\"{}\"", diag::escape(s))
}

fn block_id_json(b: &ir::BlockId) -> String {
    format!(
        "{{\"handler\":{},\"vip\":{}}}",
        hex(b.handler),
        opt_hex(b.vip)
    )
}

fn block_list(v: &[ir::BlockId]) -> String {
    let items: Vec<String> = v.iter().map(block_id_json).collect();
    format!("[{}]", items.join(","))
}

fn non_executable_targets(pe: &pe::PeFile, cfg: &ir::Cfg) -> Vec<ir::BlockId> {
    let mut out = Vec::new();
    for block in &cfg.blocks {
        for target in targets(&block.terminator) {
            if !pe.is_executable(target.handler) && !out.contains(&target) {
                out.push(target);
            }
        }
    }
    out
}

fn issue_summary(issues: &explore::CfgIssues, non_executable: &[ir::BlockId]) -> String {
    if non_executable.is_empty() {
        return issues.summary();
    }
    let executable = format!("non_executable_targets {}", non_executable.len());
    if issues.is_clean() {
        executable
    } else {
        format!("{}, {executable}", issues.summary())
    }
}

fn split_for_block<'a>(sink: &'a diag::Sink, id: &ir::BlockId) -> Option<&'a diag::SplitDiag> {
    sink.splits.iter().rev().find(|record| {
        record.pass == sink.final_pass && record.handler == id.handler && record.vip == id.vip
    })
}

fn stop_for_block<'a>(sink: &'a diag::Sink, id: &ir::BlockId) -> Option<&'a diag::StopDiag> {
    sink.stops.iter().rev().find(|record| {
        record.pass == sink.final_pass && record.handler == id.handler && record.vip == id.vip
    })
}

fn render_json(
    pe: &pe::PeFile,
    start: u64,
    stack_base: u64,
    cfg: &ir::Cfg,
    sink: &diag::Sink,
    issues: &explore::CfgIssues,
    non_executable: &[ir::BlockId],
) -> String {
    let (vm_lo, vm_hi) = pe
        .section_by_name(vm::DEFAULT_VM_SECTION)
        .map(|s| {
            let lo = pe.rva_to_va(s.virtual_address);
            (lo, lo + s.virtual_size.max(s.raw_size) as u64)
        })
        .unwrap_or((0, 0));
    let in_vm = |t: u64| vm_lo != 0 && t >= vm_lo && t < vm_hi;

    let mut s = String::with_capacity(1 << 16);
    let _ = write!(
        s,
        "{{\"entry\":{},\"stack_base\":{},\"blocks\":{},\"unresolved_blocks\":{},\"total_steps\":{},\"timed_out\":{},",
        hex(start),
        hex(stack_base),
        cfg.blocks.len(),
        count_unresolved(cfg),
        cfg.total_steps,
        cfg.timed_out
    );
    let _ = write!(
        s,
        "\"vm_section\":{{\"lo\":{},\"hi\":{}}},",
        hex(vm_lo),
        hex(vm_hi)
    );
    let _ = write!(s, "\"issues\":{},", issues_json(issues, non_executable));
    let _ = write!(s, "\"unresolved\":[");

    let mut first = true;
    for b in &cfg.blocks {
        let Terminator::Unresolved { reason } = &b.terminator else {
            continue;
        };
        if !first {
            s.push(',');
        }
        first = false;
        // The latest pass is the one that produced the returned graph, so its record is
        // the one that describes this block.
        let split = split_for_block(sink, &b.id);
        let stop = stop_for_block(sink, &b.id);
        let _ = write!(
            s,
            "{{\"handler\":{},\"vip\":{},\"reason\":{},\"cost\":{},\"preds\":{},",
            hex(b.id.handler),
            opt_hex(b.id.vip),
            q(reason),
            b.cost,
            block_list(&b.preds)
        );
        let _ = write!(
            s,
            "\"handler_executable\":{},\"handler_in_vm_section\":{},",
            pe.is_executable(b.id.handler),
            in_vm(b.id.handler)
        );
        match stop {
            Some(d) => {
                let _ = write!(
                    s,
                    "\"stop\":{},\"site\":{},\"stop_pass\":{},",
                    q(&d.stop),
                    hex(d.site),
                    d.pass
                );
            }
            None => {
                let _ = write!(s, "\"stop\":null,\"site\":null,\"stop_pass\":null,");
            }
        }
        match split {
            Some(d) => {
                let _ = write!(s, "\"detail\":{}", split_json(d));
            }
            None => {
                let _ = write!(s, "\"detail\":null");
            }
        }
        s.push('}');
    }
    s.push_str("],");
    // Kept in its own array: these describe the folding budget rather than an
    // unresolved branch, and merging the two would hide which mechanism a count
    // belongs to.
    let _ = write!(s, "\"divergence\":{}", divergence_json(&sink.diverged));
    s.push('}');
    s
}

/// The folding-budget records, as JSON.
fn divergence_json(v: &[diag::DivergedDiag]) -> String {
    let mut s = String::from("[");
    for (i, d) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"pass\":{},\"handler\":{},\"vip\":{},\"site\":{},\"nodes\":{},\"dag_total\":{},\"live_dag\":{},\"edges\":{},\"shared\":{},\"max_indegree\":{},",
            d.pass,
            hex(d.handler),
            opt_hex(d.vip),
            hex(d.site),
            d.nodes,
            d.dag_total,
            d.live_dag,
            d.edges,
            d.shared,
            d.max_indegree
        );
        let _ = write!(s, "\"by_kind\":{},", count_map(&d.by_kind));
        let _ = write!(s, "\"by_width\":{},", count_map(&d.by_width));
        let _ = write!(s, "\"top_shared\":{}", count_map(&d.top_shared));
        s.push('}');
    }
    s.push(']');
    s
}

/// A list of (label, count) pairs as a JSON object.
fn count_map(v: &[(String, usize)]) -> String {
    // Shallow rendering can give distinct nodes the same label. Merge those labels before
    // emitting an object so JSON consumers never see duplicate keys and silently discard
    // one of the counts.
    let mut unique: Vec<(&str, usize)> = Vec::new();
    for (key, count) in v {
        if let Some((_, total)) = unique.iter_mut().find(|(seen, _)| *seen == key) {
            *total += count;
        } else {
            unique.push((key, *count));
        }
    }
    let mut s = String::from("{");
    for (i, (k, n)) in unique.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{}:{}", q(k), n);
    }
    s.push('}');
    s
}

fn issues_json(issues: &explore::CfgIssues, non_executable: &[ir::BlockId]) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        "{{\"summary\":{},\"clean\":{},\"dead_params\":{},",
        q(&issue_summary(issues, non_executable)),
        issues.is_clean() && non_executable.is_empty(),
        issues.dead_params
    );
    let edges: Vec<String> = issues
        .dangling_edges
        .iter()
        .map(|(a, b)| format!("[{},{}]", block_id_json(a), block_id_json(b)))
        .collect();
    let _ = write!(s, "\"dangling_edges\":[{}],", edges.join(","));
    let _ = write!(
        s,
        "\"unidentified_blocks\":{},",
        block_list(&issues.unidentified_blocks)
    );
    let _ = write!(s, "\"orphans\":{},", block_list(&issues.orphans));
    let _ = write!(
        s,
        "\"duplicate_ids\":{},",
        block_list(&issues.duplicate_ids)
    );
    let _ = write!(
        s,
        "\"degenerate_branches\":{},",
        block_list(&issues.degenerate_branches)
    );
    let _ = write!(
        s,
        "\"cannot_reach_exit\":{},",
        block_list(&issues.cannot_reach_exit)
    );
    let nd: Vec<String> = issues
        .nondominating_params
        .iter()
        .map(|(b, r)| format!("[{},{}]", block_id_json(b), r.0))
        .collect();
    let _ = write!(s, "\"nondominating_params\":[{}],", nd.join(","));
    let _ = write!(
        s,
        "\"non_executable_targets\":{}",
        block_list(non_executable)
    );
    s.push('}');
    s
}

fn split_json(d: &diag::SplitDiag) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        "{{\"pass\":{},\"site\":{},\"arg_dependent\":{},\"leaf_total\":{},\"dest_width\":{},",
        d.pass,
        hex(d.site),
        d.arg_dependent,
        d.leaf_total,
        q(&d.dest_width)
    );
    let _ = write!(s, "\"dest_expr\":{},", q(&d.dest_expr));
    let leaves: Vec<String> = d.leaves.iter().map(|l| q(l)).collect();
    let _ = write!(s, "\"leaves\":[{}],", leaves.join(","));
    let _ = write!(s, "\"flag_candidates\":[");
    for (i, c) in d.flag_candidates.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"expr\":{},\"prior_pin\":{},",
            q(&c.expr),
            opt_hex(c.prior_pin)
        );
        let _ = write!(s, "\"arms\":[");
        for (j, a) in c.arms.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            let _ = write!(
                s,
                "{{\"value\":{},\"target\":{},\"executable\":{},\"in_vm_section\":{},\"vip\":{},\"naive_target\":{}}}",
                a.value,
                opt_hex(a.target),
                a.executable,
                a.in_vm_section,
                opt_hex(a.vip),
                opt_hex(a.naive_target)
            );
        }
        s.push_str("]}");
    }
    s.push_str("],");
    let _ = write!(s, "\"narrow_indices\":[");
    for (i, n) in d.narrow_indices.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"expr\":{},\"span\":{},\"unfolded\":{},\"non_executable_arms\":{},\"arms\":[",
            q(&n.expr),
            n.span,
            n.unfolded,
            n.non_executable_arms
        );
        for (j, a) in n.arms.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            let _ = write!(
                s,
                "{{\"value\":{},\"target\":{},\"executable\":{},\"in_vm_section\":{},\"vip\":{}}}",
                a.value,
                opt_hex(a.target),
                a.executable,
                a.in_vm_section,
                opt_hex(a.vip)
            );
        }
        s.push_str("]}");
    }
    s.push_str("]}");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_executable_target_makes_the_combined_validation_fail() {
        let issues = explore::CfgIssues::default();
        let target = ir::BlockId {
            handler: 0x1234,
            vip: None,
        };
        let json = issues_json(&issues, &[target]);

        assert!(json.contains("\"clean\":false"), "{json}");
        assert!(json.contains("non_executable_targets 1"), "{json}");
    }

    #[test]
    fn duplicate_rendered_count_keys_are_merged() {
        let json = count_map(&[("same".into(), 2), ("same".into(), 3)]);
        assert_eq!(json, "{\"same\":5}");
    }

    #[test]
    fn a_final_pass_block_never_inherits_a_first_pass_diagnostic() {
        let id = ir::BlockId {
            handler: 0x1234,
            vip: Some(0x40),
        };
        let sink = diag::Sink {
            final_pass: 2,
            stops: vec![diag::StopDiag {
                pass: 1,
                handler: id.handler,
                vip: id.vip,
                ..Default::default()
            }],
            ..Default::default()
        };

        assert!(stop_for_block(&sink, &id).is_none());
    }
}
