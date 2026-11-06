//! Decodes the instructions a guest uses on memory-mapped device registers.
//!
//! When a guest touches device memory the hypervisor reports the address and
//! the instruction bytes, and the monitor must perform the access. Linux
//! reaches device registers with plain `mov` forms, so a small table covers
//! them, and anything else is refused rather than guessed at.

/// One decoded access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    /// True for a store to the device.
    pub write: bool,
    /// Width of the access in bytes.
    pub size: u8,
    /// General register involved, numbered as x86 encodes them.
    pub register: u8,
    /// The value, when the instruction carries it as an immediate.
    pub immediate: Option<u32>,
    /// A load clears the whole destination register.
    pub zero_extend: bool,
    /// Instruction length in bytes.
    pub length: u8,
}

const REX_W: u8 = 0x08;
const REX_R: u8 = 0x04;

/// Skips the addressing bytes, returning the position after them.
fn skip_address(bytes: &[u8], mut at: usize, modrm: u8) -> Option<usize> {
    let mode = modrm >> 6;
    let rm = modrm & 0x07;
    if mode == 3 {
        return None; // A register operand cannot have caused an exit.
    }
    if rm == 4 {
        let sib = *bytes.get(at)?;
        at += 1;
        if sib & 0x07 == 5 && mode == 0 {
            at += 4;
        }
    }
    match (mode, rm) {
        (0, 5) | (2, _) => at += 4,
        (1, _) => at += 1,
        _ => {}
    }
    Some(at)
}

/// Decodes `bytes`, returning `None` for anything not understood.
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<Access> {
    let mut at = 0;
    let mut wide16 = false;
    let mut rex = 0;
    loop {
        match *bytes.get(at)? {
            0x66 => wide16 = true,
            0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 => {}
            byte @ 0x40..=0x4f => rex = byte,
            _ => break,
        }
        at += 1;
    }
    let width = if wide16 { 2 } else { 4 };
    let opcode = *bytes.get(at)?;
    at += 1;
    // (store, size, immediate, zero extend)
    let (write, size, immediate, zero_extend) = match opcode {
        0x88 => (true, 1, false, false),
        0x89 => (true, width, false, false),
        0x8a => (false, 1, false, false),
        0x8b => (false, width, false, width == 4),
        0xc6 => (true, 1, true, false),
        0xc7 => (true, width, true, false),
        0x0f => {
            let second = *bytes.get(at)?;
            at += 1;
            match second {
                0xb6 => (false, 1, false, true),
                0xb7 => (false, 2, false, true),
                _ => return None,
            }
        }
        _ => return None,
    };
    if rex & REX_W != 0 {
        return None; // No device here has 64-bit registers.
    }
    let modrm = *bytes.get(at)?;
    at += 1;
    let field = (modrm >> 3) & 0x07;
    // Without REX, byte registers 4 to 7 mean AH, CH, DH and BH.
    if matches!(opcode, 0x88 | 0x8a) && rex == 0 && field >= 4 {
        return None;
    }
    if immediate && field != 0 {
        return None;
    }
    let register = field | if rex & REX_R != 0 { 8 } else { 0 };
    at = skip_address(bytes, at, modrm)?;
    let value = if immediate {
        let count = usize::from(size);
        let raw = bytes.get(at..at + count)?;
        at += count;
        let mut value = [0_u8; 4];
        value[..count].copy_from_slice(raw);
        Some(u32::from_le_bytes(value))
    } else {
        None
    };
    Some(Access {
        write,
        size,
        register,
        immediate: value,
        zero_extend,
        length: u8::try_from(at).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_32_bit_load() {
        // mov eax, [rbx]
        let access = decode(&[0x8b, 0x03]).unwrap();
        assert_eq!(
            (access.write, access.size, access.register, access.length),
            (false, 4, 0, 2)
        );
        assert!(access.zero_extend);
    }

    #[test]
    fn a_32_bit_store_with_displacement() {
        // mov [rbx+0x10], ecx
        let access = decode(&[0x89, 0x4b, 0x10]).unwrap();
        assert_eq!(
            (access.write, access.size, access.register, access.length),
            (true, 4, 1, 3)
        );
    }

    #[test]
    fn the_upper_32_bit_registers_are_not_mistaken_for_high_bytes() {
        // mov [rsi], edi, as Linux's iowrite32 was seen to use.
        let access = decode(&[0x89, 0x3e, 0x5d, 0xc3]).unwrap();
        assert_eq!((access.size, access.register, access.length), (4, 7, 2));
    }

    #[test]
    fn a_16_bit_store_keeps_the_prefix() {
        // mov [rdi], ax
        let access = decode(&[0x66, 0x89, 0x07]).unwrap();
        assert_eq!((access.size, access.length), (2, 3));
    }

    #[test]
    fn a_16_bit_load_does_not_clear_the_register() {
        // mov ax, [rdi]
        let access = decode(&[0x66, 0x8b, 0x07]).unwrap();
        assert!(!access.zero_extend);
    }

    #[test]
    fn byte_stores_work_with_low_registers() {
        // mov [rdi], al
        let access = decode(&[0x88, 0x07]).unwrap();
        assert_eq!((access.size, access.register), (1, 0));
    }

    #[test]
    fn high_byte_registers_are_refused() {
        // mov [rdi], ah
        assert_eq!(decode(&[0x88, 0x27]), None);
    }

    #[test]
    fn rex_makes_sil_a_low_byte_register() {
        // mov [rdi], sil
        assert_eq!(decode(&[0x40, 0x88, 0x37]).unwrap().register, 6);
    }

    #[test]
    fn rex_r_selects_the_upper_registers() {
        // mov r9d, [rbx]
        assert_eq!(decode(&[0x44, 0x8b, 0x0b]).unwrap().register, 9);
    }

    #[test]
    fn a_sib_byte_and_disp32_are_counted() {
        // mov eax, [rbx+rcx*4+0x12345678]
        let access = decode(&[0x8b, 0x84, 0x8b, 0x78, 0x56, 0x34, 0x12]).unwrap();
        assert_eq!(access.length, 7);
    }

    #[test]
    fn rip_relative_addressing_is_counted() {
        // mov eax, [rip+0x100]
        let access = decode(&[0x8b, 0x05, 0, 1, 0, 0]).unwrap();
        assert_eq!(access.length, 6);
    }

    #[test]
    fn immediates_are_read() {
        // mov dword [rbx], 0x11223344
        let access = decode(&[0xc7, 0x03, 0x44, 0x33, 0x22, 0x11]).unwrap();
        assert_eq!(access.immediate, Some(0x1122_3344));
        assert_eq!(access.length, 6);
        // mov byte [rbx], 7
        assert_eq!(decode(&[0xc6, 0x03, 7]).unwrap().immediate, Some(7));
    }

    #[test]
    fn movzx_clears_the_destination() {
        // movzx eax, byte [rbx]
        let access = decode(&[0x0f, 0xb6, 0x03]).unwrap();
        assert_eq!((access.size, access.zero_extend), (1, true));
        // movzx eax, word [rbx]
        assert_eq!(decode(&[0x0f, 0xb7, 0x03]).unwrap().size, 2);
    }

    #[test]
    fn unknown_instructions_are_refused() {
        assert_eq!(decode(&[0x90]), None);
        assert_eq!(decode(&[0xa5]), None, "string moves");
        assert_eq!(decode(&[0x0f, 0x05]), None);
    }

    #[test]
    fn register_operands_are_refused() {
        // mov eax, ebx touches no memory.
        assert_eq!(decode(&[0x8b, 0xc3]), None);
    }

    #[test]
    fn quadword_accesses_are_refused() {
        assert_eq!(decode(&[0x48, 0x8b, 0x03]), None);
    }

    #[test]
    fn truncated_instructions_are_refused() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[0x8b]), None);
        assert_eq!(decode(&[0xc7, 0x03, 0x44]), None);
        assert_eq!(decode(&[0x8b, 0x84]), None);
    }

    #[test]
    fn a_bad_immediate_group_is_refused() {
        // Group 11 with a non-zero field is not a mov.
        assert_eq!(decode(&[0xc7, 0x0b, 0, 0, 0, 0]), None);
    }
}
