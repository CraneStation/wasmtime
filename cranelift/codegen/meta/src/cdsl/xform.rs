use crate::cdsl::ast::{
    Apply, BlockPool, ConstPool, DefIndex, DefPool, DummyDef, DummyExpr, Expr, PatternPosition,
    VarIndex, VarPool,
};
use crate::cdsl::instructions::Instruction;
use crate::cdsl::type_inference::{infer_transform, TypeEnvironment};
use crate::cdsl::typevar::TypeVar;

use cranelift_entity::{entity_impl, PrimaryMap};

use std::collections::{HashMap, HashSet};
use std::iter::FromIterator;

/// An instruction transformation consists of a source and destination pattern.
///
/// Patterns are expressed in *register transfer language* as tuples of Def or Expr nodes. A
/// pattern may optionally have a sequence of TypeConstraints, that additionally limit the set of
/// cases when it applies.
///
/// The source pattern can contain only a single instruction.
pub(crate) struct Transform {
    pub src: DefIndex,
    pub dst: Vec<DefIndex>,
    pub var_pool: VarPool,
    pub def_pool: DefPool,
    pub block_pool: BlockPool,
    pub const_pool: ConstPool,
    pub type_env: TypeEnvironment,
}

type SymbolTable = HashMap<String, VarIndex>;

impl Transform {
    fn new(src: DummyDef, dst: Vec<DummyDef>) -> Self {
        let mut var_pool = VarPool::new();
        let mut def_pool = DefPool::new();
        let mut block_pool = BlockPool::new();
        let mut const_pool = ConstPool::new();

        let mut input_vars: Vec<VarIndex> = Vec::new();
        let mut defined_vars: Vec<VarIndex> = Vec::new();

        // Maps variable names to our own Var copies.
        let mut symbol_table: SymbolTable = SymbolTable::new();

        // Rewrite variables in src and dst using our own copies.
        let src = rewrite_def_list(
            PatternPosition::Source,
            vec![src],
            &mut symbol_table,
            &mut input_vars,
            &mut defined_vars,
            &mut var_pool,
            &mut def_pool,
            &mut block_pool,
            &mut const_pool,
        )[0];

        let num_src_inputs = input_vars.len();

        let dst = rewrite_def_list(
            PatternPosition::Destination,
            dst,
            &mut symbol_table,
            &mut input_vars,
            &mut defined_vars,
            &mut var_pool,
            &mut def_pool,
            &mut block_pool,
            &mut const_pool,
        );

        // Sanity checks.
        for &var_index in &input_vars {
            assert!(
                var_pool.get(var_index).is_input(),
                "'{:?}' used as both input and def",
                var_pool.get(var_index)
            );
        }
        assert!(
            input_vars.len() == num_src_inputs,
            "extra input vars in dst pattern: {:?}",
            input_vars
                .iter()
                .map(|&i| var_pool.get(i))
                .skip(num_src_inputs)
                .collect::<Vec<_>>()
        );

        // Perform type inference and cleanup.
        let type_env = infer_transform(src, &dst, &def_pool, &mut var_pool).unwrap();

        // Sanity check: the set of inferred free type variables should be a subset of the type
        // variables corresponding to Vars appearing in the source pattern.
        {
            let free_typevars: HashSet<TypeVar> =
                HashSet::from_iter(type_env.free_typevars(&mut var_pool));
            let src_tvs = HashSet::from_iter(
                input_vars
                    .clone()
                    .iter()
                    .chain(
                        defined_vars
                            .iter()
                            .filter(|&&var_index| !var_pool.get(var_index).is_temp()),
                    )
                    .map(|&var_index| var_pool.get(var_index).get_typevar())
                    .filter(|maybe_var| maybe_var.is_some())
                    .map(|var| var.unwrap()),
            );
            if !free_typevars.is_subset(&src_tvs) {
                let missing_tvs = (&free_typevars - &src_tvs)
                    .iter()
                    .map(|tv| tv.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                panic!("Some free vars don't appear in src: {}", missing_tvs);
            }
        }

        for &var_index in input_vars.iter().chain(defined_vars.iter()) {
            let var = var_pool.get_mut(var_index);
            let canon_tv = type_env.get_equivalent(&var.get_or_create_typevar());
            var.set_typevar(canon_tv);
        }

        Self {
            src,
            dst,
            var_pool,
            def_pool,
            block_pool,
            const_pool,
            type_env,
        }
    }

    fn verify_legalize(&self) {
        let def = self.def_pool.get(self.src);
        for &var_index in def.defined_vars.iter() {
            let defined_var = self.var_pool.get(var_index);
            assert!(
                defined_var.is_output(),
                "{:?} not defined in the destination pattern",
                defined_var
            );
        }
    }
}

/// Inserts, if not present, a name in the `symbol_table`. Then returns its index in the variable
/// pool `var_pool`. If the variable was not present in the symbol table, then add it to the list of
/// `defined_vars`.
fn var_index(
    name: &str,
    symbol_table: &mut SymbolTable,
    defined_vars: &mut Vec<VarIndex>,
    var_pool: &mut VarPool,
) -> VarIndex {
    let name = name.to_string();
    match symbol_table.get(&name) {
        Some(&existing_var) => existing_var,
        None => {
            // Materialize the variable.
            let new_var = var_pool.create(name.clone());
            symbol_table.insert(name, new_var);
            defined_vars.push(new_var);
            new_var
        }
    }
}

/// Given a list of symbols defined in a Def, rewrite them to local symbols. Yield the new locals.
fn rewrite_defined_vars(
    position: PatternPosition,
    dummy_def: &DummyDef,
    def_index: DefIndex,
    symbol_table: &mut SymbolTable,
    defined_vars: &mut Vec<VarIndex>,
    var_pool: &mut VarPool,
) -> Vec<VarIndex> {
    let mut new_defined_vars = Vec::new();
    for var in &dummy_def.defined_vars {
        let own_var = var_index(&var.name, symbol_table, defined_vars, var_pool);
        var_pool.get_mut(own_var).set_def(position, def_index);
        new_defined_vars.push(own_var);
    }
    new_defined_vars
}

/// Find all uses of variables in `expr` and replace them with our own local symbols.
fn rewrite_expr(
    position: PatternPosition,
    dummy_expr: DummyExpr,
    symbol_table: &mut SymbolTable,
    input_vars: &mut Vec<VarIndex>,
    var_pool: &mut VarPool,
    const_pool: &mut ConstPool,
) -> Apply {
    let (apply_target, dummy_args) = if let DummyExpr::Apply(apply_target, dummy_args) = dummy_expr
    {
        (apply_target, dummy_args)
    } else {
        panic!("we only rewrite apply expressions");
    };

    assert_eq!(
        apply_target.inst().operands_in.len(),
        dummy_args.len(),
        "number of arguments in instruction {} is incorrect\nexpected: {:?}",
        apply_target.inst().name,
        apply_target
            .inst()
            .operands_in
            .iter()
            .map(|operand| format!("{}: {}", operand.name, operand.kind.rust_type))
            .collect::<Vec<_>>(),
    );

    let mut args = Vec::new();
    for (i, arg) in dummy_args.into_iter().enumerate() {
        match arg {
            DummyExpr::Var(var) => {
                let own_var = var_index(&var.name, symbol_table, input_vars, var_pool);
                let var = var_pool.get(own_var);
                assert!(
                    var.is_input() || var.get_def(position).is_some(),
                    "{:?} used as both input and def",
                    var
                );
                args.push(Expr::Var(own_var));
            }
            DummyExpr::Literal(literal) => {
                assert!(!apply_target.inst().operands_in[i].is_value());
                args.push(Expr::Literal(literal));
            }
            DummyExpr::Constant(constant) => {
                let const_name = const_pool.insert(constant.0);
                // Here we abuse var_index by passing an empty, immediately-dropped vector to
                // `defined_vars`; the reason for this is that unlike the `Var` case above,
                // constants will create a variable that is not an input variable (it is tracked
                // instead by ConstPool).
                let const_var = var_index(&const_name, symbol_table, &mut vec![], var_pool);
                args.push(Expr::Var(const_var));
            }
            DummyExpr::Apply(..) => {
                panic!("Recursive apply is not allowed.");
            }
            DummyExpr::Block(_block) => {
                panic!("Blocks are not valid arguments.");
            }
        }
    }

    Apply::new(apply_target, args)
}

#[allow(clippy::too_many_arguments)]
fn rewrite_def_list(
    position: PatternPosition,
    dummy_defs: Vec<DummyDef>,
    symbol_table: &mut SymbolTable,
    input_vars: &mut Vec<VarIndex>,
    defined_vars: &mut Vec<VarIndex>,
    var_pool: &mut VarPool,
    def_pool: &mut DefPool,
    block_pool: &mut BlockPool,
    const_pool: &mut ConstPool,
) -> Vec<DefIndex> {
    let mut new_defs = Vec::new();
    // Register variable names of new blocks first as a block name can be used to jump forward. Thus
    // the name has to be registered first to avoid misinterpreting it as an input-var.
    for dummy_def in dummy_defs.iter() {
        if let DummyExpr::Block(ref var) = dummy_def.expr {
            var_index(&var.name, symbol_table, defined_vars, var_pool);
        }
    }

    // Iterate over the definitions and blocks, to map variables names to inputs or outputs.
    for dummy_def in dummy_defs {
        let def_index = def_pool.next_index();

        let new_defined_vars = rewrite_defined_vars(
            position,
            &dummy_def,
            def_index,
            symbol_table,
            defined_vars,
            var_pool,
        );
        if let DummyExpr::Block(var) = dummy_def.expr {
            let var_index = *symbol_table
                .get(&var.name)
                .or_else(|| {
                    panic!(
                        "Block {} was not registered during the first visit",
                        var.name
                    )
                })
                .unwrap();
            var_pool.get_mut(var_index).set_def(position, def_index);
            block_pool.create_block(var_index, def_index);
        } else {
            let new_apply = rewrite_expr(
                position,
                dummy_def.expr,
                symbol_table,
                input_vars,
                var_pool,
                const_pool,
            );

            assert!(
                def_pool.next_index() == def_index,
                "shouldn't have created new defs in the meanwhile"
            );
            assert_eq!(
                new_apply.inst.value_results.len(),
                new_defined_vars.len(),
                "number of Var results in instruction is incorrect"
            );

            new_defs.push(def_pool.create_inst(new_apply, new_defined_vars));
        }
    }
    new_defs
}

/// A group of related transformations.
pub(crate) struct TransformGroup {
    pub name: &'static str,
    pub doc: &'static str,
    pub chain_with: Option<TransformGroupIndex>,
    pub isa_name: Option<&'static str>,
    pub id: TransformGroupIndex,

    /// Maps Instruction camel_case names to custom legalization functions names.
    pub custom_legalizes: HashMap<String, &'static str>,
    pub transforms: Vec<Transform>,
}

impl TransformGroup {
    pub fn rust_name(&self) -> String {
        match self.isa_name {
            Some(_) => {
                // This is a function in the same module as the LEGALIZE_ACTIONS table referring to
                // it.
                self.name.to_string()
            }
            None => format!("crate::legalizer::{}", self.name),
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct TransformGroupIndex(u32);
entity_impl!(TransformGroupIndex);

pub(crate) struct TransformGroupBuilder {
    name: &'static str,
    doc: &'static str,
    chain_with: Option<TransformGroupIndex>,
    isa_name: Option<&'static str>,
    pub custom_legalizes: HashMap<String, &'static str>,
    pub transforms: Vec<Transform>,
}

impl TransformGroupBuilder {
    pub fn new(name: &'static str, doc: &'static str) -> Self {
        Self {
            name,
            doc,
            chain_with: None,
            isa_name: None,
            custom_legalizes: HashMap::new(),
            transforms: Vec::new(),
        }
    }

    pub fn chain_with(mut self, next_id: TransformGroupIndex) -> Self {
        assert!(self.chain_with.is_none());
        self.chain_with = Some(next_id);
        self
    }

    /// Add a custom legalization action for `inst`.
    ///
    /// The `func_name` parameter is the fully qualified name of a Rust function which takes the
    /// same arguments as the `isa::Legalize` actions.
    ///
    /// The custom function will be called to legalize `inst` and any return value is ignored.
    pub fn custom_legalize(&mut self, inst: &Instruction, func_name: &'static str) {
        assert!(
            self.custom_legalizes
                .insert(inst.camel_name.clone(), func_name)
                .is_none(),
            "custom legalization action for {} inserted twice",
            inst.name
        );
    }

    /// Add a legalization pattern to this group.
    pub fn legalize(&mut self, src: DummyDef, dst: Vec<DummyDef>) {
        let transform = Transform::new(src, dst);
        transform.verify_legalize();
        self.transforms.push(transform);
    }

    pub fn build_and_add_to(self, owner: &mut TransformGroups) -> TransformGroupIndex {
        let next_id = owner.next_key();
        owner.add(TransformGroup {
            name: self.name,
            doc: self.doc,
            isa_name: self.isa_name,
            id: next_id,
            chain_with: self.chain_with,
            custom_legalizes: self.custom_legalizes,
            transforms: self.transforms,
        })
    }
}

pub(crate) struct TransformGroups {
    groups: PrimaryMap<TransformGroupIndex, TransformGroup>,
}

impl TransformGroups {
    pub fn new() -> Self {
        Self {
            groups: PrimaryMap::new(),
        }
    }
    pub fn add(&mut self, new_group: TransformGroup) -> TransformGroupIndex {
        for group in self.groups.values() {
            assert!(
                group.name != new_group.name,
                "trying to insert {} for the second time",
                new_group.name
            );
        }
        self.groups.push(new_group)
    }
    pub fn get(&self, id: TransformGroupIndex) -> &TransformGroup {
        &self.groups[id]
    }
    fn next_key(&self) -> TransformGroupIndex {
        self.groups.next_key()
    }
    pub fn by_name(&self, name: &'static str) -> &TransformGroup {
        for group in self.groups.values() {
            if group.name == name {
                return group;
            }
        }
        panic!("transform group with name {} not found", name);
    }
}

#[test]
#[should_panic]
fn test_double_custom_legalization() {
    use crate::cdsl::formats::InstructionFormatBuilder;
    use crate::cdsl::instructions::{AllInstructions, InstructionBuilder, InstructionGroupBuilder};

    let nullary = InstructionFormatBuilder::new("nullary").build();

    let mut dummy_all = AllInstructions::new();
    let mut inst_group = InstructionGroupBuilder::new(&mut dummy_all);
    inst_group.push(InstructionBuilder::new("dummy", "doc", &nullary));

    let inst_group = inst_group.build();
    let dummy_inst = inst_group.by_name("dummy");

    let mut transform_group = TransformGroupBuilder::new("test", "doc");
    transform_group.custom_legalize(&dummy_inst, "custom 1");
    transform_group.custom_legalize(&dummy_inst, "custom 2");
}
