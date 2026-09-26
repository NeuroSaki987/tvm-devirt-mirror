//! CFG recovery by state forking at virtualized branches. Evaluation of a single path stops as soon as an indirect branch depends on a guest value.

use crate::binary::pe::PeFile;
use crate::ir::expr::render;
use crate::ir::expr::{self, Arena, Op, Ref, Reg, Width};
use crate::ir::lift::{Emulator, Event, Stop};
use crate::ir::{Block, BlockId, Cfg, Terminator, terminator_kind};
use std::collections::{HashMap, HashSet};

/// A pending continuation: a live evaluator plus where it resumes.
struct Task<'a> {
    emu: Emulator<'a>,
    /// Address to resume interpretation at.
    resume: u64,
    /// Identity of the block this continuation builds.
    id: BlockId,
    /// Predecessor block, for edge bookkeeping.
    from: Option<BlockId>,
    /// Blocks on the path from the function entry to this task. Only a jump back
    /// to an *ancestor* is a loop; a jump to an already-recovered block that is
    /// not an ancestor is ordinary convergence (the join of an if/else diamond).
    ancestors: Vec<BlockId>,
}

/// Run a block while keeping its entry parameters valid across arena compaction.
fn run_retaining_entry_params(
    emu: &mut Emulator<'_>,
    start: u64,
    budget: usize,
    entry_params: &mut [(Reg, Ref)],
) -> Stop {
    let mut retained: Vec<Ref> = entry_params.iter().map(|(_, value)| *value).collect();
    let stop = emu.run_retaining(start, budget, &mut retained);
    for ((_, value), remapped) in entry_params.iter_mut().zip(retained) {
        *value = remapped;
    }
    stop
}

#[cfg(test)]
mod divergence_root_tests {
    use super::*;
    use crate::binary::pe::Section;
    use crate::ir::expr::{Arena, Reg, Width};
    use crate::vm::state::State;
    use std::collections::HashSet;

    #[test]
    fn divergence_roots_include_state_and_active_pins() {
        let mut arena = Arena::new();
        let mut state = State::new(&mut arena, 0x7fff_ffff_0000);
        let reg = arena.opaque("live-reg", Width::W64);
        let pin = arena.opaque("live-pin", Width::W8);
        state.regs.clear();
        state.regs.insert(Reg::Rax, reg);

        let roots: HashSet<_> = divergence_roots(&state, &[(pin, 1)], &[])
            .into_iter()
            .collect();
        assert!(roots.contains(&reg));
        assert!(roots.contains(&pin));
    }

    #[test]
    fn compaction_preserves_overwritten_block_entry_parameters_for_grafting() {
        const IMAGE_BASE: u64 = 0x1400_0000_0;
        const START: u64 = IMAGE_BASE + 0x1000;
        let mut data = vec![0u8; 0x400];
        data[0x200] = 0xc3;
        let pe = PeFile {
            data,
            image_base: IMAGE_BASE,
            entry_point_rva: 0x1000,
            size_of_image: 0x2000,
            sections: vec![Section {
                name: ".text".into(),
                virtual_address: 0x1000,
                virtual_size: 0x100,
                raw_address: 0x200,
                raw_size: 0x100,
                characteristics: 0x2000_0000,
            }],
            opt_header_offset: 0,
            section_table_offset: 0,
            file_alignment: 0x200,
            section_alignment: 0x1000,
            size_of_headers: 0x200,
            loader_bound: Vec::new(),
        };
        let mut emu = Emulator::new(&pe, 0x7fff_ffff_0000);
        let parameter = emu.arena.param(expr::BlockRef(4), Reg::R12);
        let mut entry_params = vec![(Reg::R12, parameter)];
        emu.state.set_reg(Reg::R12, parameter);
        let replacement = emu.arena.constant(7, Width::W64);
        emu.state.set_reg(Reg::R12, replacement);
        for _ in 0..64 {
            emu.arena.opaque("dead-history", Width::W64);
        }
        emu.node_limit = emu.arena.len() - 1;
        let before_hash = emu.arena.structural_hash(parameter, 8);

        let stop = run_retaining_entry_params(&mut emu, START, 1, &mut entry_params);

        assert!(matches!(stop, Stop::Return { .. }));
        let remapped = entry_params[0].1;
        assert_eq!(emu.arena.structural_hash(remapped, 8), before_hash);
        assert_eq!(
            emu.arena.op(remapped),
            &Op::Param(expr::BlockRef(4), Reg::R12)
        );
        let mut cfg_arena = Arena::new();
        let grafted = cfg_arena.graft(&emu.arena, remapped, &mut HashMap::new());
        assert_eq!(
            cfg_arena.op(grafted),
            &Op::Param(expr::BlockRef(4), Reg::R12)
        );
    }
}

pub struct Explorer<'a> {
    pe: &'a PeFile,
    stack_base: u64,
    /// Instruction budget per block.
    pub step_budget: usize,
    /// Maximum blocks to recover.
    pub block_budget: usize,
    /// Wall-clock limit for the whole recovery. Guards against a single path
    /// that keeps making progress without ever terminating.
    pub time_budget: std::time::Duration,
    /// Total interpreted instructions allowed across the whole function.
    pub work_budget: usize,

    /// Print progress as blocks are recovered.
    pub verbose: bool,

    /// Name of the section holding VM bytecode.
    vm_section: String,
}

impl<'a> Explorer<'a> {
    pub fn new(pe: &'a PeFile, stack_base: u64) -> Self {
        Self {
            pe,
            stack_base,
            step_budget: 500_000,
            block_budget: 512,
            time_budget: std::time::Duration::from_secs(120),
            work_budget: 24_000_000,
            verbose: false,
            vm_section: crate::vm::DEFAULT_VM_SECTION.to_string(),
        }
    }

    /// Recover from a VM section other than `.tvm0`.
    pub fn with_vm_section(pe: &'a PeFile, stack_base: u64, vm_section: &str) -> Self {
        Self {
            vm_section: vm_section.to_string(),
            ..Self::new(pe, stack_base)
        }
    }

    /// Recover the CFG of the virtualized function entered at `start`. Runs recovery twice. The first pass discovers the graph shape; the second re-runs it knowing which blocks are joins, and cuts SSA parameters only there.
    pub fn recover(&self, start: u64) -> Cfg {
        // Derive the VIP slot offset once, here, and use the same answer for both
        // passes. Block identity is `(handler, vip)`, so an offset that differed
        // between passes would leave the second unable to find the joins the first
        // reported, and `adopt_known` would match nothing.
        let vip_slot = {
            let mut probe = Emulator::with_vm_section(self.pe, self.stack_base, &self.vm_section);
            probe.discover_vip_slot(start, Self::VIP_PROBE_STEPS)
        };
        let first = self.recover_pass(start, &HashSet::new(), vip_slot);
        let joins: HashSet<BlockId> = first
            .blocks
            .iter()
            .filter(|b| b.preds.len() > 1)
            .map(|b| b.id)
            .collect();
        if joins.is_empty() {
            return first;
        }
        let mut cfg = self.recover_pass_with(start, &joins, Some(&first), vip_slot);
        cfg.trivial_phis_removed = cfg.simplify_phis();
        cfg.remove_dead_params();
        cfg
    }

    pub const VIP_PROBE_STEPS: usize = 20_000;

    fn recover_pass(&self, start: u64, cut_at: &HashSet<BlockId>, vip_slot: Option<u64>) -> Cfg {
        self.recover_pass_with(start, cut_at, None, vip_slot)
    }

    /// One recovery pass, optionally guided by the structure an earlier pass found. `known` decouples control-flow recovery from data-flow recovery.
    fn recover_pass_with(
        &self,
        start: u64,
        cut_at: &HashSet<BlockId>,
        known: Option<&Cfg>,
        vip_slot: Option<u64>,
    ) -> Cfg {
        // Pass 2 re-runs recovery with the joins cut and the concrete pass as a guide;
        // the graph it returns is the one callers see, so a failure has to be
        // attributable to the pass that produced it.
        crate::vm::diag::set_pass(if known.is_some() { 2 } else { 1 });
        let entry_id = BlockId {
            handler: start,
            vip: None,
        };
        // Long-lived arena for the recovered CFG. Blocks graft into this as they
        // are finalized.
        let mut arena = Arena::default();
        let mut blocks: Vec<Block> = Vec::new();
        let mut visited: HashSet<BlockId> = HashSet::new();
        let mut edges: Vec<(BlockId, BlockId)> = Vec::new();
        let mut back_edges: Vec<(BlockId, BlockId)> = Vec::new();
        let mut total_steps = 0usize;
        let mut timed_out = false;
        let mut unresolved = 0usize;

        let mut queue: Vec<Task> = vec![Task {
            emu: {
                let mut e = Emulator::with_vm_section(self.pe, self.stack_base, &self.vm_section);
                // Forks clone the evaluator, so setting this on the seed reaches every
                // block in the pass.
                e.vip_slot = vip_slot;
                e
            },
            resume: start,
            id: entry_id,
            from: None,
            ancestors: Vec::new(),
        }];

        let pass_started = std::time::Instant::now();

        while let Some(mut task) = queue.pop() {
            // The work budget is the deterministic limit and should be the one that
            // fires. If the timer wins the race the result depends on machine load,
            // so say so rather than silently emitting different code than last run.
            let out_of_work = total_steps >= self.work_budget;
            let recovery_elapsed = pass_started
                .elapsed()
                .saturating_sub(crate::vm::diag::excluded_time());
            let out_of_time = recovery_elapsed >= self.time_budget;
            if out_of_time && !out_of_work {
                timed_out = true;
            }
            if out_of_work || out_of_time {
                unresolved += 1;
                blocks.push(Block {
                    id: task.id,
                    block_ref: expr::BlockRef(blocks.len() as u32),
                    params: Vec::new(),
                    events: Vec::new(),
                    from_vm_context: false,
                    exit_regs: Vec::new(),
                    terminator: Terminator::Unresolved {
                        reason: "time budget reached".into(),
                    },
                    cost: 0,
                    preds: Vec::new(),
                });
                break;
            }
            if let Some(p) = task.from {
                edges.push((p, task.id));
            }
            if !visited.insert(task.id) {
                if let Some(p) = task.from {
                    back_edges.push((p, task.id));
                }
                continue;
            }
            if blocks.len() >= self.block_budget {
                unresolved += 1;
                blocks.push(Block {
                    id: task.id,
                    block_ref: expr::BlockRef(blocks.len() as u32),
                    params: Vec::new(),
                    events: Vec::new(),
                    from_vm_context: false,
                    exit_regs: Vec::new(),
                    terminator: Terminator::Unresolved {
                        reason: "block budget reached".into(),
                    },
                    cost: 0,
                    preds: Vec::new(),
                });
                continue;
            }

            if self.verbose {
                eprintln!("  -> evaluating {} (resume {:#x})", task.id, task.resume);
            }

            let t_block = std::time::Instant::now();
            let before = task.emu.steps;
            // Only ancestors are legitimate back-edge targets. Using "every block recovered so far" would misreport the join of an if/else diamond as a loop, since the join is reached twice without ever being an ancestor of itself.
            task.emu.clear_visited();
            for a in &task.ancestors {
                task.emu.mark_visited(a.handler, a.vip);
            }
            task.emu.begin_block();
            // Replace the guest register image with this block's SSA parameters before any of its instructions run, so everything the block computes is expressed in terms of its own entry state rather than reaching back to function entry through whichever single path got here.
            let block_ref = expr::BlockRef(blocks.len() as u32);
            let mut entry_params = if cut_at.contains(&task.id) {
                task.emu.seed_guest_params(block_ref)
            } else {
                Vec::new()
            };

            let stop = run_retaining_entry_params(
                &mut task.emu,
                task.resume,
                self.step_budget,
                &mut entry_params,
            );
            // Recorded before `stop` is consumed below.
            let returning = matches!(stop, Stop::Return { .. });
            let cost = task.emu.steps - before;
            total_steps += cost;

            // Locate the context *before* taking the events. The entry block builds the context as it runs, so while those stores were being recorded there was no known VM region to compare them against and they were provisionally tagged as guest accesses.
            task.emu.locate_guest_context();
            let events = std::mem::take(&mut task.emu.events);

            // Locate the guest register image *before* the terminator is built.
            task.emu.locate_guest_context();

            // Diagnostics: which stop kind ended this block, and where. Gated so it
            // costs nothing in a normal run. The unresolved diagnostic needs the same
            // two facts, so one computation serves both consumers.
            let debug_stops = std::env::var_os("TVM_DEBUG_STOPS").is_some();
            let diag_on = crate::vm::diag::is_on();
            let stop_tag = if debug_stops || diag_on {
                Some(stop_kind(&stop))
            } else {
                None
            };
            let stop_site = stop_site(&stop);
            let terminator = match stop {
                Stop::Return { dest, .. } => match task.emu.arena.as_const(dest) {
                    // The VM leaves a function by jumping to the concrete
                    // address of the next routine; a symbolic destination is a
                    // real return to the caller.
                    Some(t) if self.pe.is_executable(t) => Terminator::TailCall { target: t },
                    _ => Terminator::Return { dest },
                },
                Stop::SymbolicBranch { site, dest } => {
                    {
                        let t = self.split(&mut task, site, dest, &mut queue);
                        // Fall back to the structure the concrete pass found. The
                        // cut can only *lose* folding power, never gain it, so a
                        // failure here is an artefact of symbolizing guest state
                        // rather than a genuinely unresolvable branch.
                        match t {
                            Terminator::Unresolved { .. } => self
                                .adopt_known(&mut task, known, &mut queue, cost)
                                .unwrap_or(t),
                            t => t,
                        }
                    }
                }
                // A native JCC needs no folding: fork the state on the predicate
                // and continue down both statically-known edges.
                Stop::NativeBranch {
                    predicate,
                    taken,
                    not_taken,
                    ..
                } => {
                    let mut ancestors = task.ancestors.clone();
                    ancestors.push(task.id);
                    let mut f_taken = task.emu.clone();
                    f_taken.pins.push((predicate, 1));
                    let mut f_not = task.emu.clone();
                    f_not.pins.push((predicate, 0));

                    let id_t = BlockId {
                        handler: taken,
                        vip: f_taken.current_vip(),
                    };
                    let id_n = BlockId {
                        handler: not_taken,
                        vip: f_not.current_vip(),
                    };
                    queue.push(Task {
                        emu: f_taken,
                        resume: taken,
                        id: id_t,
                        from: Some(task.id),
                        ancestors: ancestors.clone(),
                    });
                    queue.push(Task {
                        emu: f_not,
                        resume: not_taken,
                        id: id_n,
                        from: Some(task.id),
                        ancestors,
                    });
                    Terminator::Branch {
                        predicate,
                        taken: id_t,
                        not_taken: id_n,
                    }
                }
                Stop::Backedge { target, vip, .. } => {
                    let id = BlockId {
                        handler: target,
                        vip,
                    };
                    edges.push((task.id, id));
                    back_edges.push((task.id, id));
                    Terminator::Backedge { target: id }
                }
                Stop::Unsupported { site, text, .. } => Terminator::Unresolved {
                    reason: format!("unsupported at {site:#x}: {text}"),
                },
                Stop::Unreadable { site } => Terminator::Unresolved {
                    reason: format!("unreadable at {site:#x}"),
                },
                Stop::Budget { site } => Terminator::Unresolved {
                    reason: format!("budget exhausted at {site:#x}"),
                },
                Stop::OutOfImage { site } => Terminator::Unresolved {
                    reason: format!("left image at {site:#x}"),
                },
                // Preserve the expression for diagnostics, but do not replace the
                // observed non-executable outcome with forced executable arms. Doing
                // so would silently discard a path already proved reachable by this
                // task. Closing it safely needs an IR shape that retains that unresolved
                // outcome alongside any executable alternatives.
                Stop::BadTarget { site, dest, target } => {
                    if diag_on {
                        crate::vm::diag::measure(|| {
                            let candidates = flag_candidates(&mut task.emu.arena, dest);
                            let leaves = expr::leaves(&task.emu.arena, dest);
                            let arg_dependent = leaves
                                .iter()
                                .any(|leaf| matches!(task.emu.arena.op(*leaf), Op::InitReg(_)));
                            let enumerated = narrow_indices(&mut task.emu.arena, dest);
                            self.record_split_diag(
                                &mut task,
                                site,
                                dest,
                                &candidates,
                                &leaves,
                                arg_dependent,
                                &enumerated,
                            );
                        });
                    }
                    Terminator::Unresolved {
                        reason: format!("left image at {site:#x} (folded to {target:#x})"),
                    }
                }
                // The call is already recorded as an event; the block simply has
                // no successor. Modelled as a return with no exit values, which is
                // what the emitted code does: make the call, then nothing.
                Stop::NoReturn { .. } => {
                    // No destination exists, so the `dest` is an opaque rather than
                    // a fabricated address. Recovery treats this as a return, which
                    // is what the emitted code does: make the call, then stop.
                    let d = task.emu.arena.opaque("noreturn", Width::W64);
                    Terminator::Return { dest: d }
                }
                Stop::Diverged { site, nodes } => {
                    // The fork's arena dies with this block, so the composition has to
                    // be taken here or not at all. Gated: it walks the whole DAG.
                    if diag_on {
                        // The live roots: what the machine still holds, plus this block's
                        // guest-visible effects. A node unreachable from these cannot
                        // influence any later result, so the gap between this count and the
                        // arena size is the mass a trimming rule would be discarding -- or
                        // reclaiming, if it is provably dead.
                        crate::vm::diag::measure(|| {
                            let roots = divergence_roots(&task.emu.state, &task.emu.pins, &events);
                            let live = task.emu.arena.reachable_from(&roots);
                            let c = task.emu.arena.composition();
                            crate::vm::diag::record_diverged(crate::vm::diag::DivergedDiag {
                                entry: 0,
                                pass: 0,
                                handler: task.id.handler,
                                vip: task.id.vip,
                                site,
                                nodes,
                                dag_total: c.total,
                                by_kind: c.by_kind,
                                by_width: c.by_width,
                                edges: c.edges,
                                shared: c.shared,
                                max_indegree: c.max_indegree,
                                top_shared: c.top_shared,
                                live_dag: live,
                            });
                        });
                    }
                    Terminator::Unresolved {
                        reason: format!("folding diverged at {site:#x} ({nodes} DAG nodes)"),
                    }
                }
            };

            if let Terminator::Unresolved { reason } = &terminator {
                unresolved += 1;
                if debug_stops && let Some(tag) = stop_tag {
                    eprintln!("  [unresolved] stop={tag} :: {reason}");
                }
                if diag_on {
                    crate::vm::diag::record_stop(crate::vm::diag::StopDiag {
                        entry: 0,
                        pass: 0,
                        handler: task.id.handler,
                        vip: task.id.vip,
                        stop: stop_tag.unwrap_or("no-stop").to_string(),
                        site: stop_site,
                    });
                }
            }
            if self.verbose {
                eprintln!(
                    "     {} -> {} ({} steps, {:.1?})",
                    task.id,
                    terminator_kind(&terminator),
                    cost,
                    t_block.elapsed()
                );
            }

            // Graft everything this block references out of the fork's arena and
            // into the CFG's own, so the block stays valid after the fork dies.
            // A fresh memo per block is required: `src` differs between blocks,
            // and a `Ref` means nothing across arenas.
            let mut memo: HashMap<Ref, Ref> = HashMap::new();
            let src = &task.emu.arena;

            // Cut the block's expressions at its own entry by seeding the graft memo with `entry value -> parameter`. Constant entry values are deliberately *not* cut.
            let mut params: Vec<(crate::ir::expr::Reg, Ref)> = entry_params
                .iter()
                .map(|(r, p)| (*r, arena.graft(src, *p, &mut memo)))
                .collect();
            params.sort_by_key(|(r, _)| *r as u8);

            let events: Vec<Event> = events
                .into_iter()
                .map(|e| graft_event(&mut arena, src, e, &mut memo))
                .collect();
            let terminator = graft_terminator(&mut arena, src, terminator, &mut memo);
            // Guest registers, read out of the VM context rather than from the evaluator's own registers. The evaluator's RSP/R11/RAX belong to the *interpreter* (VM scratch, bytecode base, decode temporaries) and say nothing about guest state.
            task.emu.ctx_miss = None;
            // At a guest return the VM has already copied the context back into the machine registers and torn the context down, so the slots hold teardown residue rather than guest state.
            let guest = if returning {
                None
            } else {
                task.emu.guest_registers()
            };
            if self.verbose && guest.is_none() {
                if let Some((rsp_ok, best)) = task.emu.ctx_miss {
                    eprintln!("     no guest ctx (rsp_slot_ok={rsp_ok}, best_score={best})");
                }
            }

            let from_vm_context = guest.is_some();
            let src = &task.emu.arena;
            let mut exit_regs: Vec<(crate::ir::expr::Reg, Ref)> = match guest {
                Some(regs) => regs
                    .into_iter()
                    .map(|(r, v)| (r, arena.graft(src, v, &mut memo)))
                    .collect(),
                None => crate::ir::expr::GPRS
                    .iter()
                    .filter_map(|&r| {
                        let v = *task.emu.state.regs.get(&r)?;
                        Some((r, arena.graft(src, v, &mut memo)))
                    })
                    .collect(),
            };
            exit_regs.sort_by_key(|(r, _)| *r as u8);

            // Read the continuation target before the terminator is moved.
            let continuation = match terminator {
                Terminator::Jump(t) => Some(t),
                _ => None,
            };

            blocks.push(Block {
                id: task.id,
                block_ref,
                params,
                events,
                from_vm_context,
                exit_regs,
                terminator,
                cost,
                preds: Vec::new(),
            });

            // A plain continuation reuses this evaluator rather than cloning it.
            // Done after grafting, which borrows the evaluator's arena.
            if let Some(t) = continuation {
                let mut ancestors = task.ancestors.clone();
                ancestors.push(task.id);
                queue.push(Task {
                    emu: task.emu,
                    resume: t.handler,
                    id: t,
                    from: Some(task.id),
                    ancestors,
                });
            }
        }

        let mut preds: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
        for (from, to) in &edges {
            preds.entry(*to).or_default().push(*from);
        }
        for b in &mut blocks {
            if let Some(p) = preds.get_mut(&b.id) {
                p.sort_unstable();
                p.dedup();
                b.preds = p.clone();
            }
        }

        blocks.sort_by_key(|b| b.id);
        back_edges.sort_unstable();
        back_edges.dedup();
        Cfg {
            entry: entry_id,
            arena,
            blocks,
            total_steps,
            timed_out,
            unresolved,
            back_edges,
            trivial_phis_removed: 0,
            stack_base: self.stack_base,
            vm_range: self.vm_range(),
        }
    }

    /// Half-open VA range of the configured VM section, or `(0, 0)` if absent.
    fn vm_range(&self) -> (u64, u64) {
        self.pe
            .section_by_name(&self.vm_section)
            .map(|s| {
                let lo = self.pe.rva_to_va(s.virtual_address);
                (lo, lo + s.virtual_size.max(s.raw_size) as u64)
            })
            .unwrap_or((0, 0))
    }

    /// Re-use the terminator an earlier concrete pass found for this block, queueing its successors with the current (symbolized) state.
    fn adopt_known(
        &self,
        task: &mut Task<'a>,
        known: Option<&Cfg>,
        queue: &mut Vec<Task<'a>>,
        cost: usize,
    ) -> Option<Terminator> {
        let prev = known?.block(task.id)?;
        if !adoption_covers_the_block(cost, prev.cost) {
            if self.verbose {
                eprintln!(
                    "     refusing to adopt {}: {cost} steps here vs {} in the \
                     concrete pass, so {} steps of events are missing",
                    task.id,
                    prev.cost,
                    prev.cost - cost,
                );
            }
            return None;
        }
        if self.verbose {
            eprintln!(
                "     adopting {} from concrete pass ({})",
                task.id,
                terminator_kind(&prev.terminator)
            );
        }
        let mut ancestors = task.ancestors.clone();
        ancestors.push(task.id);

        let resume_at = |queue: &mut Vec<Task<'a>>, id: BlockId| {
            queue.push(Task {
                emu: task.emu.clone(),
                resume: id.handler,
                id,
                from: Some(task.id),
                ancestors: ancestors.clone(),
            });
        };

        match &prev.terminator {
            Terminator::Jump(t) => {
                resume_at(queue, *t);
                Some(Terminator::Jump(*t))
            }
            Terminator::Branch {
                predicate,
                taken,
                not_taken,
            } => {
                // The predicate belongs to the previous pass's arena, so it cannot
                // be reused here; the successors are what matter for structure.
                let _ = predicate;
                resume_at(queue, *taken);
                resume_at(queue, *not_taken);
                Some(Terminator::Adopted {
                    taken: *taken,
                    not_taken: *not_taken,
                })
            }
            Terminator::Switch { targets, .. } => {
                for t in targets {
                    resume_at(queue, *t);
                }
                Some(Terminator::AdoptedSwitch {
                    targets: targets.clone(),
                })
            }
            // A back edge needs no successor queued: its target is recovered elsewhere in the graph.
            Terminator::Backedge { target } => Some(Terminator::Backedge { target: *target }),
            Terminator::TailCall { target } => Some(Terminator::TailCall { target: *target }),
            // The concrete pass ran this block to a return.
            Terminator::Return { .. } => Some(Terminator::AdoptedReturn),
            // Already-adopted terminators cannot appear here: only the concrete
            // pass is consulted, and it never adopts.
            Terminator::Unresolved { .. }
            | Terminator::Adopted { .. }
            | Terminator::AdoptedSwitch { .. }
            | Terminator::AdoptedReturn => None,
        }
    }

    fn split(
        &self,
        task: &mut Task<'a>,
        site: u64,
        dest: Ref,
        queue: &mut Vec<Task<'a>>,
    ) -> Terminator {
        let candidates = flag_candidates(&mut task.emu.arena, dest);
        if let Some(t) = self.split_two_way(task, site, dest, &candidates, queue) {
            return t;
        }
        // The diagnostic needs the same index enumeration the multi-way split just
        // built. Recomputing it would clone the whole arena a second time, and for a
        // function with a million DAG nodes that clone dominates the run.
        let mut enumerated: Vec<(Ref, u64)> = Vec::new();
        if let Some(t) = self.split_multiway(
            task,
            site,
            dest,
            queue,
            crate::vm::diag::is_on().then_some(&mut enumerated),
        ) {
            return t;
        }

        let leaves = expr::leaves(&task.emu.arena, dest);
        // Distinguish "we failed to fold" from "this genuinely depends on a function argument". An indirect dispatch whose index mixes in an `InitReg` (e.g.
        let arg_dependent = leaves
            .iter()
            .any(|l| matches!(task.emu.arena.op(*l), Op::InitReg(_)));
        // The reason string below is all that survives into the graph, so anything the
        // caller may want to aggregate has to be captured here, while the expression,
        // the state and the candidates all still exist.
        if crate::vm::diag::is_on() {
            crate::vm::diag::measure(|| {
                self.record_split_diag(
                    task,
                    site,
                    dest,
                    &candidates,
                    &leaves,
                    arg_dependent,
                    &enumerated,
                );
            });
        }
        let detail = leaves
            .iter()
            .take(4)
            .map(|l| render(&task.emu.arena, *l, 14))
            .collect::<Vec<_>>()
            .join(" | ");
        Terminator::Unresolved {
            reason: if arg_dependent {
                format!(
                    "argument-dependent indirect branch at {site:#x}: {} [leaves: {detail}]",
                    render(&task.emu.arena, dest, 3)
                )
            } else {
                format!(
                    "unresolved branch at {site:#x} ({} candidates): {} [leaves: {detail}]",
                    candidates.len(),
                    render(&task.emu.arena, dest, 3)
                )
            },
        }
    }

    /// Try to resolve `dest` as a two-way branch on a single bit.
    fn split_two_way(
        &self,
        task: &mut Task<'a>,
        site: u64,
        dest: Ref,
        candidates: &[Ref],
        queue: &mut Vec<Task<'a>>,
    ) -> Option<Terminator> {
        for cand in candidates.iter().copied() {
            let mut forks: [Option<(Emulator<'a>, u64)>; 2] = [None, None];
            for (i, value) in [0u64, 1u64].into_iter().enumerate() {
                let mut fork = task.emu.clone();
                fork.pins.push((cand, value));
                let folded = fork.substitute(dest);
                if let Some(t) = fork.arena.as_const(folded) {
                    // Reject predicates mistaken for branch targets. A pinned flag
                    // expression may fold to 0 and 1, which are not code addresses.
                    if self.pe.is_executable(t) {
                        forks[i] = Some((fork, t));
                    }
                }
            }

            // Deliberately NOT accepting a single executable side here.
            //
            // It is tempting: if one pinning lands on code and the other does not,
            // the surviving side looks like an unconditional jump. But a side that
            // fails to fold is not thereby *proved* unreachable -- it may simply not
            // be a constant under the pins in force. Turning that into a Jump
            // replaces "unresolved" with a silently wrong single edge, and a wrong
            // edge is worse than a missing one: it recovers fewer blocks while
            // reporting fewer failures, which is exactly how such a change looks
            // like an improvement. Measured on ACE-DFS64, enabling it changed five
            // of 157 functions and removed blocks from two of them.
            //
            // Both sides, or neither.
            let (Some((f0, t0)), Some((f1, t1))) = (forks[0].take(), forks[1].take()) else {
                continue;
            };
            let mut ancestors = task.ancestors.clone();
            ancestors.push(task.id);
            let id0 = BlockId {
                handler: t0,
                vip: f0.current_vip(),
            };
            let id1 = BlockId {
                handler: t1,
                vip: f1.current_vip(),
            };
            if id0 == id1 {
                // Not conditional under this pinning: continue with one side
                // and record a plain jump.
                queue.push(Task {
                    emu: f0,
                    resume: t0,
                    id: id0,
                    from: Some(task.id),
                    ancestors,
                });
                return Some(Terminator::Jump(id0));
            }
            if self.verbose {
                let shown = render(&task.emu.arena, cand, 4);
                eprintln!("     split at {site:#x}: [{shown}] -> {id1} / {id0}");
            }
            queue.push(Task {
                emu: f1,
                resume: t1,
                id: id1,
                from: Some(task.id),
                ancestors: ancestors.clone(),
            });
            queue.push(Task {
                emu: f0,
                resume: t0,
                id: id0,
                from: Some(task.id),
                ancestors,
            });
            return Some(Terminator::Branch {
                predicate: cand,
                taken: id1,
                not_taken: id0,
            });
        }
        None
    }

    /// Recover a virtualized switch by enumerating a narrow index. Candidate indices are subexpressions whose provable range is small. Enumeration is sound because the range comes from known-zero bits, so no reachable value is missed.
    fn split_multiway(
        &self,
        task: &mut Task<'a>,
        site: u64,
        dest: Ref,
        queue: &mut Vec<Task<'a>>,
        mut enumerated: Option<&mut Vec<(Ref, u64)>>,
    ) -> Option<Terminator> {
        for (idx, span) in narrow_indices(&mut task.emu.arena, dest) {
            if let Some(out) = enumerated.as_deref_mut() {
                out.push((idx, span));
            }
            let mut arms: Vec<(Emulator<'a>, u64)> = Vec::new();
            let mut all_folded = true;
            for value in 0..=span {
                let mut fork = task.emu.clone();
                fork.pins.push((idx, value));
                let folded = fork.substitute(dest);
                match fork.arena.as_const(folded) {
                    Some(t) if self.pe.is_executable(t) => arms.push((fork, t)),
                    Some(_) => {}
                    None => {
                        all_folded = false;
                        break;
                    }
                }
            }
            if !all_folded || arms.len() < 2 {
                continue;
            }

            let mut ancestors = task.ancestors.clone();
            ancestors.push(task.id);
            let mut targets: Vec<BlockId> = Vec::new();
            for (fork, t) in arms {
                let id = BlockId {
                    handler: t,
                    vip: fork.current_vip(),
                };
                let fresh = !targets.contains(&id);
                if fresh {
                    targets.push(id);
                }
                queue.push(Task {
                    emu: fork,
                    resume: t,
                    id,
                    from: Some(task.id),
                    ancestors: ancestors.clone(),
                });
            }
            if self.verbose {
                let shown = render(&task.emu.arena, idx, 4);
                eprintln!(
                    "     switch at {site:#x}: [{shown}] -> {} arms",
                    targets.len()
                );
            }
            if targets.len() == 1 {
                return Some(Terminator::Jump(targets[0]));
            }
            return Some(Terminator::Switch {
                selector: idx,
                targets,
            });
        }
        None
    }

    /// Everything still known about a branch that no split could resolve.
    ///
    /// Called only when collection is on, and only from the failure path, so it costs one
    /// probe per failing block rather than one per candidate per block.
    #[allow(clippy::too_many_arguments)]
    fn record_split_diag(
        &self,
        task: &mut Task<'a>,
        site: u64,
        dest: Ref,
        candidates: &[Ref],
        leaves: &[Ref],
        arg_dependent: bool,
        enumerated: &[(Ref, u64)],
    ) {
        let record = self.collect_split_diag(
            task,
            site,
            dest,
            candidates,
            leaves,
            arg_dependent,
            enumerated,
        );
        crate::vm::diag::record_split(record);
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_split_diag(
        &self,
        task: &mut Task<'a>,
        site: u64,
        dest: Ref,
        candidates: &[Ref],
        leaves: &[Ref],
        arg_dependent: bool,
        enumerated: &[(Ref, u64)],
    ) -> crate::vm::diag::SplitDiag {
        // Bounded: a block with hundreds of candidates would otherwise cost more to
        // describe than it did to fail.
        const MAX_CANDIDATES: usize = 24;
        const MAX_LEAVES: usize = 12;
        const MAX_INDICES: usize = 8;

        let (vm_lo, vm_hi) = self.vm_range();
        let in_vm = |t: u64| vm_lo != 0 && t >= vm_lo && t < vm_hi;

        let mut flag_candidates = Vec::new();
        for cand in candidates.iter().take(MAX_CANDIDATES) {
            let prior_pin = task
                .emu
                .pins
                .iter()
                .find(|(p, _)| p == cand)
                .map(|(_, v)| *v);
            let mut arms = Vec::new();
            for value in [0u64, 1u64] {
                let (target, vip) = task.emu.probe_pin(*cand, value, dest);
                // With no older pin for this candidate, appending and forcing are the
                // same operation, so the comparison cannot say anything and one of the
                // two folds is skipped. A fold is not cheap: it invalidates the
                // substitution caches and re-walks the dependency cone.
                let naive = match prior_pin {
                    Some(_) => task.emu.probe_pin_appended(*cand, value, dest),
                    None => target,
                };
                arms.push(crate::vm::diag::PinArm {
                    value,
                    executable: target.is_some_and(|t| self.pe.is_executable(t)),
                    in_vm_section: target.is_some_and(&in_vm),
                    target,
                    vip,
                    naive_target: if naive == target { None } else { naive },
                });
            }
            flag_candidates.push(crate::vm::diag::FlagProbe {
                expr: render(&task.emu.arena, *cand, 6),
                prior_pin,
                arms,
            });
        }

        let mut index_probes = Vec::new();
        for (idx, span) in enumerated.iter().copied().take(MAX_INDICES) {
            let mut arms = Vec::new();
            let mut unfolded = false;
            let mut non_executable_arms = 0usize;
            for value in 0..=span {
                let (target, vip) = task.emu.probe_pin(idx, value, dest);
                let executable = target.is_some_and(|t| self.pe.is_executable(t));
                if target.is_some() && !executable {
                    non_executable_arms += 1;
                }
                if target.is_none() {
                    unfolded = true;
                }
                arms.push(crate::vm::diag::SwitchArm {
                    value,
                    target,
                    executable,
                    in_vm_section: target.is_some_and(&in_vm),
                    vip,
                });
            }
            index_probes.push(crate::vm::diag::SwitchProbe {
                expr: render(&task.emu.arena, idx, 6),
                span,
                arms,
                unfolded,
                non_executable_arms,
            });
        }

        crate::vm::diag::SplitDiag {
            entry: 0,
            pass: 0,
            handler: task.id.handler,
            vip: task.id.vip,
            site,
            // Bounded depth: the reason string keeps only three levels, and a full DAG
            // render of a large expression is neither readable nor cheap.
            dest_expr: render(&task.emu.arena, dest, 12),
            dest_width: format!("{:?}", task.emu.arena.width(dest)),
            arg_dependent,
            leaves: leaves
                .iter()
                .take(MAX_LEAVES)
                .map(|l| render(&task.emu.arena, *l, 10))
                .collect(),
            leaf_total: leaves.len(),
            flag_candidates,
            narrow_indices: index_probes,
        }
    }
}

/// Expression roots retained outside the arena when a path stops diverging.
/// Any future arena compaction must preserve and remap this complete set.
fn divergence_roots(
    state: &crate::vm::state::State,
    pins: &[(Ref, u64)],
    events: &[Event],
) -> Vec<Ref> {
    let mut roots = state.symbolic_roots();
    roots.extend(pins.iter().map(|(expr, _)| *expr));
    for event in events {
        event.append_roots(&mut roots);
    }
    roots
}

/// Name of a stop variant, for diagnostics.
fn stop_kind(stop: &Stop) -> &'static str {
    match stop {
        Stop::SymbolicBranch { .. } => "SymbolicBranch",
        Stop::NativeBranch { .. } => "NativeBranch",
        Stop::Return { .. } => "Return",
        Stop::Unsupported { .. } => "Unsupported",
        Stop::Unreadable { .. } => "Unreadable",
        Stop::Backedge { .. } => "Backedge",
        Stop::Budget { .. } => "Budget",
        Stop::Diverged { .. } => "Diverged",
        Stop::OutOfImage { .. } => "OutOfImage",
        Stop::BadTarget { .. } => "BadTarget",
        Stop::NoReturn { .. } => "NoReturn",
    }
}

/// Address a stop was raised at.
fn stop_site(stop: &Stop) -> u64 {
    match stop {
        Stop::SymbolicBranch { site, .. }
        | Stop::NativeBranch { site, .. }
        | Stop::Return { site, .. }
        | Stop::Unsupported { site, .. }
        | Stop::Unreadable { site }
        | Stop::Backedge { site, .. }
        | Stop::Budget { site }
        | Stop::Diverged { site, .. }
        | Stop::OutOfImage { site }
        | Stop::BadTarget { site, .. }
        | Stop::NoReturn { site } => *site,
    }
}

/// Move an event's expressions into `dst`.
fn graft_event(dst: &mut Arena, src: &Arena, e: Event, memo: &mut HashMap<Ref, Ref>) -> Event {
    match e {
        Event::Store {
            addr,
            value,
            width,
            site,
            region,
        } => Event::Store {
            addr: dst.graft(src, addr, memo),
            value: dst.graft(src, value, memo),
            width,
            site,
            region,
        },
        Event::Load {
            addr,
            value,
            width,
            site,
            region,
        } => Event::Load {
            addr: dst.graft(src, addr, memo),
            value: dst.graft(src, value, memo),
            width,
            site,
            region,
        },
        // The memory address must be grafted like any other expression, or it
        // would name a node in the source arena and read as an unrelated value.
        Event::Boxed {
            site,
            text,
            bytes,
            mem,
            defs,
            uses,
        } => Event::Boxed {
            site,
            text,
            bytes,
            mem: mem.map(|m| crate::ir::lift::BoxedMem {
                addr: dst.graft(src, m.addr, memo),
                bytes: m.bytes,
                writes: m.writes,
            }),
            // The opaques naming this instruction's register results are nodes in the
            // source arena like any other expression, so they need grafting too.
            defs: defs
                .into_iter()
                .map(|(r, v)| (r, dst.graft(src, v, memo)))
                .collect(),
            // Likewise the guest values its implicit register reads consume.
            uses: uses
                .into_iter()
                .map(|(r, v)| (r, dst.graft(src, v, memo)))
                .collect(),
        },
        Event::Call {
            target,
            site,
            rsp,
            import_slot,
            args,
            ret,
        } => Event::Call {
            import_slot,
            target: target.map(|t| dst.graft(src, t, memo)),
            site,
            rsp,
            // Argument expressions must be grafted too, or they would name nodes
            // in the source arena and read as unrelated values in the destination.
            args: args
                .into_iter()
                .map(|(r, v)| (r, dst.graft(src, v, memo)))
                .collect(),
            // Likewise the return opaque: an ungrafted ref would name a node in the
            // source arena, and scheduling would place an unrelated value at the call.
            ret: ret.map(|r| dst.graft(src, r, memo)),
        },
    }
}

/// Move a terminator's expressions into `dst`.
fn graft_terminator(
    dst: &mut Arena,
    src: &Arena,
    t: Terminator,
    memo: &mut HashMap<Ref, Ref>,
) -> Terminator {
    match t {
        Terminator::Branch {
            predicate,
            taken,
            not_taken,
        } => Terminator::Branch {
            predicate: dst.graft(src, predicate, memo),
            taken,
            not_taken,
        },
        Terminator::Switch { selector, targets } => Terminator::Switch {
            selector: dst.graft(src, selector, memo),
            targets,
        },
        Terminator::Return { dest } => Terminator::Return {
            dest: dst.graft(src, dest, memo),
        },
        other => other,
    }
}

/// Maximum number of arms a virtualized switch is enumerated over.
///
/// Kept small deliberately: each arm costs a state fork, and TVM's multi-way
/// dispatches select among a handful of VPCs rather than large jump tables.
const MAX_SWITCH_ARMS: u64 = 63;

/// Subexpressions of `dest` whose provable value range is narrow enough to enumerate, paired with the largest value each can take. The range is derived from known-zero bits, which makes enumeration sound: a value outside it is unreachable.
fn narrow_indices(a: &mut Arena, dest: Ref) -> Vec<(Ref, u64)> {
    let mut out: Vec<(Ref, u64)> = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![dest];

    while let Some(cur) = stack.pop() {
        if !seen.insert(cur) {
            continue;
        }
        if !a.is_const(cur) {
            // Bits that may be set, within the node's own width.
            let known_zero = a.known_zero(cur);
            let live = !known_zero & a.width(cur).mask();
            // Only contiguous low-bit ranges are enumerated directly.
            if live != 0 && live.count_ones() == live.trailing_ones() && live <= MAX_SWITCH_ARMS {
                out.push((cur, live));
            }
        }
        match *a.op(cur) {
            Op::Bin(_, x, y) => {
                stack.push(x);
                stack.push(y);
            }
            Op::Un(_, x) | Op::Zext(x) | Op::Sext(x) | Op::Trunc(x) | Op::Load(x, _) => {
                stack.push(x)
            }
            Op::Select(c, x, y) => {
                stack.push(c);
                stack.push(x);
                stack.push(y);
            }
            _ => {}
        }
    }

    // Narrowest range first: fewer forks, and a narrow index is more likely to
    // be the actual selector rather than an incidental subexpression.
    out.sort_by_key(|(_, span)| *span);
    out
}

/// Symbolic leaves that could be the decisive bit of a virtualized JCC.
///
/// Ordered so the most likely candidates come first, because each one costs two
/// state forks to test: entry flags (opaque single-bit values), then any 8-bit
/// boolean-producing node, innermost first.
fn flag_candidates(a: &mut Arena, dest: Ref) -> Vec<Ref> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for leaf in expr::leaves(a, dest) {
        if let Op::Opaque(tag, _) = a.op(leaf) {
            if tag.contains("flag") && seen.insert(leaf) {
                out.push(leaf);
            }
        }
    }

    // Any node that is provably a single bit is a candidate, regardless of its declared width. This matters for Tencent VM's virtualized JCC, where the decisive value is reached through a `trunc32`/`zext64` chain:
    let mut ranked: Vec<(u32, Ref)> = Vec::new();
    let mut stack = vec![(dest, 0u32)];
    let mut walked = HashSet::new();
    while let Some((cur, d)) = stack.pop() {
        if !walked.insert(cur) {
            continue;
        }
        if !a.is_const(cur) && a.known_zero(cur) | 1 == u64::MAX {
            // Every bit except bit 0 is provably zero: this is a 0/1 value.
            ranked.push((d, cur));
        }
        match *a.op(cur) {
            Op::Bin(_, x, y) => {
                stack.push((x, d + 1));
                stack.push((y, d + 1));
            }
            Op::Un(_, x) | Op::Zext(x) | Op::Sext(x) | Op::Trunc(x) | Op::Load(x, _) => {
                stack.push((x, d + 1))
            }
            Op::Select(c, x, y) => {
                stack.push((c, d + 1));
                stack.push((x, d + 1));
                stack.push((y, d + 1));
            }
            _ => {}
        }
    }
    ranked.sort_by_key(|(d, _)| std::cmp::Reverse(*d));
    for (_, r) in ranked {
        if seen.insert(r) {
            out.push(r);
        }
    }

    // Finally, any remaining 8-bit non-constant node, as a fallback.
    for leaf in expr::leaves(a, dest) {
        if a.width(leaf) == Width::W8 && !a.is_const(leaf) && seen.insert(leaf) {
            out.push(leaf);
        }
    }
    out
}

/// Structural problems in a recovered CFG.
///
/// These are invariants the recovery process should maintain regardless of the
/// binary. A violation means a bug in the explorer rather than a limitation on
/// the target, so they are checked separately from coverage.
#[derive(Debug, Default)]
pub struct CfgIssues {
    /// Edges pointing at a block that was never recovered.
    pub dangling_edges: Vec<(BlockId, BlockId)>,
    /// Blocks the VM was interpreting for which no bytecode position could be read, so their identity is `handler` alone. Block identity is `(handler, vip)`.
    pub unidentified_blocks: Vec<BlockId>,
    /// Blocks other than the entry with no predecessors.
    pub orphans: Vec<BlockId>,
    /// Distinct blocks that share a handler *and* a VIP, which should be
    /// impossible given that the pair is the block identity.
    pub duplicate_ids: Vec<BlockId>,
    /// Branch or switch terminators whose targets are not all distinct.
    pub degenerate_branches: Vec<BlockId>,
    /// Uses of a parameter owned by a block that does not dominate the use. This is the SSA scoping rule.
    pub nondominating_params: Vec<(BlockId, expr::BlockRef)>,
    /// Parameters declared by a block but not used by anything in it. Harmless,
    /// but a sign the cut is wider than it needs to be.
    pub dead_params: usize,
    pub cannot_reach_exit: Vec<BlockId>,
}

impl CfgIssues {
    pub fn is_clean(&self) -> bool {
        self.nondominating_params.is_empty()
            && self.dangling_edges.is_empty()
            && self.orphans.is_empty()
            && self.duplicate_ids.is_empty()
            && self.degenerate_branches.is_empty()
            && self.cannot_reach_exit.is_empty()
            && self.unidentified_blocks.is_empty()
    }

    /// One line naming the kinds that fired, with counts. The `Debug` form lists every offending `BlockId`, which runs to hundreds of lines for a large graph and buries the one fact needed first: which invariant broke.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut add = |name: &str, n: usize| {
            if n > 0 {
                parts.push(format!("{name} {n}"));
            }
        };
        add("dangling_edges", self.dangling_edges.len());
        add("unidentified", self.unidentified_blocks.len());
        add("orphans", self.orphans.len());
        add("duplicate_ids", self.duplicate_ids.len());
        add("degenerate_branches", self.degenerate_branches.len());
        add("nondominating_params", self.nondominating_params.len());
        add("cannot_reach_exit", self.cannot_reach_exit.len());
        if parts.is_empty() {
            parts.push("clean".to_string());
        }
        if self.dead_params > 0 {
            parts.push(format!("(dead_params {})", self.dead_params));
        }
        parts.join(", ")
    }
}

/// Dominator sets, keyed by block. Standard iterative fixed point: a block is dominated by itself plus the intersection of its predecessors' dominators.
fn dominators(cfg: &Cfg) -> HashMap<BlockId, HashSet<BlockId>> {
    let all: HashSet<BlockId> = cfg.blocks.iter().map(|b| b.id).collect();
    let mut dom: HashMap<BlockId, HashSet<BlockId>> = cfg
        .blocks
        .iter()
        .map(|b| {
            let init = if b.id == cfg.entry {
                HashSet::from([b.id])
            } else {
                all.clone()
            };
            (b.id, init)
        })
        .collect();

    let mut changed = true;
    while changed {
        changed = false;
        for b in &cfg.blocks {
            if b.id == cfg.entry {
                continue;
            }
            let mut next: Option<HashSet<BlockId>> = None;
            for p in &b.preds {
                let Some(pd) = dom.get(p) else { continue };
                next = Some(match next {
                    None => pd.clone(),
                    Some(acc) => acc.intersection(pd).copied().collect(),
                });
            }
            let mut next = next.unwrap_or_default();
            next.insert(b.id);
            if dom.get(&b.id) != Some(&next) {
                dom.insert(b.id, next);
                changed = true;
            }
        }
    }
    dom
}

/// Check a recovered CFG against its structural invariants. Whether adopting a concrete-pass terminator would describe the whole block. Adoption takes the terminator from one pass and the events from another, so it is only coherent when both passes ran the same instructions.
fn adoption_covers_the_block(steps_this_pass: usize, steps_concrete_pass: usize) -> bool {
    steps_this_pass >= steps_concrete_pass
}

/// Blocks that sit on, or only lead to, a cycle that polls memory it does not own. A cycle with no exit is only a hang if it computes the same thing forever.
fn polling_cycle_blocks(
    cfg: &Cfg,
    by_id: &HashMap<BlockId, &Block>,
    exitless: &HashSet<BlockId>,
) -> HashSet<BlockId> {
    const FRAME_WINDOW: u64 = 0x10_000;
    let in_frame_window = |c: u64| c.abs_diff(cfg.stack_base) <= FRAME_WINDOW;

    fn frame_rooted(
        arena: &expr::Arena,
        r: Ref,
        depth: u32,
        in_window: &dyn Fn(u64) -> bool,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        match arena.op(r) {
            Op::Const(c) => in_window(*c),
            Op::InitReg(expr::Reg::Rsp) | Op::InitReg(expr::Reg::Rbp) => true,
            Op::Param(_, expr::Reg::Rsp) | Op::Param(_, expr::Reg::Rbp) => true,
            Op::Bin(_, a, b) => {
                frame_rooted(arena, *a, depth - 1, in_window)
                    || frame_rooted(arena, *b, depth - 1, in_window)
            }
            Op::Un(_, a) | Op::Zext(a) | Op::Sext(a) | Op::Trunc(a) => {
                frame_rooted(arena, *a, depth - 1, in_window)
            }
            Op::Select(_, a, b) => {
                frame_rooted(arena, *a, depth - 1, in_window)
                    || frame_rooted(arena, *b, depth - 1, in_window)
            }
            _ => false,
        }
    }

    // A block polls if it can observe something it did not compute itself. Two things disqualify a load. Stores never count: a loop that only writes cannot learn that it should stop.
    let polls = |id: &BlockId| {
        by_id.get(id).is_some_and(|b| {
            b.events.iter().any(|e| match e {
                Event::Load { addr, region, .. } => {
                    region.is_guest() && !frame_rooted(&cfg.arena, *addr, 32, &in_frame_window)
                }
                Event::Call { .. } | Event::Boxed { .. } => true,
                _ => false,
            })
        })
    };

    let seeds: Vec<BlockId> = exitless.iter().copied().filter(polls).collect();
    if seeds.is_empty() {
        return HashSet::new();
    }

    // Walk backwards from the polling blocks through exitless predecessors.
    let mut preds: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
    for b in &cfg.blocks {
        if !exitless.contains(&b.id) {
            continue;
        }
        for s in Cfg::successors(b) {
            if exitless.contains(&s) {
                preds.entry(s).or_default().push(b.id);
            }
        }
    }

    let mut excused: HashSet<BlockId> = HashSet::new();
    let mut stack = seeds;
    while let Some(id) = stack.pop() {
        if !excused.insert(id) {
            continue;
        }
        if let Some(ps) = preds.get(&id) {
            stack.extend(ps.iter().copied());
        }
    }
    excused
}

pub fn validate(cfg: &Cfg) -> CfgIssues {
    let mut issues = CfgIssues::default();
    let known: HashSet<BlockId> = cfg.blocks.iter().map(|b| b.id).collect();
    let dom = dominators(cfg);
    let owner_of: HashMap<expr::BlockRef, BlockId> =
        cfg.blocks.iter().map(|b| (b.block_ref, b.id)).collect();

    // Every block reachable from the entry must be able to reach a terminator that leaves the function.
    let by_id: HashMap<BlockId, &Block> = cfg.blocks.iter().map(|b| (b.id, b)).collect();
    let mut reachable: HashSet<BlockId> = HashSet::new();
    let mut stack = vec![cfg.entry];
    while let Some(id) = stack.pop() {
        if !reachable.insert(id) {
            continue;
        }
        if let Some(b) = by_id.get(&id) {
            stack.extend(Cfg::successors(b));
        }
    }

    // Walk backwards from the exits: a block can terminate exactly when some
    // successor can, so the answer is reverse reachability over the exit set.
    let mut preds: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
    let mut exits: Vec<BlockId> = Vec::new();
    for b in &cfg.blocks {
        for s in Cfg::successors(b) {
            preds.entry(s).or_default().push(b.id);
        }
        if matches!(
            b.terminator,
            Terminator::Return { .. }
                | Terminator::AdoptedReturn
                | Terminator::TailCall { .. }
                | Terminator::Unresolved { .. }
        ) {
            exits.push(b.id);
        }
    }
    let mut can_exit: HashSet<BlockId> = HashSet::new();
    let mut stack = exits;
    while let Some(id) = stack.pop() {
        if !can_exit.insert(id) {
            continue;
        }
        if let Some(ps) = preds.get(&id) {
            stack.extend(ps.iter().copied());
        }
    }
    issues.unidentified_blocks = cfg
        .blocks
        .iter()
        .filter(|b| {
            b.id != cfg.entry
                && b.from_vm_context
                && b.id.vip.is_none()
                && (cfg.vm_range.0..cfg.vm_range.1).contains(&b.id.handler)
        })
        .map(|b| b.id)
        .collect();

    issues.cannot_reach_exit = {
        let exitless: HashSet<BlockId> = cfg
            .blocks
            .iter()
            .map(|b| b.id)
            .filter(|id| reachable.contains(id) && !can_exit.contains(id))
            .collect();
        let excused = polling_cycle_blocks(cfg, &by_id, &exitless);
        let mut v: Vec<BlockId> = exitless
            .into_iter()
            .filter(|id| !excused.contains(id))
            .collect();
        // `HashSet` iteration order is unspecified, and this list is reported to the
        // user and compared in tests.
        v.sort_by_key(|id| (id.handler, id.vip));
        v
    };

    let mut seen = HashSet::new();
    for b in &cfg.blocks {
        if !seen.insert(b.id) {
            issues.duplicate_ids.push(b.id);
        }
        let succs = Cfg::successors(b);
        for s in &succs {
            if !known.contains(s) {
                issues.dangling_edges.push((b.id, *s));
            }
        }
        match &b.terminator {
            Terminator::Branch {
                taken, not_taken, ..
            } if taken == not_taken => {
                issues.degenerate_branches.push(b.id);
            }
            Terminator::Switch { targets, .. } => {
                let uniq: HashSet<_> = targets.iter().collect();
                if uniq.len() != targets.len() {
                    issues.degenerate_branches.push(b.id);
                }
            }
            _ => {}
        }
        if b.id != cfg.entry && b.preds.is_empty() {
            issues.orphans.push(b.id);
        }

        // Every parameter this block uses must be owned by a block that dominates
        // it. A phi's *incoming* values legitimately mention the predecessor's
        // parameters, so they are excluded here: they are edge annotations rather
        // than part of the block body.
        let mut refs: Vec<Ref> = b.exit_regs.iter().map(|(_, v)| *v).collect();
        for e in &b.events {
            match e {
                Event::Store { addr, value, .. } | Event::Load { addr, value, .. } => {
                    refs.push(*addr);
                    refs.push(*value);
                }
                Event::Call { target, args, .. } => {
                    if let Some(t) = target {
                        refs.push(*t);
                    }
                    refs.extend(args.iter().map(|(_, v)| *v));
                }
                _ => {}
            }
        }
        match &b.terminator {
            Terminator::Branch { predicate, .. } => refs.push(*predicate),
            Terminator::Switch { selector, .. } => refs.push(*selector),
            Terminator::Return { dest } => refs.push(*dest),
            _ => {}
        }

        let mut used: HashSet<Ref> = HashSet::new();
        for r in refs {
            for leaf in expr::leaves(&cfg.arena, r) {
                if let Op::Param(owner, _) = cfg.arena.op(leaf) {
                    let dominates = owner_of
                        .get(owner)
                        .and_then(|oid| dom.get(&b.id).map(|d| d.contains(oid)))
                        .unwrap_or(false);
                    if !dominates {
                        issues.nondominating_params.push((b.id, *owner));
                    }
                    used.insert(leaf);
                }
            }
        }
        // `exit_regs` count as uses: they are the values leaving the block, so a
        // parameter passed straight through is still live.
        issues.dead_params += b.params.iter().filter(|(_, p)| !used.contains(p)).count();
    }
    issues
}

#[cfg(test)]
mod dom_tests {
    use super::*;
    use crate::ir::expr::Reg;
    use crate::vm::DEFAULT_STACK_BASE;

    const STACK_BASE: u64 = DEFAULT_STACK_BASE;

    fn blk(handler: u64, preds: Vec<BlockId>, term: Terminator) -> Block {
        Block {
            id: BlockId { handler, vip: None },
            block_ref: expr::BlockRef(handler as u32),
            params: Vec::new(),
            events: Vec::new(),
            // These are structural tests: the blocks carry no VM context, so the
            // "no bytecode position" invariant does not apply to them.
            from_vm_context: false,
            exit_regs: Vec::new(),
            terminator: term,
            cost: 0,
            preds,
        }
    }

    fn id(h: u64) -> BlockId {
        BlockId {
            handler: h,
            vip: None,
        }
    }

    /// A diamond: 0 -> {1, 2} -> 3. Block 0 dominates all; 1 and 2 dominate only
    /// themselves and neither dominates the join.
    fn diamond() -> Cfg {
        let mut arena = Arena::default();
        let pred = arena.init_reg(Reg::Rax);
        let mut blocks = vec![
            blk(
                0,
                vec![],
                Terminator::Branch {
                    predicate: pred,
                    taken: id(1),
                    not_taken: id(2),
                },
            ),
            blk(1, vec![id(0)], Terminator::Jump(id(3))),
            blk(2, vec![id(0)], Terminator::Jump(id(3))),
            blk(
                3,
                vec![id(1), id(2)],
                Terminator::TailCall { target: 0x1000 },
            ),
        ];
        blocks.sort_by_key(|b| b.id);
        Cfg {
            entry: id(0),
            arena,
            blocks,
            total_steps: 0,
            unresolved: 0,
            back_edges: Vec::new(),
            trivial_phis_removed: 0,
            timed_out: false,
            stack_base: STACK_BASE,
            vm_range: (0, 0),
        }
    }

    /// Adoption must be refused exactly when the block's events are incomplete.
    #[test]
    fn adoption_is_refused_when_it_would_lose_events() {
        // The two real truncations.
        assert!(
            !adoption_covers_the_block(2833, 4610),
            "0x140012bac lost 1777 steps including the indexed CAS"
        );
        assert!(
            !adoption_covers_the_block(2831, 4890),
            "0x140012d1c lost 2059 steps the same way"
        );
        // Reaching the same stop point is what makes adoption coherent.
        assert!(
            adoption_covers_the_block(4610, 4610),
            "same stop, safe to adopt"
        );
        // One step short is still short: there is no tolerance to spend, because a
        // single missing step can be the guest operation the whole block exists to
        // perform.
        assert!(
            !adoption_covers_the_block(4609, 4610),
            "one step short is short"
        );
        // The symbolic pass cannot outrun the concrete one, but if it ever did the
        // block would be fully covered and adoption stays sound.
        assert!(adoption_covers_the_block(4611, 4610), "more than covered");
        // A block the concrete pass never ran has nothing to be short of.
        assert!(adoption_covers_the_block(0, 0));
    }

    /// A graph where every path loops forever must be rejected, and the same
    /// graph with one exit added must pass.
    #[test]
    fn a_graph_with_no_reachable_exit_is_rejected() {
        let mut arena = Arena::default();
        let pred = arena.init_reg(Reg::Rax);
        let spin = |arena: Arena, last: Terminator| {
            let mut blocks = vec![
                blk(0, vec![], Terminator::Jump(id(1))),
                blk(
                    1,
                    vec![id(0), id(2), id(3)],
                    Terminator::Branch {
                        predicate: pred,
                        taken: id(2),
                        not_taken: id(3),
                    },
                ),
                blk(2, vec![id(1)], Terminator::Backedge { target: id(1) }),
                blk(3, vec![id(1)], last),
            ];
            blocks.sort_by_key(|b| b.id);
            Cfg {
                entry: id(0),
                arena,
                blocks,
                total_steps: 0,
                unresolved: 0,
                back_edges: Vec::new(),
                trivial_phis_removed: 0,
                timed_out: false,
                stack_base: STACK_BASE,
                vm_range: (0, 0),
            }
        };

        let looping = spin(arena.clone(), Terminator::Backedge { target: id(1) });
        let issues = validate(&looping);
        assert_eq!(
            issues.cannot_reach_exit.len(),
            4,
            "every block of a graph that cannot terminate must be flagged"
        );
        assert!(!issues.is_clean());
        // Nothing else should be wrong with it: the point is that this graph is
        // well formed apart from having no way out.
        assert!(issues.dangling_edges.is_empty());
        assert!(issues.orphans.is_empty());
        assert!(issues.degenerate_branches.is_empty());

        // Giving one path an exit clears every block: block 2 loops back to 1,
        // and 1 can still reach the exit through 3.
        let exits = spin(arena.clone(), Terminator::TailCall { target: 0x1000 });
        assert!(validate(&exits).cannot_reach_exit.is_empty());

        // But a block that returns does not excuse a sibling stuck in a closed
        // cycle. This is the shape that reached the output file: one arm of the
        // branch returns, the other spins, and checking only the entry passes it.
        let mut arena2 = arena;
        let pred2 = arena2.init_reg(Reg::Rbx);
        let dest = arena2.init_reg(Reg::Rax);
        let mut blocks = vec![
            blk(
                0,
                vec![],
                Terminator::Branch {
                    predicate: pred2,
                    taken: id(1),
                    not_taken: id(2),
                },
            ),
            blk(1, vec![id(0)], Terminator::Return { dest }),
            blk(2, vec![id(0), id(3)], Terminator::Jump(id(3))),
            blk(3, vec![id(2)], Terminator::Jump(id(2))),
        ];
        blocks.sort_by_key(|b| b.id);
        let half = Cfg {
            entry: id(0),
            arena: arena2,
            blocks,
            total_steps: 0,
            unresolved: 0,
            back_edges: Vec::new(),
            trivial_phis_removed: 0,
            timed_out: false,
            stack_base: STACK_BASE,
            vm_range: (0, 0),
        };
        let issues = validate(&half);
        assert_eq!(
            issues.cannot_reach_exit,
            vec![id(2), id(3)],
            "a hanging arm must be flagged even when another arm returns"
        );
    }

    #[test]
    fn using_a_sibling_branchs_param_is_flagged() {
        // Block 2 using block 1's parameter is unsound: block 1 does not run on
        // the path through block 2. This is exactly the mistake block parameters
        // exist to prevent, so validation must catch it.
        let mut cfg = diamond();
        let p = cfg.arena.param(expr::BlockRef(1), Reg::Rax);
        let two = cfg.blocks.iter_mut().find(|b| b.id == id(2)).unwrap();
        two.exit_regs = vec![(Reg::Rax, p)];

        let issues = validate(&cfg);
        assert_eq!(
            issues.nondominating_params,
            vec![(id(2), expr::BlockRef(1))]
        );
        assert!(!issues.is_clean());
    }

    #[test]
    fn using_a_dominators_param_is_accepted() {
        // Block 3 using block 0's parameter is fine: block 0 dominates everything,
        // so the value is always defined by the time block 3 runs.
        let mut cfg = diamond();
        let p = cfg.arena.param(expr::BlockRef(0), Reg::Rax);
        let three = cfg.blocks.iter_mut().find(|b| b.id == id(3)).unwrap();
        three.exit_regs = vec![(Reg::Rax, p)];

        let issues = validate(&cfg);
        assert!(issues.nondominating_params.is_empty());
        assert!(issues.is_clean());
    }

    /// The missing-position invariant applies to VM handlers, not to native blocks. `.tvm0` also holds *partially* virtualized functions: the VM stub runs the prologue and then jumps into ordinary `.text` that still addresses the VM context through RBP.
    #[test]
    fn a_native_block_without_a_bytecode_position_is_not_unidentified() {
        let mut cfg = diamond();
        cfg.vm_range = (0x1_0000, 0x2_0000);
        // Block 2 sits outside the VM section: a native continuation of the stub.
        let two = cfg.blocks.iter_mut().find(|b| b.id == id(2)).unwrap();
        two.from_vm_context = true;
        assert!(
            validate(&cfg).unidentified_blocks.is_empty(),
            "a native block needs no bytecode position to be identified"
        );

        // A block at a VM handler is still checked, since handlers are shared.
        let mut cfg = diamond();
        cfg.vm_range = (0, 0x1_0000);
        let two = cfg.blocks.iter_mut().find(|b| b.id == id(2)).unwrap();
        two.from_vm_context = true;
        assert_eq!(
            validate(&cfg).unidentified_blocks,
            vec![id(2)],
            "a VM handler without a position is ambiguous and must be flagged"
        );
    }
}

#[cfg(test)]
mod candidate_tests {
    use super::*;
    use crate::ir::expr::{BinOp, Reg, Width};

    /// The VJCC index shape must yield the ZF bit as a candidate.
    #[test]
    fn finds_one_bit_flag_under_trunc_and_shift() {
        let mut a = Arena::new();
        let packed = a.init_reg(Reg::Rax);
        let six = a.constant(6, Width::W64);
        let shifted = a.bin(BinOp::Shr, packed, six);
        let t = a.trunc(shifted, Width::W8);
        let one = a.constant(1, Width::W8);
        let bit = a.bin(BinOp::And, t, one);
        // index = bit << 3, then used as a table offset
        let wide = a.zext(bit, Width::W64);
        let three = a.constant(3, Width::W64);
        let idx = a.bin(BinOp::Shl, wide, three);
        let base = a.constant(0x140055f6b, Width::W64);
        let addr = a.bin(BinOp::Add, idx, base);
        let dest = a.load(addr, Width::W32);

        let cands = flag_candidates(&mut a, dest);
        assert!(
            !cands.is_empty(),
            "no candidates found for the VJCC index shape"
        );
        // The one-bit value must be among them.
        assert!(
            cands.iter().any(|&c| {
                let mut probe = a.clone();
                probe.known_zero(c) | 1 == u64::MAX
            }),
            "candidates contain no provably one-bit value"
        );
    }
}
