//! Write recovered functions into the PE, then patch each entry trampoline to jmp to its recovered counterpart.

use rayon::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::binary::pe::PeFile;
use crate::binary::pe_format as fmt;
use crate::codegen;
use crate::ir;
use crate::ir::regalloc;
use crate::ir::sched;
use crate::vm::discover::VmEntry;
use crate::vm::explore;
use anyhow::{Context, Result, bail};

/// Per-function devirtualization results.
pub struct DevirtReport {
    /// Functions whose code was written and whose trampoline now points at it.
    pub retargeted: usize,
    /// Recovery did not finish
    pub incomplete: Vec<(u64, String)>,
    /// Recovery finished but codegen refused, with the reason.
    pub failed: Vec<(u64, String)>,
    /// Recovery-level unresolved accounting, summed over every function.
    pub unresolved: explore::UnresolvedStats,
    /// Image was signed and the signature has been removed.
    pub dropped_signature: bool,
    pub pdata_entries: usize,
    pub pdata_added: usize,
    pub checksum: u32,
}

impl DevirtReport {
    pub fn total(&self) -> usize {
        self.retargeted + self.incomplete.len() + self.failed.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeFunction {
    begin: u32,
    end: u32,
    unwind: u32,
}

impl RuntimeFunction {
    fn to_bytes(self) -> [u8; fmt::RUNTIME_FUNCTION_SIZE] {
        let mut b = [0u8; fmt::RUNTIME_FUNCTION_SIZE];
        b[0..4].copy_from_slice(&self.begin.to_le_bytes());
        b[4..8].copy_from_slice(&self.end.to_le_bytes());
        b[8..12].copy_from_slice(&self.unwind.to_le_bytes());
        b
    }
}

/// Data-directory indices, in entries rather than bytes.
fn read_exception_directory(pe: &PeFile) -> Vec<RuntimeFunction> {
    let dir =
        fmt::data_directory_offset(pe.opt_header_offset, fmt::IMAGE_DIRECTORY_ENTRY_EXCEPTION);
    let Some(d) = pe.data.get(dir..dir + fmt::DATA_DIRECTORY_ENTRY_SIZE) else {
        return Vec::new();
    };
    let rva = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
    let size = u32::from_le_bytes([d[4], d[5], d[6], d[7]]);
    if rva == 0 || size < fmt::RUNTIME_FUNCTION_SIZE as u32 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(size as usize / fmt::RUNTIME_FUNCTION_SIZE);
    for i in 0..(size / fmt::RUNTIME_FUNCTION_SIZE as u32) {
        let va = pe.rva_to_va(rva + i * fmt::RUNTIME_FUNCTION_SIZE as u32);
        let Some(b) = pe.read_va(va, fmt::RUNTIME_FUNCTION_SIZE) else {
            break;
        };
        if b.len() < fmt::RUNTIME_FUNCTION_SIZE {
            break;
        }
        out.push(RuntimeFunction {
            begin: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            end: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            unwind: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
        });
    }
    out
}

fn pe_checksum(data: &[u8], checksum_offset: usize) -> u32 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        let word = if i + 4 > checksum_offset && i < checksum_offset + 4 {
            0u32
        } else {
            u16::from_le_bytes([data[i], data[i + 1]]) as u32
        };
        sum += word;
        sum = (sum & 0xffff) + (sum >> 16);
        i += 2;
    }
    if data.len() % 2 == 1 {
        sum += data[data.len() - 1] as u32;
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum = (sum & 0xffff) + (sum >> 16);
    sum + data.len() as u32
}

/// Emit all devirtualizable functions and write a patched copy of the PE.
pub fn write_devirt(
    pe: &PeFile,
    entries: &[VmEntry],
    steps: usize,
    timeout_secs: u64,
    vm_section: &str,
    jobs: usize,
    max_blocks: usize,
) -> Result<(Vec<u8>, DevirtReport)> {
    // recover, schedule, and allocate all functions.

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build()
        .context("building the recovery thread pool")?;
    let total = entries.len();
    let width = total.max(1).to_string().len();
    let recovered = AtomicUsize::new(0);
    let prepared: Vec<PreparedFn> = pool.install(|| {
        entries
            .par_iter()
            .map(|e| {
                let started = std::time::Instant::now();
                let prepared = prepare_one(pe, e, steps, timeout_secs, vm_section, max_blocks);
                let elapsed = started.elapsed();
                let n = recovered.fetch_add(1, Ordering::Relaxed) + 1;
                let blocks = prepared.cfg.blocks.len();
                let steps = prepared.cfg.total_steps;
                match &prepared.incomplete {
                    Some(reason) => eprintln!(
                        "  recover [{n:>width$}/{total}] {:#x}: incomplete ({reason}; \
                         {blocks} blocks, {steps} steps, {elapsed:.1?})",
                        prepared.entry_va,
                    ),
                    None => eprintln!(
                        "  recover [{n:>width$}/{total}] {:#x}: ready \
                         ({blocks} blocks, {steps} steps, {elapsed:.1?})",
                        prepared.entry_va,
                    ),
                }
                prepared
            })
            .collect()
    });

    let ready = prepared.iter().filter(|p| p.incomplete.is_none()).count();
    eprintln!(
        "recovery complete: {ready} ready, {} incomplete",
        prepared.len() - ready
    );
    eprintln!("planning output layout for {ready} functions…");

    let fa = pe.file_alignment as usize;
    let sa = pe.section_alignment as usize;
    let last = pe.sections.last().context("no sections")?;
    let devirt_base_va = pe.image_base
        + align_up_u32(
            last.virtual_address + last.virtual_size.max(last.raw_size),
            sa as u32,
        ) as u64;

    // Emit every complete function at its final VA, laying them out as we go.
    let mut offsets: Vec<Option<u32>> = Vec::new();
    let mut emitted: Vec<Vec<u8>> = Vec::new();
    let mut unwind: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut cursor = 0u32;

    // lay out the code, then build the trampoline -> body map from it.
    let mut redirect = codegen::CallRedirects::none();
    {
        let mut probe = 0u32;
        for p in &prepared {
            if p.incomplete.is_some() {
                continue;
            }
            let real_va = devirt_base_va + probe as u64;
            let Ok(out) =
                codegen::emit_function_full(&p.cfg, &p.blocks, &p.allocs, real_va, pe, &redirect)
            else {
                continue;
            };
            redirect.insert(p.entry_va, real_va);
            probe += (out.bytes.len() as u32 + 15) & !15;
        }
    }
    let mut report = DevirtReport {
        retargeted: 0,
        incomplete: Vec::new(),
        failed: Vec::new(),
        unresolved: explore::UnresolvedStats {
            budget: max_blocks,
            ..Default::default()
        },
        dropped_signature: false,
        pdata_entries: 0,
        pdata_added: 0,
        checksum: 0,
    };
    let mut emit_done = 0usize;
    eprintln!("emitting recovered functions…");
    for p in &prepared {
        report.unresolved.accumulate(&p.unresolved);
        if let Some(reason) = &p.incomplete {
            report.incomplete.push((p.entry_va, reason.clone()));
            offsets.push(None);
            emitted.push(Vec::new());
            continue;
        }

        emit_done += 1;
        let real_va = devirt_base_va + cursor as u64;
        let out =
            match codegen::emit_function_full(&p.cfg, &p.blocks, &p.allocs, real_va, pe, &redirect)
            {
                Ok(out) => out,
                Err(e) => {
                    eprintln!(
                        "  emit   [{emit_done:>width$}/{ready}] {:#x}: failed ({e})",
                        p.entry_va,
                    );
                    report.failed.push((p.entry_va, e.to_string()));
                    offsets.push(None);
                    emitted.push(Vec::new());
                    continue;
                }
            };

        let unwind_blob = codegen::unwind_info(&out.prologue);
        unwind.push((cursor, unwind_blob));
        let bytes = out.bytes;
        eprintln!(
            "  emit   [{emit_done:>width$}/{ready}] {:#x} -> {real_va:#x}: {} bytes",
            p.entry_va,
            bytes.len(),
        );
        if std::env::var_os("TVM_MAP").is_some() {
            eprintln!("MAP {:#x} {:#x} {}", p.entry_va, real_va, bytes.len());
        }

        report.retargeted += 1;
        offsets.push(Some(cursor));

        debug_assert_eq!(
            redirect.body_va(p.entry_va),
            Some(real_va),
            "layout probe and emit disagree on where {:#x} lands",
            p.entry_va
        );
        cursor += (bytes.len() as u32 + 15) & !15;
        emitted.push(bytes);
    }
    let code_size = cursor as usize;
    eprintln!("building unwind metadata and patching the image…");

    // Store unwind metadata beside the emitted code in the extended last section.
    let devirt_rva = (devirt_base_va - pe.image_base) as u32;

    let mut unwind_area: Vec<u8> = Vec::new();
    let unwind_base = align_up_u32(code_size as u32, 4);
    // Code offset -> RVA of that function's UNWIND_INFO.
    let mut unwind_rva: Vec<(u32, u32)> = Vec::new();
    for (code_off, blob) in &unwind {
        let at = unwind_base + unwind_area.len() as u32;
        unwind_rva.push((*code_off, devirt_rva + at));
        unwind_area.extend_from_slice(blob);
        while unwind_area.len() % 4 != 0 {
            unwind_area.push(0);
        }
    }

    let mut records = read_exception_directory(pe);
    for (code_off, uw) in &unwind_rva {
        let idx = offsets
            .iter()
            .position(|o| *o == Some(*code_off))
            .expect("every unwind record pairs with an emitted function");
        let len = emitted[idx].len() as u32;
        records.push(RuntimeFunction {
            begin: devirt_rva + code_off,
            end: devirt_rva + code_off + len,
            unwind: *uw,
        });
    }

    records.sort_by_key(|r| r.begin);

    let rf_base = unwind_base + align_up_u32(unwind_area.len() as u32, 4);
    let rf_bytes: Vec<u8> = records.iter().flat_map(|r| r.to_bytes()).collect();
    let exception_rva = devirt_rva + rf_base;
    let exception_size = rf_bytes.len() as u32;

    let section_data_size = rf_base as usize + rf_bytes.len();

    let mut section_data = vec![0u8; section_data_size];
    for b in &mut section_data[..code_size] {
        *b = 0xCC;
    }

    for (idx, bytes) in emitted.iter().enumerate() {
        let Some(off) = offsets[idx] else { continue };
        let off = off as usize;
        section_data[off..off + bytes.len()].copy_from_slice(bytes);
    }

    section_data[unwind_base as usize..unwind_base as usize + unwind_area.len()]
        .copy_from_slice(&unwind_area);
    section_data[rf_base as usize..rf_base as usize + rf_bytes.len()].copy_from_slice(&rf_bytes);

    report.pdata_entries = records.len();
    report.pdata_added = unwind_rva.len();

    // splice the payload into the last section.
    let va_start = align_up_u32(
        last.virtual_address + last.virtual_size.max(last.raw_size),
        sa as u32,
    );
    let body_delta = (va_start - last.virtual_address) as usize;
    let devirt_file_off = last.raw_address as usize + body_delta;

    let mut out = pe.data.clone();
    let raw_size = align_up_u32(section_data_size as u32, fa as u32);
    let needed = devirt_file_off + raw_size as usize;
    if out.len() < needed {
        out.resize(needed, 0);
    }
    out[devirt_file_off..devirt_file_off + section_data.len()].copy_from_slice(&section_data);

    for b in &mut out[devirt_file_off + section_data.len()..needed] {
        *b = 0;
    }

    let vsize = section_data_size as u32;

    let last_idx = pe.sections.len() - 1;
    let last_hdr = pe.section_table_offset + last_idx * fmt::SECTION_HEADER_SIZE;
    if last_hdr + fmt::SECTION_HEADER_SIZE > pe.size_of_headers as usize {
        bail!(
            "the last section header at {:#x} is not inside the {:#x} bytes reserved \
             for headers",
            last_hdr,
            pe.size_of_headers,
        );
    }
    write_u32_at(&mut out, last_hdr + 8, body_delta as u32 + vsize);
    write_u32_at(&mut out, last_hdr + 16, body_delta as u32 + raw_size);

    // The section should be RX.
    let chars = last_hdr + fmt::SECTION_CHARACTERISTICS_OFFSET;
    let old_chars = u32::from_le_bytes(out[chars..chars + 4].try_into().unwrap());
    write_u32_at(
        &mut out,
        chars,
        old_chars | fmt::IMAGE_SCN_CNT_CODE | fmt::IMAGE_SCN_MEM_EXECUTE | fmt::IMAGE_SCN_MEM_READ,
    );

    // Clear invalid authenticode signature.
    let sec_dir =
        fmt::data_directory_offset(pe.opt_header_offset, fmt::IMAGE_DIRECTORY_ENTRY_SECURITY);
    let had_signature = u32::from_le_bytes(out[sec_dir + 4..sec_dir + 8].try_into().unwrap()) != 0;
    write_u32_at(&mut out, sec_dir, 0);
    write_u32_at(&mut out, sec_dir + 4, 0);
    report.dropped_signature = had_signature;

    // Repoint the exception directory at the array.
    let exc_dir =
        fmt::data_directory_offset(pe.opt_header_offset, fmt::IMAGE_DIRECTORY_ENTRY_EXCEPTION);
    write_u32_at(&mut out, exc_dir, exception_rva);
    write_u32_at(&mut out, exc_dir + 4, exception_size);

    // Update SizeOfImage in the optional header.
    let size_off = pe.opt_header_offset + fmt::OPTIONAL_HEADER_SIZE_OF_IMAGE_OFFSET;
    let new_size = align_up_u32(va_start + vsize, sa as u32);
    write_u32_at(&mut out, size_off, new_size);

    // patch the trampolines.
    for (idx, entry) in entries.iter().enumerate() {
        let Some(off) = offsets[idx] else { continue };
        let tramp_off = match pe
            .va_to_rva(entry.trampoline_va)
            .and_then(|rva| pe.section_for_rva(rva))
            .map(|s| {
                s.raw_address + (entry.trampoline_va - pe.image_base) as u32 - s.virtual_address
            }) {
            Some(o) => o as usize,
            None => continue,
        };

        let devirt_va = devirt_base_va + off as u64;
        let tramp_va = entry.trampoline_va;
        // Patch: E9 <rel32> where rel32 = devirt_va - (tramp_va + 5)
        let rel: i64 = devirt_va as i64 - (tramp_va as i64 + 5);
        if rel < i32::MIN as i64 || rel > i32::MAX as i64 {
            // More than 2 GiB away; can't use a rel32 JMP, Skip.
            continue;
        }
        out[tramp_off] = 0xE9;
        write_u32_at(&mut out, tramp_off + 1, rel as i32 as u32);
    }

    // recompute the PE checksum over the fully-patched image.
    let ck_off = pe.opt_header_offset + fmt::OPTIONAL_HEADER_CHECKSUM_OFFSET;
    write_u32_at(&mut out, ck_off, 0); // field must read zero during the sum
    let cksum = pe_checksum(&out, ck_off);
    write_u32_at(&mut out, ck_off, cksum);
    report.checksum = cksum;

    Ok((out, report))
}

/// Recover + schedule + allocate one function, returning reusable parts.
struct PreparedFn {
    entry_va: u64,
    incomplete: Option<String>,
    cfg: ir::Cfg,
    blocks: Vec<sched::SchedBlock>,
    allocs: Vec<regalloc::Alloc>,
    unresolved: explore::UnresolvedStats,
}

fn prepare_one(
    pe: &PeFile,
    entry: &VmEntry,
    steps: usize,
    timeout_secs: u64,
    vm_section: &str,
    max_blocks: usize,
) -> PreparedFn {
    let mut ex = explore::Explorer::with_vm_section(pe, crate::vm::DEFAULT_STACK_BASE, vm_section);
    ex.step_budget = steps;
    ex.time_budget = std::time::Duration::from_secs(timeout_secs);
    ex.block_budget = max_blocks;

    let cfg = ex.recover(entry.vm_entry_va);
    let blocks = sched::schedule(&cfg);
    let entry_va = entry.trampoline_va;
    let unresolved = explore::UnresolvedStats::of(&cfg, max_blocks);
    let incomplete = why_incomplete(&cfg, &unresolved);
    let allocs: Vec<regalloc::Alloc> = blocks.iter().map(regalloc::allocate).collect();

    PreparedFn {
        entry_va,
        incomplete,
        cfg,
        blocks,
        allocs,
        unresolved,
    }
}

/// Why a recovered graph cannot be retargeted, or `None` when it can.
fn why_incomplete(cfg: &ir::Cfg, unresolved: &explore::UnresolvedStats) -> Option<String> {
    if cfg.blocks.is_empty() {
        return Some("no blocks recovered".to_string());
    }
    if cfg.timed_out {
        return Some("recovery timed out".to_string());
    }
    if cfg.unresolved != 0 {
        return Some(format!("{} block(s) never resolved", unresolved.render()));
    }
    let issues = explore::validate(cfg);
    if !issues.is_clean() {
        return Some(format!("CFG invariant: {}", issues.summary()));
    }
    None
}

fn align_up_u32(v: u32, align: u32) -> u32 {
    (v + align - 1) / align * align
}

fn write_u32_at(buf: &mut Vec<u8>, off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::expr::{Arena, BlockRef, Reg};

    /// A graph with `real` ordinary blocks and `truncated` blocks carrying the
    /// block-budget reason, which is what raising the cap past `real` produces.
    fn graph(real: usize, truncated: usize) -> ir::Cfg {
        let mut arena = Arena::default();
        let entry = ir::BlockId {
            handler: 0x1000,
            vip: None,
        };
        let mut blocks = Vec::new();
        for i in 0..real {
            blocks.push(ir::Block {
                id: ir::BlockId {
                    handler: 0x1000 + i as u64 * 0x10,
                    vip: None,
                },
                block_ref: BlockRef(i as u32),
                params: Vec::new(),
                events: Vec::new(),
                from_vm_context: false,
                exit_regs: vec![(Reg::Rax, arena.init_reg(Reg::Rax))],
                terminator: if i + 1 == real {
                    ir::Terminator::TailCall { target: 0x2000 }
                } else {
                    ir::Terminator::Jump(ir::BlockId {
                        handler: 0x1000 + (i as u64 + 1) * 0x10,
                        vip: None,
                    })
                },
                cost: 1,
                preds: if i == 0 { Vec::new() } else { Vec::new() },
            });
        }
        for t in 0..truncated {
            blocks.push(ir::Block {
                id: ir::BlockId {
                    handler: 0x9000 + t as u64 * 0x10,
                    vip: None,
                },
                block_ref: BlockRef((real + t) as u32),
                params: Vec::new(),
                events: Vec::new(),
                from_vm_context: false,
                exit_regs: Vec::new(),
                terminator: ir::Terminator::Unresolved {
                    reason: explore::BLOCK_BUDGET_REASON.into(),
                },
                cost: 0,
                preds: Vec::new(),
            });
        }
        // `validate` walks predecessors, so thread them for the linear chain.
        for i in 1..real {
            let prev = blocks[i - 1].id;
            blocks[i].preds = vec![prev];
        }
        ir::Cfg {
            entry,
            arena,
            blocks,
            total_steps: real,
            unresolved: truncated,
            back_edges: Vec::new(),
            trivial_phis_removed: 0,
            timed_out: false,
            stack_base: crate::vm::DEFAULT_STACK_BASE,
            vm_range: (0, 0),
        }
    }

    /// Raising the cap must turn a truncated graph into a complete one, and must
    /// never turn a recovery failure into a truncation: the two are counted apart
    /// and reported apart, because only one of them is the caller's to fix.
    #[test]
    fn raising_the_block_cap_separates_truncation_from_recovery_failure() {
        // Two real blocks recovered under a cap of one: the second is truncated.
        let capped = graph(1, 1);
        let stats = explore::UnresolvedStats::of(&capped, 1);
        assert_eq!(stats.total, 1);
        assert_eq!(stats.truncated, 1, "the dropped block carries the budget reason");
        assert_eq!(stats.genuine, 0, "nothing about recovery failed");
        assert_eq!(stats.families.block_budget, 1);
        assert_eq!(stats.predicted_truncated, 1);
        assert_eq!(stats.truncated, stats.predicted_truncated);
        // And it must not be reported as complete.
        let reason = why_incomplete(&capped, &stats)
            .expect("a budget-truncated graph must never be treated as complete");
        assert!(
            reason.contains("block-budget truncated=1"),
            "the reason must name truncation, got {reason:?}"
        );
        assert!(
            reason.contains("genuine=0"),
            "the reason must not blame recovery, got {reason:?}"
        );

        // The same function under a cap that fits it: nothing is truncated and it
        // is no longer reported as incomplete.
        let raised = graph(2, 0);
        let stats = explore::UnresolvedStats::of(&raised, 8);
        assert_eq!(stats.total, 0);
        assert_eq!(stats.truncated, 0);
        assert_eq!(stats.genuine, 0);
        assert_eq!(stats.predicted_truncated, 0);
        assert_eq!(why_incomplete(&raised, &stats), None);
    }

    /// A block that recovery genuinely could not resolve stays a failure at every
    /// cap: raising the budget must never reclassify it as truncation.
    #[test]
    fn a_genuine_failure_is_not_reclassified_by_a_raised_cap() {
        let mut cfg = graph(2, 0);
        cfg.blocks[1].terminator = ir::Terminator::Unresolved {
            reason: "unsupported at 0x1234: vmxon".into(),
        };
        cfg.unresolved = 1;

        for cap in [1usize, 2, 512, 4096] {
            let stats = explore::UnresolvedStats::of(&cfg, cap);
            assert_eq!(stats.truncated, 0, "cap {cap} invented a truncation");
            assert_eq!(stats.genuine, 1, "cap {cap} lost the genuine failure");
            assert_eq!(stats.families.unsupported, 1);
            let reason = why_incomplete(&cfg, &stats).expect("a failure is still incomplete");
            assert!(
                reason.contains("genuine=1"),
                "cap {cap} reported the failure as {reason:?}"
            );
        }
    }
}
