use anyhow::{bail, ensure, Error};
use object::elf::*;
use object::endian::LittleEndian;
use std::ffi::CStr;
use std::mem::size_of;
use std::os::raw::c_char;

/// Checks if ELF is supported.
pub fn ensure_supported_elf_format(bytes: &[u8]) -> Result<(), Error> {
    parse_elf_object_mut(bytes)?;
    Ok(())
}

/// Converts ELF into loadable file.
pub fn convert_object_elf_to_loadable_file(bytes: &mut Vec<u8>) {
    // TODO support all platforms, but now don't fix unsupported.
    let mut file = match parse_elf_object_mut(bytes) {
        Ok(file) => file,
        Err(_) => {
            return;
        }
    };

    let shstrtab_off = shstrtab_off(file.as_mut());

    let segment = ElfSectionIterator::new(file.as_mut())
        .find(|s| is_text_section(bytes, shstrtab_off, s.as_ref()))
        .map(|s| {
            let sh_offset = s.sh_offset();
            let sh_size = s.sh_size();
            (sh_offset, sh_size)
        });

    // LLDB wants segment with virtual address set, placing them at the end of ELF.
    let ph_off = bytes.len();
    let e_phentsize = file.target_phentsize();
    let e_phnum = 1;
    bytes.resize(ph_off + e_phentsize * e_phnum, 0);
    let mut file = parse_elf_object_mut(bytes).unwrap();

    if let Some((sh_offset, sh_size)) = segment {
        let mut program = file.program_at(ph_off as u64);
        program.p_type_set(PT_LOAD);
        program.p_offset_set(sh_offset);
        program.p_filesz_set(sh_size);
        program.p_memsz_set(sh_size);
    } else {
        unreachable!();
    }

    // It is somewhat loadable ELF file at this moment.
    file.e_type_set(ET_DYN);
    file.e_ph_set(e_phentsize as u16, ph_off as u64, e_phnum as u16);

    // A linker needs to patch section's `sh_addr` and
    // program's `p_vaddr`, `p_paddr`, and maybe `p_memsz`.
}

/// Patches loadable file fields.
pub fn patch_loadable_file(bytes: &mut [u8], code_region: (*const u8, usize)) -> Result<(), Error> {
    // TODO support all platforms, but now don't fix unsupported.
    let mut file = match parse_elf_object_mut(bytes) {
        Ok(file) => file,
        Err(_) => {
            return Ok(());
        }
    };

    ensure!(
        file.e_phoff() != 0 && file.e_phnum() == 1,
        "program header table must created"
    );

    let shstrtab_off = shstrtab_off(file.as_mut());

    if let Some(mut section) = ElfSectionIterator::new(file.as_mut())
        .find(|s| is_text_section(bytes, shstrtab_off, s.as_ref()))
    {
        // Patch vaddr, and save file location and its size.
        section.sh_addr_set(code_region.0 as u64);
    }

    // LLDB wants segment with virtual address set, placing them at the end of ELF.
    let ph_off = file.e_phoff();
    let (v_offset, size) = code_region;
    let mut program = file.program_at(ph_off);
    program.p_vaddr_set(v_offset as u64);
    program.p_paddr_set(v_offset as u64);
    program.p_memsz_set(size as u64);

    Ok(())
}

/// Removes all patched information from loadable file fields.
/// Inverse of `patch_loadable_file`.
pub fn sanitize_loadable_file(bytes: &mut [u8]) -> Result<(), Error> {
    const NON_EXISTENT_RANGE: (*const u8, usize) = (std::ptr::null(), 0);
    patch_loadable_file(bytes, NON_EXISTENT_RANGE)
}

fn is_text_section(bytes: &mut [u8], shstrtab_off: u64, section: &dyn ElfSection) -> bool {
    if section.sh_type() != SHT_PROGBITS {
        return false;
    }
    // It is a SHT_PROGBITS, but we need to check sh_name to ensure it is our function
    let sh_name_off = section.sh_name();
    let sh_name = unsafe {
        CStr::from_ptr(
            bytes
                .as_ptr()
                .offset((shstrtab_off + sh_name_off as u64) as isize) as *const c_char,
        )
        .to_str()
        .expect("name")
    };
    sh_name == ".text"
}

fn shstrtab_off(file: &mut dyn ElfObject) -> u64 {
    ElfSectionIterator::new(file)
        .rev()
        .find(|s| s.sh_type() == SHT_STRTAB)
        .map_or(0, |s| s.sh_offset())
}

fn parse_elf_object_mut<'data>(obj: &'data [u8]) -> Result<Box<dyn ElfObject>, Error> {
    let ident: &Ident = unsafe { &*(obj.as_ptr() as *const Ident) };
    let elf = match (ident.class, ident.data) {
        (ELFCLASS64, ELFDATA2LSB) => Box::new(ElfObject64LE {
            header: unsafe { &mut *(obj.as_ptr() as *mut FileHeader64<_>) },
        }),
        (c, d) => {
            bail!("Unsupported elf format (class: {}, data: {})", c, d);
        }
    };
    match elf.e_machine() {
        EM_X86_64 | EM_ARM => (),
        machine => {
            bail!("Unsupported ELF target machine: {:x}", machine);
        }
    }
    let e_shentsize = elf.e_shentsize();
    ensure!(e_shentsize as usize == elf.target_shentsize(), "size of sh");
    Ok(elf)
}

trait ElfObject {
    fn e_ident(&self) -> &Ident;
    fn e_machine(&self) -> u16;
    fn e_type(&self) -> u16;
    fn e_type_set(&mut self, ty: u16);
    fn e_shentsize(&self) -> u16;
    fn e_shoff(&self) -> u64;
    fn e_shnum(&self) -> u16;
    fn e_phentsize(&self) -> u16;
    fn e_phoff(&self) -> u64;
    fn e_phnum(&self) -> u16;
    fn e_ph_set(&mut self, entsize: u16, off: u64, num: u16);
    fn target_shentsize(&self) -> usize;
    fn target_phentsize(&self) -> usize;
    fn section_at<'file, 'a>(&'file mut self, off: u64) -> Box<dyn ElfSection + 'a>
    where
        'a: 'file;
    fn program_at<'file, 'a>(&'file mut self, off: u64) -> Box<dyn ElfProgram + 'a>
    where
        'a: 'file;
}

struct ElfObject64LE<'data> {
    header: &'data mut FileHeader64<LittleEndian>,
}

impl<'data> ElfObject for ElfObject64LE<'data> {
    fn e_ident(&self) -> &Ident {
        &self.header.e_ident
    }
    fn e_machine(&self) -> u16 {
        self.header.e_machine.get(LittleEndian)
    }
    fn e_type(&self) -> u16 {
        self.header.e_type.get(LittleEndian)
    }
    fn e_type_set(&mut self, ty: u16) {
        self.header.e_type.set(LittleEndian, ty);
    }
    fn e_shentsize(&self) -> u16 {
        self.header.e_shentsize.get(LittleEndian)
    }
    fn e_shoff(&self) -> u64 {
        self.header.e_shoff.get(LittleEndian)
    }
    fn e_shnum(&self) -> u16 {
        self.header.e_shnum.get(LittleEndian)
    }
    fn e_phentsize(&self) -> u16 {
        self.header.e_phentsize.get(LittleEndian)
    }
    fn e_phoff(&self) -> u64 {
        self.header.e_phoff.get(LittleEndian)
    }
    fn e_phnum(&self) -> u16 {
        self.header.e_phnum.get(LittleEndian)
    }
    fn e_ph_set(&mut self, entsize: u16, off: u64, num: u16) {
        self.header.e_phentsize.set(LittleEndian, entsize);
        self.header.e_phoff.set(LittleEndian, off);
        self.header.e_phnum.set(LittleEndian, num);
    }
    fn target_shentsize(&self) -> usize {
        size_of::<SectionHeader64<LittleEndian>>()
    }
    fn target_phentsize(&self) -> usize {
        size_of::<ProgramHeader64<LittleEndian>>()
    }
    fn section_at<'file, 'a>(&'file mut self, off: u64) -> Box<dyn ElfSection + 'a>
    where
        'a: 'file,
    {
        let header: &mut SectionHeader64<LittleEndian> = unsafe {
            &mut *((self.header as *const _ as *const u8).offset(off as isize)
                as *mut SectionHeader64<_>)
        };
        Box::new(ElfSection64LE { header })
    }
    fn program_at<'file, 'a>(&'file mut self, off: u64) -> Box<dyn ElfProgram + 'a>
    where
        'a: 'file,
    {
        let header: &mut ProgramHeader64<LittleEndian> = unsafe {
            &mut *((self.header as *const _ as *const u8).offset(off as isize)
                as *mut ProgramHeader64<_>)
        };
        Box::new(ElfProgram64LE { header })
    }
}

trait ElfSection {
    fn sh_type(&self) -> u32;
    fn sh_name(&self) -> u32;
    fn sh_offset(&self) -> u64;
    fn sh_size(&self) -> u64;
    fn sh_addr_set(&mut self, addr: u64);
}

struct ElfSection64LE<'data> {
    header: &'data mut SectionHeader64<LittleEndian>,
}

impl<'data> ElfSection for ElfSection64LE<'data> {
    fn sh_type(&self) -> u32 {
        self.header.sh_type.get(LittleEndian)
    }
    fn sh_name(&self) -> u32 {
        self.header.sh_name.get(LittleEndian)
    }
    fn sh_offset(&self) -> u64 {
        self.header.sh_offset.get(LittleEndian)
    }
    fn sh_size(&self) -> u64 {
        self.header.sh_size.get(LittleEndian)
    }
    fn sh_addr_set(&mut self, addr: u64) {
        self.header.sh_addr.set(LittleEndian, addr);
    }
}

struct ElfSectionIterator<'b> {
    e_shentsize: u64,
    e_shoff: u64,
    elf: &'b mut dyn ElfObject,
    start: u64,
    end: u64,
}
impl<'b> ElfSectionIterator<'b> {
    pub fn new(elf: &'b mut (dyn ElfObject + 'b)) -> Self {
        let e_shentsize = elf.e_shentsize() as u64;
        let e_shoff = elf.e_shoff();
        let e_shnum = elf.e_shnum() as u64;
        Self {
            e_shentsize,
            e_shoff,
            elf,
            start: 0,
            end: e_shnum,
        }
    }
}
impl<'b> Iterator for ElfSectionIterator<'b> {
    type Item = Box<dyn ElfSection + 'b>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.start >= self.end {
            return None;
        }
        let off = self.e_shoff + self.start * self.e_shentsize;
        self.start += 1;
        Some(self.elf.section_at(off))
    }
}
impl<'b> DoubleEndedIterator for ElfSectionIterator<'b> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.start >= self.end {
            return None;
        }
        self.end -= 1;
        let off = self.e_shoff + self.end * self.e_shentsize;
        Some(self.elf.section_at(off))
    }
}

trait ElfProgram {
    fn p_vaddr_set(&mut self, off: u64);
    fn p_paddr_set(&mut self, off: u64);
    fn p_memsz_set(&mut self, size: u64);
    fn p_type_set(&mut self, ty: u32);
    fn p_offset_set(&mut self, off: u64);
    fn p_filesz_set(&mut self, size: u64);
}

struct ElfProgram64LE<'data> {
    header: &'data mut ProgramHeader64<LittleEndian>,
}

impl<'data> ElfProgram for ElfProgram64LE<'data> {
    fn p_vaddr_set(&mut self, off: u64) {
        self.header.p_vaddr.set(LittleEndian, off);
    }
    fn p_paddr_set(&mut self, off: u64) {
        self.header.p_paddr.set(LittleEndian, off);
    }
    fn p_memsz_set(&mut self, size: u64) {
        self.header.p_memsz.set(LittleEndian, size);
    }
    fn p_type_set(&mut self, ty: u32) {
        self.header.p_type.set(LittleEndian, ty);
    }
    fn p_offset_set(&mut self, off: u64) {
        self.header.p_offset.set(LittleEndian, off);
    }
    fn p_filesz_set(&mut self, size: u64) {
        self.header.p_filesz.set(LittleEndian, size);
    }
}
