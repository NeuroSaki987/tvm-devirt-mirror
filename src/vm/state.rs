//! Abstract machine state for the guided evaluator. Registers and flags hold [`expr::Ref`]s, so a value is "concrete" exactly when its node is `Op::Const`.

use crate::ir::expr::{Arena, BinOp, Ref, Reg, Width};
use std::collections::HashMap;

/// x86 status flags the lifter models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flag {
    Cf,
    Pf,
    Af,
    Zf,
    Sf,
    Of,
    /// Dir flag; needed to keep `pushfq`/`popfq` round-trips exact.
    Df,
}

pub const FLAGS: [Flag; 7] = [
    Flag::Cf,
    Flag::Pf,
    Flag::Af,
    Flag::Zf,
    Flag::Sf,
    Flag::Of,
    Flag::Df,
];

impl Flag {
    /// Bit position inside RFLAGS.
    pub fn bit(self) -> u32 {
        match self {
            Flag::Cf => 0,
            Flag::Pf => 2,
            Flag::Af => 4,
            Flag::Zf => 6,
            Flag::Sf => 7,
            Flag::Df => 10,
            Flag::Of => 11,
        }
    }
}

/// RFLAGS bits that are always set on x86 (bit 1 is reserved-as-1).
pub const RFLAGS_FIXED: u64 = 0x2;

/// One byte of memory: either a known constant or a byte extracted from a
/// symbolic expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Byte {
    Const(u8),
    /// Byte `index` (little-endian) of the value `expr`.
    Sym {
        expr: Ref,
        index: u32,
    },
}

#[derive(Debug, Clone)]
pub struct State {
    pub regs: HashMap<Reg, Ref>,
    pub flags: HashMap<Flag, Ref>,
    /// Symbolic/overwritten memory, keyed by concrete address.
    pub mem: HashMap<u64, Byte>,
    /// Stores whose address did not fold to a constant. Kept so the lifter can
    /// see that it lost precision instead of silently dropping the store.
    pub sym_stores: Vec<(Ref, Ref, Width)>,
    /// XMM registers, as a low and a high 64-bit half. TVM boxes every SSE instruction, and many are plain 16-byte data moves.
    pub xmm: HashMap<(u8, XmmHalf), Ref>,
}

/// Which 64-bit half of an XMM register. SSE moves are 8- or 16-byte, so halves
/// are enough; anything narrower stays boxed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum XmmHalf {
    Low,
    High,
}

impl State {
    /// Fresh state: all GPRs are `InitReg`, all flags symbolic, RSP concrete.
    pub fn new(arena: &mut Arena, stack_base: u64) -> Self {
        let mut regs = HashMap::new();
        for r in crate::ir::expr::GPRS {
            let v = arena.init_reg(r);
            regs.insert(r, v);
        }
        let sp = arena.constant(stack_base, Width::W64);
        regs.insert(Reg::Rsp, sp);

        let mut flags = HashMap::new();
        for f in FLAGS {
            // Guest flags on entry are unknown but DF is 0 by the Windows ABI.
            let v = if f == Flag::Df {
                arena.constant(0, Width::W8)
            } else {
                arena.opaque("init_flag", Width::W8)
            };
            flags.insert(f, v);
        }

        Self {
            regs,
            flags,
            mem: HashMap::new(),
            sym_stores: Vec::new(),
            xmm: HashMap::new(),
        }
    }

    pub fn reg(&self, r: Reg) -> Ref {
        self.regs[&r]
    }

    pub fn set_reg(&mut self, r: Reg, v: Ref) {
        self.regs.insert(r, v);
    }

    /// One half of an XMM register, or `None` if it was never written.
    pub fn xmm(&self, index: u8, half: XmmHalf) -> Option<Ref> {
        self.xmm.get(&(index, half)).copied()
    }

    pub fn set_xmm(&mut self, index: u8, half: XmmHalf, v: Ref) {
        self.xmm.insert((index, half), v);
    }

    /// Drop all knowledge of an XMM register, for an instruction that writes one
    /// in a way this does not model.
    pub fn forget_xmm(&mut self, index: u8) {
        self.xmm.remove(&(index, XmmHalf::Low));
        self.xmm.remove(&(index, XmmHalf::High));
    }

    pub fn flag(&self, f: Flag) -> Ref {
        self.flags[&f]
    }

    pub fn set_flag(&mut self, f: Flag, v: Ref) {
        self.flags.insert(f, v);
    }

    /// Every expression reference retained by the architectural machine state.
    /// Diagnostics and arena compaction must treat all of these as live roots.
    pub fn symbolic_roots(&self) -> Vec<Ref> {
        let mut roots = Vec::with_capacity(
            self.regs.len()
                + self.flags.len()
                + self.mem.len()
                + self.sym_stores.len() * 2
                + self.xmm.len(),
        );
        roots.extend(self.regs.values().copied());
        roots.extend(self.flags.values().copied());
        roots.extend(self.mem.values().filter_map(|byte| match byte {
            Byte::Sym { expr, .. } => Some(*expr),
            Byte::Const(_) => None,
        }));
        for (addr, value, _) in &self.sym_stores {
            roots.push(*addr);
            roots.push(*value);
        }
        roots.extend(self.xmm.values().copied());
        roots
    }

    /// Replace every expression reference retained by the machine state.
    pub(crate) fn remap_refs(&mut self, remap: &HashMap<Ref, Ref>) {
        let mapped = |r: Ref| {
            *remap
                .get(&r)
                .expect("machine-state root missing from arena compaction map")
        };
        for value in self.regs.values_mut() {
            *value = mapped(*value);
        }
        for value in self.flags.values_mut() {
            *value = mapped(*value);
        }
        for byte in self.mem.values_mut() {
            if let Byte::Sym { expr, .. } = byte {
                *expr = mapped(*expr);
            }
        }
        for (addr, value, _) in &mut self.sym_stores {
            *addr = mapped(*addr);
            *value = mapped(*value);
        }
        for value in self.xmm.values_mut() {
            *value = mapped(*value);
        }
    }

    /// Concrete RSP, when it is one. VM exits are detected off this.
    pub fn concrete_rsp(&self, arena: &Arena) -> Option<u64> {
        arena.as_const(self.reg(Reg::Rsp))
    }

    /// Pack the modelled flags into an RFLAGS value for `pushfq`.
    pub fn pack_flags(&self, arena: &mut Arena) -> Ref {
        let mut acc = arena.constant(RFLAGS_FIXED, Width::W64);
        for f in FLAGS {
            let bit = self.flag(f);
            let wide = arena.zext(bit, Width::W64);
            let one = arena.constant(1, Width::W64);
            let masked = arena.bin(BinOp::And, wide, one);
            let shift = arena.constant(f.bit() as u64, Width::W64);
            let placed = arena.bin(BinOp::Shl, masked, shift);
            acc = arena.bin(BinOp::Or, acc, placed);
        }
        acc
    }

    /// Unpack an RFLAGS value into the modelled flags for `popfq`.
    pub fn unpack_flags(&mut self, arena: &mut Arena, value: Ref) {
        for f in FLAGS {
            let shift = arena.constant(f.bit() as u64, Width::W64);
            let shifted = arena.bin(BinOp::Shr, value, shift);
            let one = arena.constant(1, Width::W64);
            let bit = arena.bin(BinOp::And, shifted, one);
            let narrow = arena.trunc(bit, Width::W8);
            self.set_flag(f, narrow);
        }
    }

    /// Store `value` of `width` at a concrete address.
    pub fn store_concrete(&mut self, arena: &Arena, addr: u64, value: Ref, width: Width) {
        let konst = arena.as_const(value);
        for i in 0..width.bytes() {
            let b = match konst {
                Some(c) => Byte::Const((c >> (i * 8)) as u8),
                None => Byte::Sym {
                    expr: value,
                    index: i,
                },
            };
            self.mem.insert(addr.wrapping_add(i as u64), b);
        }
    }

    pub fn forget_concrete(&mut self, addr: u64, bytes: u32) {
        for i in 0..bytes {
            self.mem.remove(&addr.wrapping_add(i as u64));
        }
    }

    /// Load `width` bytes from a concrete address. `image` supplies bytes that
    /// have never been written (i.e. the original PE contents).
    pub fn load_concrete(
        &self,
        arena: &mut Arena,
        addr: u64,
        width: Width,
        image: impl Fn(u64) -> Option<u8>,
    ) -> Option<Ref> {
        let mut bytes = Vec::with_capacity(width.bytes() as usize);
        for i in 0..width.bytes() {
            let a = addr.wrapping_add(i as u64);
            let b = match self.mem.get(&a) {
                Some(&b) => b,
                None => Byte::Const(image(a)?),
            };
            bytes.push(b);
        }

        // All constant: fold to an immediate.
        // Turn bytecode/dispatch-table reads into constants.
        if bytes.iter().all(|b| matches!(b, Byte::Const(_))) {
            let mut v = 0u64;
            for (i, b) in bytes.iter().enumerate() {
                if let Byte::Const(c) = b {
                    v |= (*c as u64) << (i * 8);
                }
            }
            return Some(arena.constant(v, width));
        }

        // A contiguous, correctly ordered run of one symbolic value: hand it
        // back directly (possibly truncated) instead of rebuilding it.
        if let Byte::Sym { expr, index: 0 } = bytes[0] {
            let contiguous = bytes
                .iter()
                .enumerate()
                .all(|(i, b)| matches!(b, Byte::Sym { expr: e, index } if *e == expr && *index == i as u32));
            if contiguous {
                return Some(arena.trunc(expr, width));
            }
        }

        let n = bytes.len();
        let mut acc: Option<Ref> = None;
        let mut i = 0;
        while i < n {
            let mut j = i + 1;
            let term = match bytes[i] {
                Byte::Const(_) => {
                    while j < n && matches!(bytes[j], Byte::Const(_)) {
                        j += 1;
                    }
                    let mut v = 0u64;
                    for (k, b) in bytes[i..j].iter().enumerate() {
                        if let Byte::Const(c) = b {
                            v |= (*c as u64) << (k * 8);
                        }
                    }
                    // Zero bytes contribute nothing to an `Or`.
                    if v == 0 {
                        i = j;
                        continue;
                    }
                    arena.constant(v << (i * 8), width)
                }
                Byte::Sym { expr, index } => {
                    while j < n
                        && matches!(bytes[j], Byte::Sym { expr: e, index: ix }
                                    if e == expr && ix as usize == index as usize + (j - i))
                    {
                        j += 1;
                    }
                    let len = j - i;
                    // Bring the source to the load's width before shifting, so the
                    // shifts and masks below are all one width.
                    let src_w = arena.width(expr);
                    let mut t = if src_w == width {
                        expr
                    } else if src_w.bits() < width.bits() {
                        arena.zext(expr, width)
                    } else {
                        arena.trunc(expr, width)
                    };
                    // Select bytes `index .. index+len` of the source ...
                    if index > 0 {
                        let sh = arena.constant(index as u64 * 8, width);
                        t = arena.bin(BinOp::Shr, t, sh);
                    }
                    // ... masking only when the run is narrower than the result, so a
                    // run covering the whole load needs no mask at all.
                    if len * 8 < width.bits() as usize {
                        let mask = arena.constant((1u64 << (len * 8)) - 1, width);
                        t = arena.bin(BinOp::And, t, mask);
                    }
                    // ... and place them at their offset in the slot.
                    if i > 0 {
                        let sh = arena.constant(i as u64 * 8, width);
                        t = arena.bin(BinOp::Shl, t, sh);
                    }
                    t
                }
            };
            acc = Some(match acc {
                None => term,
                Some(a) => arena.bin(BinOp::Or, a, term),
            });
            i = j;
        }
        // Every byte was a zero constant.
        Some(match acc {
            Some(a) => a,
            None => arena.constant(0, width),
        })
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn symbolic_roots_include_every_ref_held_by_machine_state() {
        let mut arena = Arena::new();
        let mut state = State::new(&mut arena, 0x7fff_ffff_0000);
        let reg = arena.opaque("reg-root", Width::W64);
        let flag = arena.opaque("flag-root", Width::W8);
        let mem = arena.opaque("mem-root", Width::W64);
        let store_addr = arena.opaque("store-addr-root", Width::W64);
        let store_value = arena.opaque("store-value-root", Width::W32);
        let xmm = arena.opaque("xmm-root", Width::W64);

        state.regs.clear();
        state.flags.clear();
        state.mem.clear();
        state.sym_stores.clear();
        state.xmm.clear();
        state.regs.insert(Reg::Rax, reg);
        state.flags.insert(Flag::Zf, flag);
        state.mem.insert(
            0x1000,
            Byte::Sym {
                expr: mem,
                index: 0,
            },
        );
        state.sym_stores.push((store_addr, store_value, Width::W32));
        state.xmm.insert((0, XmmHalf::Low), xmm);

        let roots: HashSet<_> = state.symbolic_roots().into_iter().collect();
        assert_eq!(
            roots,
            HashSet::from([reg, flag, mem, store_addr, store_value, xmm])
        );
    }

    #[test]
    fn remap_refs_updates_every_machine_state_holder() {
        let mut arena = Arena::new();
        let mut state = State::new(&mut arena, 0x7fff_ffff_0000);
        let old: Vec<Ref> = (0..6)
            .map(|_| arena.opaque("old-state-root", Width::W64))
            .collect();
        let new: Vec<Ref> = (0..6)
            .map(|_| arena.opaque("new-state-root", Width::W64))
            .collect();

        state.regs.clear();
        state.flags.clear();
        state.mem.clear();
        state.sym_stores.clear();
        state.xmm.clear();
        state.regs.insert(Reg::Rax, old[0]);
        state.flags.insert(Flag::Zf, old[1]);
        state.mem.insert(
            0x1000,
            Byte::Sym {
                expr: old[2],
                index: 3,
            },
        );
        state.sym_stores.push((old[3], old[4], Width::W32));
        state.xmm.insert((2, XmmHalf::High), old[5]);
        let remap: HashMap<Ref, Ref> = old.iter().copied().zip(new.iter().copied()).collect();

        state.remap_refs(&remap);

        assert_eq!(state.regs[&Reg::Rax], new[0]);
        assert_eq!(state.flags[&Flag::Zf], new[1]);
        assert_eq!(
            state.mem[&0x1000],
            Byte::Sym {
                expr: new[2],
                index: 3
            }
        );
        assert_eq!(state.sym_stores, vec![(new[3], new[4], Width::W32)]);
        assert_eq!(state.xmm[&(2, XmmHalf::High)], new[5]);
    }

    #[test]
    fn remap_refs_panics_on_a_missing_declared_root() {
        let mut arena = Arena::new();
        let mut state = State::new(&mut arena, 0x7fff_ffff_0000);
        state.regs.clear();
        state.flags.clear();
        state.mem.clear();
        state.sym_stores.clear();
        state.xmm.clear();
        state
            .regs
            .insert(Reg::Rax, arena.opaque("missing", Width::W64));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.remap_refs(&HashMap::new());
        }));

        assert!(result.is_err());
    }
}
