use arm32_cpu::{reg, Cpu, ExampleMem, Mode};

#[test]
fn muls_preserves_carry_in_zero_result_repro() {
    let mut memory = ExampleMem::new_with_data(&0xe010_0190u32.to_le_bytes());
    let mut cpu = Cpu::new();
    cpu.reg_set(Mode::User, reg::CPSR, 0x3800_0010);
    cpu.reg_set(Mode::User, 0, 0);
    cpu.reg_set(Mode::User, 1, 1);
    assert!(cpu.step(&mut memory));
    assert_eq!(cpu.reg_get(Mode::User, reg::CPSR), 0x7800_0010);
    assert_eq!(cpu.reg_get(Mode::User, 0), 0);
    assert_eq!(cpu.reg_get(Mode::User, reg::PC), 4);
}

#[test]
fn arm_multiply_updates_only_nz_and_captures_aliased_operands() {
    for &(rm, rs, rn, product, accumulated) in &[
        (0u32, 1u32, 0u32, 0u32, 0u32),
        (3, 7, 11, 21, 32),
        (0x8000_0000, 1, 1, 0x8000_0000, 0x8000_0001),
        (0xffff_ffff, 2, 2, 0xffff_fffe, 0),
        (0x8000_0000, 2, 9, 0, 9),
    ] {
        for rd in 0..4 {
            for a in 0..=1 {
                for s in 0..=1 {
                    for flags in 0..16 {
                        let instruction: u32 =
                            0xe000_0190 | (rd << 16) | (a << 21) | (s << 20) | ((a * 2) << 12);
                        let mut memory = ExampleMem::new_with_data(&instruction.to_le_bytes());
                        let mut cpu = Cpu::new();
                        let cpsr = (flags << 28) | 0x0800_00d0;
                        cpu.reg_set(Mode::User, reg::CPSR, cpsr);
                        let mut expected = [rm, rs, rn, 0xdead_beef];
                        for (r, &value) in expected.iter().enumerate() {
                            cpu.reg_set(Mode::User, r as u8, value);
                        }
                        let result = if a == 0 { product } else { accumulated };
                        expected[rd as usize] = result;
                        assert!(cpu.step(&mut memory));
                        for (r, &value) in expected.iter().enumerate() {
                            assert_eq!(cpu.reg_get(Mode::User, r as u8), value);
                        }
                        let expected_cpsr = if s == 0 {
                            cpsr
                        } else {
                            (cpsr & 0x3fff_ffff)
                                | (result & 0x8000_0000)
                                | (u32::from(result == 0) << 30)
                        };
                        assert_eq!(
                            cpu.reg_get(Mode::User, reg::CPSR),
                            expected_cpsr,
                            "instruction={:08x}",
                            instruction
                        );
                        assert_eq!(cpu.reg_get(Mode::User, reg::PC), 4);
                    }
                }
            }
        }
    }
}

#[test]
fn long_multiply_updates_only_nz_for_signed_unsigned_and_accumulate() {
    for &(u, a, rm, rs, accumulator, result) in &[
        (0u32, 0u32, 0u32, 1u32, 0u64, 0u64),
        (0, 0, 0xffff_ffff, 0xffff_ffff, 0, 0xffff_fffe_0000_0001),
        (1, 0, 0xffff_ffff, 2, 0, 0xffff_ffff_ffff_fffe),
        (1, 0, 0x8000_0000, 0x8000_0000, 0, 0x4000_0000_0000_0000),
        (0, 1, 1, 1, 0xffff_ffff_ffff_ffff, 0),
        (0, 1, 1, 1, 0x7fff_ffff_ffff_ffff, 0x8000_0000_0000_0000),
        (1, 1, 0xffff_ffff, 2, 2, 0),
        (1, 1, 0xffff_ffff, 2, 1, 0xffff_ffff_ffff_ffff),
    ] {
        for s in 0..=1 {
            for flags in 0..16 {
                let instruction = 0xe083_2190 | (u << 22) | (a << 21) | (s << 20);
                let mut memory = ExampleMem::new_with_data(&instruction.to_le_bytes());
                let mut cpu = Cpu::new();
                let cpsr = (flags << 28) | 0x0800_00d0;
                cpu.reg_set(Mode::User, reg::CPSR, cpsr);
                cpu.reg_set(Mode::User, 0, rm);
                cpu.reg_set(Mode::User, 1, rs);
                cpu.reg_set(Mode::User, 2, accumulator as u32);
                cpu.reg_set(Mode::User, 3, (accumulator >> 32) as u32);
                assert!(cpu.step(&mut memory));
                assert_eq!(cpu.reg_get(Mode::User, 2), result as u32);
                assert_eq!(cpu.reg_get(Mode::User, 3), (result >> 32) as u32);
                assert_eq!(cpu.reg_get(Mode::User, 0), rm);
                assert_eq!(cpu.reg_get(Mode::User, 1), rs);
                let expected_cpsr = if s == 0 {
                    cpsr
                } else {
                    (cpsr & 0x3fff_ffff)
                        | ((result >> 32) as u32 & 0x8000_0000)
                        | (u32::from(result == 0) << 30)
                };
                assert_eq!(
                    cpu.reg_get(Mode::User, reg::CPSR),
                    expected_cpsr,
                    "instruction={:08x}",
                    instruction
                );
                assert_eq!(cpu.reg_get(Mode::User, reg::PC), 4);
            }
        }
    }
}

#[test]
fn long_multiply_captures_destination_aliases_before_writeback() {
    for &(instruction, lo, hi) in &[
        (0xe091_0190u32, 21, 0), // umulls r0, r1, r0, r1
        (0xe0b1_0190, 24, 7),    // umlals r0, r1, r0, r1
        (0xe0d1_0190, 21, 0),    // smulls r0, r1, r0, r1
        (0xe0f1_0190, 24, 7),    // smlals r0, r1, r0, r1
    ] {
        let mut memory = ExampleMem::new_with_data(&instruction.to_le_bytes());
        let mut cpu = Cpu::new();
        cpu.reg_set(Mode::User, reg::CPSR, 0xf800_0010);
        cpu.reg_set(Mode::User, 0, 3);
        cpu.reg_set(Mode::User, 1, 7);
        assert!(cpu.step(&mut memory));
        assert_eq!(cpu.reg_get(Mode::User, 0), lo);
        assert_eq!(cpu.reg_get(Mode::User, 1), hi);
        assert_eq!(cpu.reg_get(Mode::User, reg::CPSR), 0x3800_0010);
    }
}

#[test]
fn failed_multiply_conditions_leave_registers_and_flags_unchanged() {
    for instruction in [
        0x0010_0190u32,
        0x0030_2190,
        0x0093_2190,
        0x00b3_2190,
        0x00d3_2190,
        0x00f3_2190,
    ]
    .iter()
    {
        let mut memory = ExampleMem::new_with_data(&instruction.to_le_bytes());
        let mut cpu = Cpu::new();
        cpu.reg_set(Mode::User, reg::CPSR, 0xb800_0010);
        for r in 0..15 {
            cpu.reg_set(Mode::User, r, u32::from(r) + 3);
        }
        let mut expected = cpu;
        expected.reg_set(Mode::User, reg::PC, 4);
        assert!(cpu.step(&mut memory));
        assert_eq!(cpu, expected);
    }
}

#[test]
fn thumb_multiply_preserves_cv_and_other_cpsr_bits() {
    for &(instruction, rm, rs, result) in &[
        (0x4348u16, 0u32, 1u32, 0u32),
        (0x4348, 3, 7, 21),
        (0x4348, 0xffff_ffff, 2, 0xffff_fffe),
        (0x4348, 0x8000_0000, 2, 0),
        (0x4340, 3, 7, 9), // muls r0, r0
    ] {
        for flags in 0..16 {
            let mut memory = ExampleMem::new_with_data(&instruction.to_le_bytes());
            let mut cpu = Cpu::new();
            let cpsr = (flags << 28) | 0x0800_00f0;
            cpu.reg_set(Mode::User, reg::CPSR, cpsr);
            cpu.reg_set(Mode::User, 0, rm);
            cpu.reg_set(Mode::User, 1, rs);
            assert!(cpu.step(&mut memory));
            assert_eq!(cpu.reg_get(Mode::User, 0), result);
            assert_eq!(cpu.reg_get(Mode::User, 1), rs);
            assert_eq!(
                cpu.reg_get(Mode::User, reg::CPSR),
                (cpsr & 0x3fff_ffff) | (result & 0x8000_0000) | (u32::from(result == 0) << 30)
            );
            assert_eq!(cpu.reg_get(Mode::User, reg::PC), 2);
        }
    }
}
