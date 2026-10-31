use arm32_cpu::{reg, Cpu, ExampleMem, Mode};

#[test]
fn logical_immediates_preserve_carry_without_rotation() {
    for opcode in [0u32, 1, 8, 9, 12, 13, 14, 15].iter().copied() {
        for (immediate, rotated_carry) in [(2u32, None), (0x102, Some(1u32)), (0x202, Some(0))]
            .iter()
            .copied()
        {
            for carry in 0..=1 {
                let destination = if opcode == 8 || opcode == 9 { 0 } else { 2 };
                let instruction = 0xe210_0000 | (opcode << 21) | (destination << 12) | immediate;
                let mut memory = ExampleMem::new_with_data(&instruction.to_le_bytes());
                let mut cpu = Cpu::new();
                cpu.reg_set(Mode::User, reg::CPSR, 0x1000_001f | (carry << 29));
                cpu.reg_set(Mode::User, reg::PC, 0);
                cpu.reg_set(Mode::User, 0, 0x1234_5678);
                assert!(cpu.step(&mut memory));
                let flags = cpu.reg_get(Mode::User, reg::CPSR);
                assert_eq!(
                    (flags >> 29) & 1,
                    rotated_carry.unwrap_or(carry),
                    "opcode={:x}, immediate={:x}, carry={}",
                    opcode,
                    immediate,
                    carry
                );
                assert_eq!(flags & (1 << 28), 1 << 28);
                assert_eq!(cpu.reg_get(Mode::User, reg::PC), 4);
            }
        }
    }
}
