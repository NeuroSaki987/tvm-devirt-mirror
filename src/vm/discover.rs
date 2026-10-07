use crate::binary::disasm::decode_at;
use crate::binary::pe::PeFile;
use std::collections::HashMap;

/// Why a discovered location is *not* a virtualized function entry.
///
/// find_vm_entries stays deliberately permissive -- a missed entry cannot be
/// recovered later -- so it also reports locations that merely have a
/// trampoline's shape. Anything that publishes a *count* (the `entries` census,
/// and the coverage denominators built from it) has to drop these first, or the
/// denominator is inflated by locations that can never be recovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StubKind {
    /// The .pdata record covering the location has a body that is entirely
    /// int3. These are the split-off records inside another trampoline's
    /// padding tail: several VAs, one VM target, `int3 pad 0`, and `dis` shows
    /// nothing but CC.
    Int3Padding,
    /// The bytes at the location are CD 29 -- `int 29h`, the `__fastfail`
    /// sequence -- not a jump.
    FastFail29h,
    /// A second location for a VM target already claimed by a trampoline that
    /// does carry padding, with no padding of its own.
    ZeroPadAlias,
    /// An `E9` into the VM section with no `int3` tail at all. TVM replaces a
    /// function body with `E9 rel32` and fills the remainder of the original
    /// body with `int3`, so a trampoline-shaped location with a zero-length
    /// tail is not one of them.
    NoInt3Tail,
}

impl StubKind {
    pub fn label(self) -> &'static str {
        match self {
            StubKind::Int3Padding => "int3-padding",
            StubKind::FastFail29h => "fastfail-29h",
            StubKind::ZeroPadAlias => "zero-pad-alias",
            StubKind::NoInt3Tail => "no-int3-tail",
        }
    }
}

/// Decide whether a candidate is a stub rather than a function entry.
///
/// Pure, so the rule can be unit-tested without a PE image.
///
/// * `body_all_int3` -- the covering .pdata record's body is entirely `CC`,
///   which proves the location is a padding record, not code.
/// * `int3_padding` -- the tail length measured at the location itself.
/// * `target_has_padded_claim` -- another entry with a non-empty `int3` tail
///   already reaches the same VM target, so this one is a second name for it.
fn classify_stub(
    head: &[u8],
    body_all_int3: bool,
    int3_padding: usize,
    target_has_padded_claim: bool,
) -> Option<StubKind> {
    if body_all_int3 {
        return Some(StubKind::Int3Padding);
    }
    if head.len() >= 2 && head[0] == 0xCD && head[1] == 0x29 {
        return Some(StubKind::FastFail29h);
    }
    // No `int3` tail: the location does not have a TVM trampoline's shape.
    if int3_padding == 0 {
        return Some(if target_has_padded_claim {
            StubKind::ZeroPadAlias
        } else {
            StubKind::NoInt3Tail
        });
    }
    None
}

/// Body of the .pdata record that covers `va`, if any record does.
fn pdata_body<'a>(pe: &'a PeFile, table: &[(u64, u64)], va: u64) -> Option<&'a [u8]> {
    let idx = match table.binary_search_by_key(&va, |&(begin, _)| begin) {
        Ok(i) => i,
        Err(0) => return None,
        Err(i) => i - 1,
    };
    let (begin, end) = table[idx];
    if va < begin || va >= end {
        return None;
    }
    let len = (end - begin) as usize;
    if len == 0 {
        return None;
    }
    pe.read_va(begin, len)
}

/// Mark every entry that is not a function entry. Runs after the dedup so the
/// alias test sees the final set.
fn mark_stubs(pe: &PeFile, table: &[(u64, u64)], entries: &mut [VmEntry]) {
    // VM targets already claimed by a trampoline that has padding of its own.
    let mut padded: HashMap<u64, usize> = HashMap::new();
    for e in entries.iter() {
        if e.int3_padding > 0 {
            *padded.entry(e.vm_entry_va).or_insert(0) += 1;
        }
    }

    for e in entries.iter_mut() {
        if e.stub.is_some() {
            continue; // the swallow pass already proved the body is all int3
        }
        let head = pe.read_va(e.trampoline_va, 2).unwrap_or_default();
        let body_all_int3 = pdata_body(pe, table, e.trampoline_va)
            .is_some_and(|b| !b.is_empty() && b.iter().all(|&x| x == 0xCC));
        let claimed = padded.get(&e.vm_entry_va).copied().unwrap_or(0) > 0;
        e.stub = classify_stub(&head, body_all_int3, e.int3_padding, claimed);
    }
}

/// Whether a candidate `E9` at `va` is contradicted by the instruction stream.
///
/// A real trampoline is the *first* instruction of a function, so it always sits
/// on an instruction boundary. A chance `E9` byte inside some other instruction's
/// encoding does not. That difference is the only thing separating the two, and
/// `.pdata` gives us a trustworthy place to start decoding from to tell them
/// apart.
///
/// Returns `true` only with positive evidence of a mid-instruction hit: the sweep
/// from the enclosing function's start stepped *over* `va` without landing on it.
/// Three cases, and only the second is a rejection:
///
/// * sweep lands on `va`; real boundary, keep.
/// * sweep steps over `va`; inside another instruction, reject.
/// * no enclosing record, or the sweep hits undecodable bytes before reaching
///   `va`; no evidence either way, keep.
fn is_mid_instruction(pe: &PeFile, table: &[(u64, u64)], va: u64) -> bool {
    // Enclosing record: the last one that begins at or before `va`. The table is
    // sorted, and entries do not overlap.
    let idx = match table.binary_search_by_key(&va, |&(begin, _)| begin) {
        Ok(i) => i,
        Err(0) => return false,
        Err(i) => i - 1,
    };
    let (begin, end) = table[idx];
    if va < begin || va >= end {
        return false; // not covered by .pdata
    }

    let mut ip = begin;
    while ip < va {
        match decode_at(pe, ip) {
            Some(i) => ip = i.next_ip(),
            // Undecodable bytes before `va`: the sweep proves nothing past here.
            None => return false,
        }
    }
    // Overshot `va`, so some instruction's encoding contains it.
    ip != va
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmEntry {
    /// VA of the `E9` trampoline (or the all-`int3` body start for a swallowed
    /// function) in the original code section.
    pub trampoline_va: u64,
    /// VA inside the VM section that the trampoline jumps to.
    pub vm_entry_va: u64,
    /// Number of `int3` bytes directly following the `E9 rel32`.
    /// Zero for swallowed functions that have no `E9` of their own.
    pub int3_padding: usize,
    /// `Some` when the location is provably not a function entry. Discovery
    /// still reports it (recovery must not silently lose a real entry); only
    /// the census drops it.
    pub stub: Option<StubKind>,
}

impl VmEntry {
    /// True when this location is a stub rather than a virtualized function.
    pub fn is_stub(&self) -> bool {
        self.stub.is_some()
    }
}

/// Scan every executable section other than the VM section for entry
/// trampolines pointing into the VM section.
pub fn find_vm_entries(pe: &PeFile, vm_section: &str) -> Vec<VmEntry> {
    let Some(vm) = pe.section_by_name(vm_section) else {
        return Vec::new();
    };
    let vm_lo = pe.rva_to_va(vm.virtual_address);
    let vm_hi = vm_lo + vm.virtual_size.max(vm.raw_size) as u64;

    let table = pe.function_table();

    let mut out = Vec::new();
    for sec in &pe.sections {
        if sec.name == vm_section || !sec.is_executable() {
            continue;
        }
        let bytes = pe.section_bytes(sec);
        let base = pe.rva_to_va(sec.virtual_address);

        let mut i = 0usize;
        while i + 5 <= bytes.len() {
            if bytes[i] != 0xE9 {
                i += 1;
                continue;
            }
            let rel = i32::from_le_bytes(bytes[i + 1..i + 5].try_into().unwrap());
            let va = base + i as u64;
            let target = va.wrapping_add(5).wrapping_add(rel as i64 as u64);
            if !(vm_lo..vm_hi).contains(&target) {
                i += 1;
                continue;
            }
            // Reject chance `E9` bytes that live inside another instruction.
            if is_mid_instruction(pe, &table, va) {
                i += 1;
                continue;
            }
            let padding = if i + 5 < bytes.len() {
                bytes[i + 5..].iter().take_while(|&&b| b == 0xCC).count()
            } else {
                0
            };
            out.push(VmEntry {
                trampoline_va: va,
                vm_entry_va: target,
                int3_padding: padding,
                stub: None, // classified after the second pass
            });
            // Skip past the trampoline and its padding: nothing else lives there.
            i += 5 + padding.max(1);
        }
    }

    // Second pass: pdata-based discovery for "swallowed" functions.
    let tramp_set: Vec<(u64, u64, u64)> = out
        .iter()
        .map(|e| {
            (
                e.trampoline_va,
                e.trampoline_va + 5 + e.int3_padding as u64,
                e.vm_entry_va,
            )
        })
        .collect();
    let tramp_vas: std::collections::HashSet<u64> = out.iter().map(|e| e.trampoline_va).collect();

    for &(fn_begin, fn_end) in &table {
        // Already discovered as a trampoline.
        if tramp_vas.contains(&fn_begin) {
            continue;
        }
        // Skip functions in the VM section itself.
        if fn_begin >= vm_lo && fn_begin < vm_hi {
            continue;
        }
        // Body must be entirely int3 to qualify as swallowed.
        let len = fn_end.saturating_sub(fn_begin) as usize;
        if len == 0 {
            continue;
        }
        let Some(body) = pe.read_va(fn_begin, len) else {
            continue;
        };
        if body.len() != len || body.iter().any(|&b| b != 0xCC) {
            continue;
        }
        // Must be contained within one trampoline's padding region.
        let Some(&(_, _, vm_entry_va)) = tramp_set
            .iter()
            .find(|&&(ts, te, _)| ts <= fn_begin && fn_end <= te)
        else {
            continue;
        };
        out.push(VmEntry {
            trampoline_va: fn_begin,
            vm_entry_va,
            int3_padding: 0, // no E9 of its own
            // The pass only reaches here with a body that is entirely int3, so
            // the record is padding that .pdata split off, not a function.
            stub: Some(StubKind::Int3Padding),
        });
    }

    out.sort_by_key(|e| e.trampoline_va);
    out.dedup_by_key(|e| e.trampoline_va);
    mark_stubs(pe, &table, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const JMP: [u8; 5] = [0xE9, 0x00, 0x10, 0x00, 0x00];

    #[test]
    fn real_trampoline_is_kept() {
        // E9 rel32 with a non-empty int3 tail: a genuine entry.
        assert_eq!(classify_stub(&JMP, false, 1, false), None);
        assert_eq!(classify_stub(&JMP, false, 187, false), None);
        // ...even when a second name for the same target exists.
        assert_eq!(classify_stub(&JMP, false, 187, true), None);
    }

    #[test]
    fn all_int3_body_is_padding_not_a_function() {
        assert_eq!(
            classify_stub(&[0xCC, 0xCC], true, 0, true),
            Some(StubKind::Int3Padding)
        );
        // The body verdict wins over every other signal.
        assert_eq!(
            classify_stub(&[0xCD, 0x29], true, 0, false),
            Some(StubKind::Int3Padding)
        );
    }

    #[test]
    fn fastfail_head_is_a_stub() {
        assert_eq!(
            classify_stub(&[0xCD, 0x29, 0x00, 0x00], false, 0, false),
            Some(StubKind::FastFail29h)
        );
        // A lone CD, or CD followed by something else, is not int 29h.
        assert_eq!(classify_stub(&[0xCD], false, 1, false), None);
        assert_eq!(classify_stub(&[0xCD, 0x2A], false, 1, false), None);
    }

    #[test]
    fn empty_int3_tail_is_not_a_trampoline() {
        // Same target reached by a padded trampoline: a second name for it.
        assert_eq!(
            classify_stub(&JMP, false, 0, true),
            Some(StubKind::ZeroPadAlias)
        );
        // Sole claimant of its target, but still without a TVM trampoline's
        // shape (E9 rel32 + int3 fill).
        assert_eq!(
            classify_stub(&JMP, false, 0, false),
            Some(StubKind::NoInt3Tail)
        );
    }

    #[test]
    fn head_bytes_alone_never_decide() {
        // int3 bytes at the head are only a stub when the covering .pdata body
        // says so; the census must not reject on the first two bytes.
        assert_eq!(classify_stub(&[0xCC, 0xCC], false, 3, false), None);
    }
}
