use anyhow::Result;
use cfg_if::cfg_if;
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::{binemit, ir};
use std::fmt::Write;

pub struct PrintRelocs {
    pub flag_print: bool,
    pub text: String,
}

impl PrintRelocs {
    pub fn new(flag_print: bool) -> Self {
        Self {
            flag_print,
            text: String::new(),
        }
    }
}

impl binemit::RelocSink for PrintRelocs {
    fn reloc_external(
        &mut self,
        where_: binemit::CodeOffset,
        _srcloc: ir::SourceLoc,
        r: binemit::Reloc,
        name: &ir::ExternalName,
        addend: binemit::Addend,
    ) {
        if self.flag_print {
            writeln!(
                &mut self.text,
                "reloc_external: {} {} {} at {}",
                r, name, addend, where_
            )
            .unwrap();
        }
    }

    fn reloc_jt(&mut self, where_: binemit::CodeOffset, r: binemit::Reloc, jt: ir::JumpTable) {
        if self.flag_print {
            writeln!(&mut self.text, "reloc_jt: {} {} at {}", r, jt, where_).unwrap();
        }
    }

    fn reloc_constant(
        &mut self,
        code_offset: binemit::CodeOffset,
        reloc: binemit::Reloc,
        constant: ir::ConstantOffset,
    ) {
        if self.flag_print {
            writeln!(
                &mut self.text,
                "reloc_constant: {} {} at {}",
                reloc, constant, code_offset
            )
            .unwrap();
        }
    }
}

pub struct PrintTraps {
    pub flag_print: bool,
    pub text: String,
}

impl PrintTraps {
    pub fn new(flag_print: bool) -> Self {
        Self {
            flag_print,
            text: String::new(),
        }
    }
}

impl binemit::TrapSink for PrintTraps {
    fn trap(&mut self, offset: binemit::CodeOffset, _srcloc: ir::SourceLoc, code: ir::TrapCode) {
        if self.flag_print {
            writeln!(&mut self.text, "trap: {} at {}", code, offset).unwrap();
        }
    }
}

pub struct PrintStackMaps {
    pub flag_print: bool,
    pub text: String,
}

impl PrintStackMaps {
    pub fn new(flag_print: bool) -> Self {
        Self {
            flag_print,
            text: String::new(),
        }
    }
}

impl binemit::StackMapSink for PrintStackMaps {
    fn add_stack_map(&mut self, offset: binemit::CodeOffset, _: binemit::StackMap) {
        if self.flag_print {
            writeln!(&mut self.text, "add_stack_map at {}", offset).unwrap();
        }
    }
}

cfg_if! {
    if #[cfg(feature = "disas")] {
        use capstone::prelude::*;
        use target_lexicon::Architecture;

        fn get_disassembler(isa: &dyn TargetIsa) -> Result<Capstone> {
            let cs = match isa.triple().architecture {
                Architecture::X86_32(_) => Capstone::new()
                    .x86()
                    .mode(arch::x86::ArchMode::Mode32)
                    .build()
                    .map_err(map_caperr)?,
                Architecture::X86_64 => Capstone::new()
                    .x86()
                    .mode(arch::x86::ArchMode::Mode64)
                    .build()
                    .map_err(map_caperr)?,
                Architecture::Arm(arm) => {
                    if arm.is_thumb() {
                        Capstone::new()
                            .arm()
                            .mode(arch::arm::ArchMode::Thumb)
                            .build()
                            .map_err(map_caperr)?
                    } else {
                        Capstone::new()
                            .arm()
                            .mode(arch::arm::ArchMode::Arm)
                            .build()
                            .map_err(map_caperr)?
                    }
                }
                Architecture::Aarch64 {..} => {
                    let mut cs = Capstone::new()
                        .arm64()
                        .mode(arch::arm64::ArchMode::Arm)
                        .build()
                        .map_err(map_caperr)?;
                    // AArch64 uses inline constants rather than a separate constant pool right now.
                    // Without this option, Capstone will stop disassembling as soon as it sees
                    // an inline constant that is not also a valid instruction. With this option,
                    // Capstone will print a `.byte` directive with the bytes of the inline constant
                    // and continue to the next instruction.
                    cs.set_skipdata(true).map_err(map_caperr)?;
                    cs
                }
                Architecture::S390x {..} => Capstone::new()
                    .sysz()
                    .mode(arch::sysz::ArchMode::Default)
                    .build()
                    .map_err(map_caperr)?,
                _ => anyhow::bail!("Unknown ISA"),
            };

            Ok(cs)
        }

        pub fn print_disassembly(isa: &dyn TargetIsa, mem: &[u8]) -> Result<()> {
            let cs = get_disassembler(isa)?;

            println!("\nDisassembly of {} bytes:", mem.len());
            let insns = cs.disasm_all(&mem, 0x0).unwrap();
            for i in insns.iter() {
                let mut line = String::new();

                write!(&mut line, "{:4x}:\t", i.address()).unwrap();

                let mut bytes_str = String::new();
                let mut len = 0;
                let mut first = true;
                for b in i.bytes() {
                    if !first {
                        write!(&mut bytes_str, " ").unwrap();
                    }
                    write!(&mut bytes_str, "{:02x}", b).unwrap();
                    len += 1;
                    first = false;
                }
                write!(&mut line, "{:21}\t", bytes_str).unwrap();
                if len > 8 {
                    write!(&mut line, "\n\t\t\t\t").unwrap();
                }

                if let Some(s) = i.mnemonic() {
                    write!(&mut line, "{}\t", s).unwrap();
                }

                if let Some(s) = i.op_str() {
                    write!(&mut line, "{}", s).unwrap();
                }

                println!("{}", line);
            }
            Ok(())
        }

        fn map_caperr(err: capstone::Error) -> anyhow::Error{
            anyhow::format_err!("{}", err)
        }
    } else {
        pub fn print_disassembly(_: &dyn TargetIsa, _: &[u8]) -> Result<()> {
            println!("\nNo disassembly available.");
            Ok(())
        }
    }
}

pub fn print_all(
    isa: &dyn TargetIsa,
    mem: &[u8],
    code_size: u32,
    rodata_size: u32,
    relocs: &PrintRelocs,
    traps: &PrintTraps,
    stack_maps: &PrintStackMaps,
) -> Result<()> {
    print_bytes(&mem);
    print_disassembly(isa, &mem[0..code_size as usize])?;
    print_readonly_data(&mem[code_size as usize..(code_size + rodata_size) as usize]);
    println!("\n{}\n{}\n{}", &relocs.text, &traps.text, &stack_maps.text);
    Ok(())
}

pub fn print_bytes(mem: &[u8]) {
    print!(".byte ");
    let mut first = true;
    for byte in mem.iter() {
        if first {
            first = false;
        } else {
            print!(", ");
        }
        print!("{}", byte);
    }
    println!();
}

pub fn print_readonly_data(mem: &[u8]) {
    if mem.is_empty() {
        return;
    }

    println!("\nFollowed by {} bytes of read-only data:", mem.len());

    for (i, byte) in mem.iter().enumerate() {
        if i % 16 == 0 {
            if i != 0 {
                println!();
            }
            print!("{:4}: ", i);
        }
        if i % 4 == 0 {
            print!(" ");
        }
        print!("{:02x} ", byte);
    }
    println!();
}
