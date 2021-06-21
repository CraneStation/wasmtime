use cranelift_codegen_shared::constants;
use cranelift_entity::{entity_impl, EntityRef, PrimaryMap};

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RegBankIndex(u32);
entity_impl!(RegBankIndex);

pub(crate) struct RegBank {
    pub name: &'static str,
    pub first_unit: u8,
    pub units: u8,
    pub names: Vec<&'static str>,
    pub prefix: &'static str,
    pub pressure_tracking: bool,
    pub pinned_reg: Option<u16>,
    pub toprcs: Vec<RegClassIndex>,
    pub classes: Vec<RegClassIndex>,
}

impl RegBank {
    pub fn new(
        name: &'static str,
        first_unit: u8,
        units: u8,
        names: Vec<&'static str>,
        prefix: &'static str,
        pressure_tracking: bool,
        pinned_reg: Option<u16>,
    ) -> Self {
        RegBank {
            name,
            first_unit,
            units,
            names,
            prefix,
            pressure_tracking,
            pinned_reg,
            toprcs: Vec::new(),
            classes: Vec::new(),
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct RegClassIndex(u32);
entity_impl!(RegClassIndex);

pub(crate) struct RegClass {
    pub name: &'static str,
    pub index: RegClassIndex,
    pub width: u8,
    pub bank: RegBankIndex,
    pub toprc: RegClassIndex,
    pub count: u8,
    pub start: u8,
    pub subclasses: Vec<RegClassIndex>,
}

impl RegClass {
    pub fn new(
        name: &'static str,
        index: RegClassIndex,
        width: u8,
        bank: RegBankIndex,
        toprc: RegClassIndex,
        count: u8,
        start: u8,
    ) -> Self {
        Self {
            name,
            index,
            width,
            bank,
            toprc,
            count,
            start,
            subclasses: Vec::new(),
        }
    }

    /// Compute a bit-mask of subclasses, including self.
    pub fn subclass_mask(&self) -> u64 {
        let mut m = 1 << self.index.index();
        for rc in self.subclasses.iter() {
            m |= 1 << rc.index();
        }
        m
    }

    /// Compute a bit-mask of the register units allocated by this register class.
    pub fn mask(&self, bank_first_unit: u8) -> Vec<u32> {
        let mut u = (self.start + bank_first_unit) as usize;
        let mut out_mask = vec![0, 0, 0];
        for _ in 0..self.count {
            out_mask[u / 32] |= 1 << (u % 32);
            u += self.width as usize;
        }
        out_mask
    }
}

pub(crate) enum RegClassProto {
    TopLevel(RegBankIndex),
}

pub(crate) struct RegClassBuilder {
    pub name: &'static str,
    pub width: u8,
    pub count: u8,
    pub start: u8,
    pub proto: RegClassProto,
}

impl RegClassBuilder {
    pub fn new_toplevel(name: &'static str, bank: RegBankIndex) -> Self {
        Self {
            name,
            width: 1,
            count: 0,
            start: 0,
            proto: RegClassProto::TopLevel(bank),
        }
    }
    pub fn count(mut self, count: u8) -> Self {
        self.count = count;
        self
    }
    pub fn width(mut self, width: u8) -> Self {
        match self.proto {
            RegClassProto::TopLevel(_) => self.width = width,
        }
        self
    }
}

pub(crate) struct RegBankBuilder {
    pub name: &'static str,
    pub units: u8,
    pub names: Vec<&'static str>,
    pub prefix: &'static str,
    pub pressure_tracking: Option<bool>,
    pub pinned_reg: Option<u16>,
}

impl RegBankBuilder {
    pub fn new(name: &'static str, prefix: &'static str) -> Self {
        Self {
            name,
            units: 0,
            names: vec![],
            prefix,
            pressure_tracking: None,
            pinned_reg: None,
        }
    }
    pub fn units(mut self, units: u8) -> Self {
        self.units = units;
        self
    }
    pub fn names(mut self, names: Vec<&'static str>) -> Self {
        self.names = names;
        self
    }
    pub fn track_pressure(mut self, track: bool) -> Self {
        self.pressure_tracking = Some(track);
        self
    }
}

pub(crate) struct IsaRegsBuilder {
    pub banks: PrimaryMap<RegBankIndex, RegBank>,
    pub classes: PrimaryMap<RegClassIndex, RegClass>,
}

impl IsaRegsBuilder {
    pub fn new() -> Self {
        Self {
            banks: PrimaryMap::new(),
            classes: PrimaryMap::new(),
        }
    }

    pub fn add_bank(&mut self, builder: RegBankBuilder) -> RegBankIndex {
        let first_unit = if self.banks.is_empty() {
            0
        } else {
            let last = &self.banks.last().unwrap();
            let first_available_unit = (last.first_unit + last.units) as i8;
            let units = builder.units;
            let align = if units.is_power_of_two() {
                units
            } else {
                units.next_power_of_two()
            } as i8;
            (first_available_unit + align - 1) & -align
        } as u8;

        self.banks.push(RegBank::new(
            builder.name,
            first_unit,
            builder.units,
            builder.names,
            builder.prefix,
            builder
                .pressure_tracking
                .expect("Pressure tracking must be explicitly set"),
            builder.pinned_reg,
        ))
    }

    pub fn add_class(&mut self, builder: RegClassBuilder) -> RegClassIndex {
        let class_index = self.classes.next_key();

        // Finish delayed construction of RegClass.
        let (bank, toprc, start, width) = match builder.proto {
            RegClassProto::TopLevel(bank_index) => {
                self.banks
                    .get_mut(bank_index)
                    .unwrap()
                    .toprcs
                    .push(class_index);
                (bank_index, class_index, builder.start, builder.width)
            }
        };

        let reg_bank_units = self.banks.get(bank).unwrap().units;
        assert!(start < reg_bank_units);

        let count = if builder.count != 0 {
            builder.count
        } else {
            reg_bank_units / width
        };

        let reg_class = RegClass::new(builder.name, class_index, width, bank, toprc, count, start);
        self.classes.push(reg_class);

        let reg_bank = self.banks.get_mut(bank).unwrap();
        reg_bank.classes.push(class_index);

        class_index
    }

    /// Checks that the set of register classes satisfies:
    ///
    /// 1. Closed under intersection: The intersection of any two register
    ///    classes in the set is either empty or identical to a member of the
    ///    set.
    /// 2. There are no identical classes under different names.
    /// 3. Classes are sorted topologically such that all subclasses have a
    ///    higher index that the superclass.
    pub fn build(self) -> IsaRegs {
        for reg_bank in self.banks.values() {
            for i1 in reg_bank.classes.iter() {
                for i2 in reg_bank.classes.iter() {
                    if i1 >= i2 {
                        continue;
                    }

                    let rc1 = self.classes.get(*i1).unwrap();
                    let rc2 = self.classes.get(*i2).unwrap();

                    let rc1_mask = rc1.mask(0);
                    let rc2_mask = rc2.mask(0);

                    assert!(
                        rc1.width != rc2.width || rc1_mask != rc2_mask,
                        "no duplicates"
                    );
                    if rc1.width != rc2.width {
                        continue;
                    }

                    let mut intersect = Vec::new();
                    for (a, b) in rc1_mask.iter().zip(rc2_mask.iter()) {
                        intersect.push(a & b);
                    }
                    if intersect == vec![0; intersect.len()] {
                        continue;
                    }

                    // Classes must be topologically ordered, so the intersection can't be the
                    // superclass.
                    assert!(intersect != rc1_mask);

                    // If the intersection is the second one, then it must be a subclass.
                    if intersect == rc2_mask {
                        assert!(self
                            .classes
                            .get(*i1)
                            .unwrap()
                            .subclasses
                            .iter()
                            .any(|x| *x == *i2));
                    }
                }
            }
        }

        assert!(
            self.classes.len() <= constants::MAX_NUM_REG_CLASSES,
            "Too many register classes"
        );

        let num_toplevel = self
            .classes
            .values()
            .filter(|x| x.toprc == x.index && self.banks.get(x.bank).unwrap().pressure_tracking)
            .count();

        assert!(
            num_toplevel <= constants::MAX_TRACKED_TOP_RCS,
            "Too many top-level register classes"
        );

        IsaRegs::new(self.banks, self.classes)
    }
}

pub(crate) struct IsaRegs {
    pub banks: PrimaryMap<RegBankIndex, RegBank>,
    pub classes: PrimaryMap<RegClassIndex, RegClass>,
}

impl IsaRegs {
    fn new(
        banks: PrimaryMap<RegBankIndex, RegBank>,
        classes: PrimaryMap<RegClassIndex, RegClass>,
    ) -> Self {
        Self { banks, classes }
    }
}
