use arm32_cpu::{reg, Cpu, ExampleMem, Memory, Mode};

#[derive(Debug, PartialEq)]
enum Access {
    Read(u8, u32),
    Write(u8, u32, u32),
}

#[derive(Default)]
struct TracedMemory {
    inner: ExampleMem,
    accesses: Vec<Access>,
    fault: Option<u32>,
    faults: Vec<u32>,
}

impl Memory for TracedMemory {
    fn r8(&mut self, addr: u32) -> u8 {
        self.accesses.push(Access::Read(8, addr));
        self.inner.r8(addr)
    }

    fn r16(&mut self, addr: u32) -> u16 {
        self.accesses.push(Access::Read(16, addr));
        self.inner.r16(addr)
    }

    fn r32(&mut self, addr: u32) -> u32 {
        self.accesses.push(Access::Read(32, addr));
        if self.fault == Some(addr) {
            self.faults.push(addr);
            return 0;
        }
        self.inner.r32(addr)
    }

    fn w8(&mut self, addr: u32, val: u8) {
        self.accesses.push(Access::Write(8, addr, u32::from(val)));
        self.inner.w8(addr, val);
    }

    fn w16(&mut self, addr: u32, val: u16) {
        self.accesses.push(Access::Write(16, addr, u32::from(val)));
        self.inner.w16(addr, val);
    }

    fn w32(&mut self, addr: u32, val: u32) {
        self.accesses.push(Access::Write(32, addr, val));
        if self.fault == Some(addr) {
            self.faults.push(addr);
            return;
        }
        self.inner.w32(addr, val);
    }
}

#[test]
fn arm_bx_blx_register_select_state_and_capture_lr_target() {
    for &link in &[false, true] {
        for &rm in &[0, reg::LR] {
            for &target in &[0x200, 0x201, 0x203] {
                for &pc in &[0x40u32, 0xffff_fffc] {
                    let instruction = 0xe12f_ff10 | (u32::from(link) << 5) | u32::from(rm);
                    let mut memory = TracedMemory::default();
                    memory.inner.w32(pc, instruction);
                    let mut cpu = Cpu::new();
                    cpu.reg_set(Mode::User, reg::CPSR, 0xf800_00d0);
                    cpu.reg_set(Mode::User, reg::PC, pc);
                    cpu.reg_set(Mode::User, reg::LR, 0xdead_beef);
                    cpu.reg_set(Mode::User, rm, target);
                    let mut expected = cpu;
                    expected.reg_set(Mode::User, reg::PC, target & !1);
                    expected.reg_set(Mode::User, reg::CPSR, 0xf800_00d0 | ((target & 1) << 5));
                    if link {
                        expected.reg_set(Mode::User, reg::LR, pc.wrapping_add(4));
                    }
                    assert!(cpu.step(&mut memory));
                    assert_eq!(cpu, expected, "instruction={:08x}", instruction);
                    assert_eq!(memory.accesses, [Access::Read(32, pc)]);
                }
            }
        }
    }
}

#[test]
fn arm_bx_pc_uses_pipeline_address() {
    let mut memory = ExampleMem::new_with_data(&0xe12f_ff1fu32.to_le_bytes());
    let mut cpu = Cpu::new();
    cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0010);
    assert!(cpu.step(&mut memory));
    assert_eq!(cpu.reg_get(Mode::User, reg::PC), 8);
    assert_eq!(cpu.reg_get(Mode::User, reg::CPSR), 0x3800_0010);
}

#[test]
fn arm_ldr_pc_interworks_with_offset_pre_and_post_indexing() {
    for &(instruction, address, base_after) in &[
        (0xe590_f008u32, 0x108, 0x100), // ldr pc, [r0, #8]
        (0xe530_f008, 0xf8, 0xf8),      // ldr pc, [r0, #-8]!
        (0xe490_f008, 0x100, 0x108),    // ldr pc, [r0], #8
        (0xe690_f101, 0x100, 0x108),    // ldr pc, [r0], r1, lsl #2
        (0xe790_f101, 0x108, 0x100),    // ldr pc, [r0, r1, lsl #2]
        (0xe59f_f0f8, 0x100, 0x100),    // ldr pc, [pc, #0xf8]
    ] {
        for &target in &[0x200, 0x201, 0x203] {
            let mut memory = TracedMemory::default();
            memory.inner.w32(0, instruction);
            memory.inner.w32(address, target);
            let mut cpu = Cpu::new();
            cpu.reg_set(Mode::User, reg::CPSR, 0xb800_00d0);
            cpu.reg_set(Mode::User, 0, 0x100);
            cpu.reg_set(Mode::User, 1, 2);
            let mut expected = cpu;
            expected.reg_set(Mode::User, 0, base_after);
            expected.reg_set(Mode::User, reg::PC, target & !1);
            expected.reg_set(Mode::User, reg::CPSR, 0xb800_00d0 | ((target & 1) << 5));
            assert!(cpu.step(&mut memory));
            assert_eq!(cpu, expected, "instruction={:08x}", instruction);
            assert_eq!(
                memory.accesses,
                [Access::Read(32, 0), Access::Read(32, address)]
            );
        }
    }
}

#[test]
fn arm_ldm_pc_interworks_in_all_addressing_modes() {
    for &(mode, start, end) in &[
        (0x0080_0000u32, 0x100, 0x10c),
        (0x0180_0000, 0x104, 0x10c),
        (0, 0xf8, 0xf4),
        (0x0100_0000, 0xf4, 0xf4),
    ] {
        for w in 0..=1 {
            for &target in &[0x200, 0x201, 0x203] {
                let instruction = 0xe810_800a | mode | (w << 21); // ldm r0{!}, {r1, r3, pc}
                let mut memory = TracedMemory::default();
                memory.inner.w32(0, instruction);
                memory.inner.w32(start, 0x1234_5678);
                memory.inner.w32(start + 4, 0x9876_5432);
                memory.inner.w32(start + 8, target);
                let mut cpu = Cpu::new();
                cpu.reg_set(Mode::User, reg::CPSR, 0x3800_00d0);
                cpu.reg_set(Mode::User, 0, 0x100);
                let mut expected = cpu;
                expected.reg_set(Mode::User, 0, if w == 0 { 0x100 } else { end });
                expected.reg_set(Mode::User, 1, 0x1234_5678);
                expected.reg_set(Mode::User, 3, 0x9876_5432);
                expected.reg_set(Mode::User, reg::PC, target & !1);
                expected.reg_set(Mode::User, reg::CPSR, 0x3800_00d0 | ((target & 1) << 5));
                assert!(cpu.step(&mut memory));
                assert_eq!(cpu, expected, "instruction={:08x}", instruction);
                assert_eq!(
                    memory.accesses,
                    [
                        Access::Read(32, 0),
                        Access::Read(32, start),
                        Access::Read(32, start + 4),
                        Access::Read(32, start + 8)
                    ]
                );
            }
        }
    }
}

#[test]
fn arm_ldm_captures_base_when_loading_it_without_writeback() {
    let mut memory = TracedMemory::default();
    memory.inner.w32(0, 0xe890_8001); // ldmia r0, {r0, pc}
    memory.inner.w32(0x100, 0xdead_beef);
    memory.inner.w32(0x104, 0x203);
    let mut cpu = Cpu::new();
    cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0010);
    cpu.reg_set(Mode::User, 0, 0x100);
    assert!(cpu.step(&mut memory));
    assert_eq!(cpu.reg_get(Mode::User, 0), 0xdead_beef);
    assert_eq!(cpu.reg_get(Mode::User, reg::PC), 0x202);
    assert_eq!(cpu.reg_get(Mode::User, reg::CPSR), 0x3800_0030);
    assert_eq!(
        memory.accesses,
        [
            Access::Read(32, 0),
            Access::Read(32, 0x100),
            Access::Read(32, 0x104)
        ]
    );
}

#[test]
fn failed_branch_and_pc_load_conditions_have_no_side_effects() {
    for &instruction in &[
        0x012f_ff1eu32,
        0x012f_ff3e,
        0x0530_f008,
        0x0690_f101,
        0x08b0_800a,
    ] {
        let mut memory = TracedMemory::default();
        memory.inner.w32(0, instruction);
        let mut cpu = Cpu::new();
        cpu.reg_set(Mode::User, reg::CPSR, 0xb800_0010);
        for r in 0..15 {
            cpu.reg_set(Mode::User, r, 0xffff_ff00 + u32::from(r));
        }
        let mut expected = cpu;
        expected.reg_set(Mode::User, reg::PC, 4);
        assert!(cpu.step(&mut memory));
        assert_eq!(cpu, expected);
        assert_eq!(memory.accesses, [Access::Read(32, 0)]);
    }
}

#[test]
fn thumb_bl_and_blx_pairs_still_retire_in_one_step() {
    for &(suffix, target, cpsr) in &[(0xf801u16, 6, 0x3800_0030), (0xe802, 8, 0x3800_0010)] {
        let mut memory = TracedMemory::default();
        memory.inner.w16(0, 0xf000);
        memory.inner.w16(2, suffix);
        let mut cpu = Cpu::new();
        cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0030);
        let mut expected = cpu;
        expected.reg_set(Mode::User, reg::PC, target);
        expected.reg_set(Mode::User, reg::LR, 5);
        expected.reg_set(Mode::User, reg::CPSR, cpsr);
        assert!(cpu.step(&mut memory));
        assert_eq!(cpu, expected);
        assert_eq!(memory.accesses, [Access::Read(16, 0), Access::Read(16, 2)]);
    }
}

#[test]
fn arm_doubleword_transfers_use_all_mode_three_addresses() {
    for &(addressing, immediate, address_up, address_down, writeback) in &[
        (0x0140_0000u32, true, 0x218, 0x1e8, false),
        (0x0100_0000, false, 0x218, 0x1e8, false),
        (0x0160_0000, true, 0x218, 0x1e8, true),
        (0x0120_0000, false, 0x218, 0x1e8, true),
        (0x0040_0000, true, 0x200, 0x200, true),
        (0, false, 0x200, 0x200, true),
    ] {
        for &up in &[false, true] {
            for &store in &[false, true] {
                for &condition_passed in &[false, true] {
                    let instruction = 0x0004_20d0
                        | addressing
                        | (u32::from(up) << 23)
                        | (u32::from(store) << 5)
                        | if immediate { 0x108 } else { 6 };
                    let address = if up { address_up } else { address_down };
                    let mut memory = TracedMemory::default();
                    memory.inner.w32(0, instruction); // ldreqd/streqd r2, [r4, +/-0x18 or r6]
                    memory.inner.w32(address, 0x1234_5678);
                    memory.inner.w32(address + 4, 0x9abc_def0);
                    let mut cpu = Cpu::new();
                    cpu.reg_set(
                        Mode::User,
                        reg::CPSR,
                        0xb800_00d0 | (u32::from(condition_passed) << 30),
                    );
                    cpu.reg_set(Mode::User, 2, 0x1122_3344);
                    cpu.reg_set(Mode::User, 3, 0x5566_7788);
                    cpu.reg_set(Mode::User, 4, 0x200);
                    cpu.reg_set(Mode::User, 6, 0x18);
                    let mut expected = cpu;
                    expected.reg_set(Mode::User, reg::PC, 4);
                    let mut accesses = vec![Access::Read(32, 0)];
                    if condition_passed {
                        if store {
                            accesses.push(Access::Write(32, address, 0x1122_3344));
                            accesses.push(Access::Write(32, address + 4, 0x5566_7788));
                        } else {
                            accesses.push(Access::Read(32, address));
                            accesses.push(Access::Read(32, address + 4));
                            expected.reg_set(Mode::User, 2, 0x1234_5678);
                            expected.reg_set(Mode::User, 3, 0x9abc_def0);
                        }
                        if writeback {
                            expected.reg_set(Mode::User, 4, if up { 0x218 } else { 0x1e8 });
                        }
                    }
                    assert!(cpu.step(&mut memory), "instruction={:08x}", instruction);
                    assert_eq!(cpu, expected, "instruction={:08x}", instruction);
                    assert_eq!(memory.accesses, accesses);
                    assert_eq!(
                        memory.inner.r32(address),
                        if store && condition_passed {
                            0x1122_3344
                        } else {
                            0x1234_5678
                        }
                    );
                    assert_eq!(
                        memory.inner.r32(address + 4),
                        if store && condition_passed {
                            0x5566_7788
                        } else {
                            0x9abc_def0
                        }
                    );
                }
            }
        }
    }
}

#[test]
fn arm_ldrd_captures_base_in_destination_pair_without_writeback() {
    for &base_register in &[2u8, 3] {
        let mut memory = TracedMemory::default();
        memory
            .inner
            .w32(0, 0xe1c0_20d0 | (u32::from(base_register) << 16));
        memory.inner.w32(0x100, 0x1122_3344);
        memory.inner.w32(0x104, 0x5566_7788);
        let mut cpu = Cpu::new();
        cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0010);
        cpu.reg_set(Mode::User, base_register, 0x100);
        let mut expected = cpu;
        expected.reg_set(Mode::User, 2, 0x1122_3344);
        expected.reg_set(Mode::User, 3, 0x5566_7788);
        expected.reg_set(Mode::User, reg::PC, 4);
        assert!(cpu.step(&mut memory));
        assert_eq!(cpu, expected);
        assert_eq!(
            memory.accesses,
            [
                Access::Read(32, 0),
                Access::Read(32, 0x100),
                Access::Read(32, 0x104)
            ]
        );
    }
}

#[test]
fn arm_strd_captures_source_pair_aliased_with_base_or_offset() {
    for &(instruction, r2, r3, base_after, address) in &[
        (0xe1c2_20f0u32, 0x100, 0x5566_7788, 0x200, 0x100), // strd r2, [r2]
        (0xe1c3_20f0, 0x1122_3344, 0x100, 0x200, 0x100),    // strd r2, [r3]
        (0xe1a4_20f2, 0x18, 0x5566_7788, 0x218, 0x218),     // strd r2, [r4, r2]!
        (0xe084_20f3, 0x1122_3344, 0x18, 0x218, 0x200),     // strd r2, [r4], r3
    ] {
        let mut memory = TracedMemory::default();
        memory.inner.w32(0, instruction);
        let mut cpu = Cpu::new();
        cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0010);
        cpu.reg_set(Mode::User, 2, r2);
        cpu.reg_set(Mode::User, 3, r3);
        cpu.reg_set(Mode::User, 4, 0x200);
        let mut expected = cpu;
        expected.reg_set(Mode::User, 4, base_after);
        expected.reg_set(Mode::User, reg::PC, 4);
        assert!(cpu.step(&mut memory));
        assert_eq!(cpu, expected);
        assert_eq!(
            memory.accesses,
            [
                Access::Read(32, 0),
                Access::Write(32, address, r2),
                Access::Write(32, address + 4, r3)
            ]
        );
        assert_eq!(memory.inner.r32(address), r2);
        assert_eq!(memory.inner.r32(address + 4), r3);
    }
}

#[test]
fn arm_doubleword_transfers_support_pc_relative_and_wrapping_addresses() {
    for &(instruction, base, address, base_after) in &[
        (0xe1cf_2fd8u32, 0x200, 0x100, 0x200), // ldrd r2, [pc, #0xf8]
        (0xe18f_20d6, 0x200, 0x100, 0x200),    // ldrd r2, [pc, r6]
        (0xe1e4_20d8, 0xffff_fff8, 0, 0),      // ldrd r2, [r4, #8]!
        (0xe144_20d8, 0, 0xffff_fff8, 0),      // ldrd r2, [r4, #-8]
        (0xe0c4_20d8, 0xffff_fff8, 0xffff_fff8, 0), // ldrd r2, [r4], #8
    ] {
        for &store in &[false, true] {
            let mut memory = TracedMemory::default();
            // Wraparound data at zero is also the already-fetched instruction.
            memory.inner.w32(address, 0x1122_3344);
            memory.inner.w32(address + 4, 0x5566_7788);
            memory.inner.w32(0, instruction | (u32::from(store) << 5));
            let first_word = memory.inner.r32(address);
            let mut cpu = Cpu::new();
            cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0010);
            cpu.reg_set(Mode::User, 2, 0x9abc_def0);
            cpu.reg_set(Mode::User, 3, 0x1234_5678);
            cpu.reg_set(Mode::User, 4, base);
            cpu.reg_set(Mode::User, 6, 0xf8);
            let mut expected = cpu;
            expected.reg_set(Mode::User, 4, base_after);
            expected.reg_set(Mode::User, reg::PC, 4);
            let accesses = if store {
                vec![
                    Access::Read(32, 0),
                    Access::Write(32, address, 0x9abc_def0),
                    Access::Write(32, address + 4, 0x1234_5678),
                ]
            } else {
                expected.reg_set(Mode::User, 2, first_word);
                expected.reg_set(Mode::User, 3, 0x5566_7788);
                vec![
                    Access::Read(32, 0),
                    Access::Read(32, address),
                    Access::Read(32, address + 4),
                ]
            };
            assert!(cpu.step(&mut memory));
            assert_eq!(cpu, expected, "instruction={:08x}", instruction);
            assert_eq!(memory.accesses, accesses);
        }
    }
}

#[test]
fn arm_doubleword_memory_faults_keep_the_infallible_callback_contract() {
    for &fault in &[0x100, 0x104] {
        for &store in &[false, true] {
            let mut memory = TracedMemory {
                fault: Some(fault),
                ..Default::default()
            };
            memory.inner.w32(0, 0xe0c4_20d8 | (u32::from(store) << 5)); // ldrd/strd r2, [r4], #8
            memory.inner.w32(0x100, 0x1122_3344);
            memory.inner.w32(0x104, 0x5566_7788);
            let mut cpu = Cpu::new();
            cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0010);
            cpu.reg_set(Mode::User, 2, 0x9abc_def0);
            cpu.reg_set(Mode::User, 3, 0x1234_5678);
            cpu.reg_set(Mode::User, 4, 0x100);
            let mut expected = cpu;
            expected.reg_set(Mode::User, 4, 0x108);
            expected.reg_set(Mode::User, reg::PC, 4);
            let accesses = if store {
                vec![
                    Access::Read(32, 0),
                    Access::Write(32, 0x100, 0x9abc_def0),
                    Access::Write(32, 0x104, 0x1234_5678),
                ]
            } else {
                expected.reg_set(Mode::User, 2, if fault == 0x100 { 0 } else { 0x1122_3344 });
                expected.reg_set(Mode::User, 3, if fault == 0x104 { 0 } else { 0x5566_7788 });
                vec![
                    Access::Read(32, 0),
                    Access::Read(32, 0x100),
                    Access::Read(32, 0x104),
                ]
            };
            assert!(cpu.step(&mut memory));
            assert_eq!(cpu, expected);
            assert_eq!(memory.accesses, accesses);
            assert_eq!(memory.faults, [fault]);
            assert_eq!(
                memory.inner.r32(0x100),
                if store && fault != 0x100 {
                    0x9abc_def0
                } else {
                    0x1122_3344
                }
            );
            assert_eq!(
                memory.inner.r32(0x104),
                if store && fault != 0x104 {
                    0x1234_5678
                } else {
                    0x5566_7788
                }
            );
        }
    }
}

#[test]
fn arm_invalid_doubleword_forms_stop_before_data_access() {
    for &(instruction, base) in &[
        (0xe1c4_30d0u32, 0x100), // odd Rd
        (0xe1c4_e0d0, 0x100),    // pair includes PC
        (0xe1c4_20d0, 0x104),    // not doubleword aligned
        (0xe1c4_20d0, 0x101),
        (0xe0e4_20d0, 0x100), // post-indexed with W set
        (0xe1e2_20d0, 0x100), // writeback to first destination
        (0xe1e3_20d0, 0x100), // writeback to second destination
        (0xe1ef_20d0, 0x100), // writeback to PC
        (0xe184_20df, 0x100), // PC offset register
        (0xe1a4_20d4, 0x100), // writeback with Rn == Rm
    ] {
        for &store in &[false, true] {
            let mut memory = TracedMemory::default();
            memory.inner.w32(0, instruction | (u32::from(store) << 5));
            let mut cpu = Cpu::new();
            cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0010);
            cpu.reg_set(Mode::User, 2, base);
            cpu.reg_set(Mode::User, 3, base);
            cpu.reg_set(Mode::User, 4, base);
            assert!(!cpu.step(&mut memory), "instruction={:08x}", instruction);
            assert_eq!(memory.accesses, [Access::Read(32, 0)]);
            assert_eq!(cpu.reg_get(Mode::User, 2), base);
            assert_eq!(cpu.reg_get(Mode::User, 3), base);
            assert_eq!(cpu.reg_get(Mode::User, 4), base);
        }
    }
    for &rm in &[2, 3] {
        let mut memory = TracedMemory::default();
        memory.inner.w32(0, 0xe184_20d0 | rm); // ldrd r2, [r4, r2/r3]
        let mut cpu = Cpu::new();
        cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0010);
        cpu.reg_set(Mode::User, 4, 0x100);
        assert!(!cpu.step(&mut memory));
        assert_eq!(memory.accesses, [Access::Read(32, 0)]);
    }
}

#[test]
fn arm_pld_is_an_unconditional_hint_without_data_access() {
    for &instruction in &[
        0xf5d0_f013,
        0xf550_ffff,
        0xf5df_f004,
        0xf7d0_f101,
        0xf750_f021,
        0xf7d0_f041,
        0xf750_f061,
    ] {
        for flags in 0..16 {
            let mut memory = TracedMemory {
                fault: Some(0x100),
                ..Default::default()
            };
            memory.inner.w32(0, instruction);
            let mut cpu = Cpu::new();
            cpu.reg_set(Mode::User, reg::CPSR, (flags << 28) | 0x0800_00d0);
            cpu.reg_set(Mode::User, 0, 0xffff_fff8);
            cpu.reg_set(Mode::User, 1, 0xffff_ffff);
            let mut expected = cpu;
            expected.reg_set(Mode::User, reg::PC, 4);
            assert!(cpu.step(&mut memory));
            assert_eq!(
                memory.accesses,
                [Access::Read(32, 0)],
                "instruction={:08x}",
                instruction
            );
            assert_eq!(cpu, expected);
            assert!(memory.faults.is_empty());
        }
    }
}

#[test]
fn arm_blx_can_call_thumb_and_return_to_arm() {
    let mut memory = TracedMemory::default();
    memory.inner.w32(0, 0xe12f_ff3e); // blx lr
    memory.inner.w32(4, 0xe3a0_002a); // mov r0, #42
    memory.inner.w16(0x100, 0x4770); // bx lr
    let mut cpu = Cpu::new();
    cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0010);
    cpu.reg_set(Mode::User, reg::LR, 0x101);
    assert!(cpu.step(&mut memory));
    assert!(cpu.thumb_mode());
    assert_eq!(cpu.reg_get(Mode::User, reg::LR), 4);
    assert!(cpu.step(&mut memory));
    assert!(!cpu.thumb_mode());
    assert_eq!(cpu.reg_get(Mode::User, reg::PC), 4);
    assert!(cpu.step(&mut memory));
    assert_eq!(cpu.reg_get(Mode::User, 0), 42);
    assert_eq!(cpu.reg_get(Mode::User, reg::PC), 8);
    assert_eq!(cpu.reg_get(Mode::User, reg::CPSR), 0x3800_0010);
    assert_eq!(
        memory.accesses,
        [
            Access::Read(32, 0),
            Access::Read(16, 0x100),
            Access::Read(32, 4)
        ]
    );
}

#[test]
fn arm_blx_pc_is_rejected_only_when_condition_passes() {
    for &condition_passed in &[false, true] {
        let mut memory = TracedMemory::default();
        memory.inner.w32(0, 0x012f_ff3f); // blxeq pc
        let mut cpu = Cpu::new();
        cpu.reg_set(
            Mode::User,
            reg::CPSR,
            0x3800_0010 | (u32::from(condition_passed) << 30),
        );
        cpu.reg_set(Mode::User, reg::LR, 0x101);
        assert_eq!(cpu.step(&mut memory), !condition_passed);
        assert_eq!(cpu.reg_get(Mode::User, reg::LR), 0x101);
        assert_eq!(memory.accesses, [Access::Read(32, 0)]);
    }
}

#[test]
fn arm_pc_load_fault_callbacks_finish_without_an_abort_engine() {
    for &instruction in &[0xe490_f008, 0xe8b0_8002] {
        for &fault in &[0x100, 0x104] {
            let mut memory = TracedMemory {
                fault: Some(fault),
                ..Default::default()
            };
            memory.inner.w32(0, instruction); // ldr pc, [r0], #8 / ldmia r0!, {r1, pc}
            memory.inner.w32(0x100, 0x201);
            memory.inner.w32(0x104, 0x203);
            let mut cpu = Cpu::new();
            cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0010);
            cpu.reg_set(Mode::User, 0, 0x100);
            let mut expected = cpu;
            expected.reg_set(Mode::User, 0, 0x108);
            let mut accesses = vec![Access::Read(32, 0), Access::Read(32, 0x100)];
            let target = if instruction == 0xe8b0_8002 {
                accesses.push(Access::Read(32, 0x104));
                expected.reg_set(Mode::User, 1, if fault == 0x100 { 0 } else { 0x201 });
                if fault == 0x104 {
                    0
                } else {
                    0x203
                }
            } else if fault == 0x100 {
                0
            } else {
                0x201
            };
            expected.reg_set(Mode::User, reg::PC, target & !1);
            expected.reg_set(Mode::User, reg::CPSR, 0x3800_0010 | ((target & 1) << 5));
            assert!(cpu.step(&mut memory));
            assert_eq!(cpu, expected);
            assert_eq!(memory.accesses, accesses);
            let expected_faults = if instruction == 0xe8b0_8002 || fault == 0x100 {
                vec![fault]
            } else {
                vec![]
            };
            assert_eq!(memory.faults, expected_faults);
        }
    }
}

#[test]
fn arm_byte_and_halfword_encodings_remain_distinct() {
    for &(instruction, width, result) in &[
        (0xe5d0_2000u32, 8, 0x80),      // ldrb r2, [r0]
        (0xe7d0_2101, 8, 0x80),         // ldrb r2, [r0, r1, lsl #2]
        (0xe1d0_20b0, 16, 0xff80),      // ldrh r2, [r0]
        (0xe1d0_20d0, 8, 0xffff_ff80),  // ldrsb r2, [r0]
        (0xe1d0_20f0, 16, 0xffff_ff80), // ldrsh r2, [r0]
    ] {
        let mut memory = TracedMemory::default();
        memory.inner.w32(0, instruction);
        memory.inner.w32(0x100, 0x1234_ff80);
        let mut cpu = Cpu::new();
        cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0010);
        cpu.reg_set(Mode::User, 0, 0x100);
        let mut expected = cpu;
        expected.reg_set(Mode::User, 2, result);
        expected.reg_set(Mode::User, reg::PC, 4);
        assert!(cpu.step(&mut memory));
        assert_eq!(cpu, expected);
        assert_eq!(
            memory.accesses,
            [Access::Read(32, 0), Access::Read(width, 0x100)]
        );
    }
    let mut memory = TracedMemory::default();
    memory.inner.w32(0, 0xe1e0_20b2); // strh r2, [r0, #2]!
    let mut cpu = Cpu::new();
    cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0010);
    cpu.reg_set(Mode::User, 0, 0x100);
    cpu.reg_set(Mode::User, 2, 0x1234_ff80);
    assert!(cpu.step(&mut memory));
    assert_eq!(cpu.reg_get(Mode::User, 0), 0x102);
    assert_eq!(cpu.reg_get(Mode::User, reg::CPSR), 0x3800_0010);
    assert_eq!(
        memory.accesses,
        [Access::Read(32, 0), Access::Write(16, 0x102, 0xff80)]
    );
    assert_eq!(memory.inner.r16(0x102), 0xff80);
}
