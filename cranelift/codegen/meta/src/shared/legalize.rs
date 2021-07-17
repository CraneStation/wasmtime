use crate::cdsl::ast::{var, ExprBuilder, Literal};
use crate::cdsl::instructions::{Bindable, Instruction, InstructionGroup};
use crate::cdsl::xform::{TransformGroupBuilder, TransformGroups};

use crate::shared::immediates::Immediates;
use crate::shared::types::Float::{F32, F64};
use crate::shared::types::Int::{I128, I16, I32, I64, I8};
use cranelift_codegen_shared::condcodes::{CondCode, IntCC};

#[allow(clippy::many_single_char_names, clippy::cognitive_complexity)]
pub(crate) fn define(insts: &InstructionGroup, imm: &Immediates) -> TransformGroups {
    let mut narrow = TransformGroupBuilder::new(
        "narrow",
        r#"
        Legalize instructions by narrowing.

        The transformations in the 'narrow' group work by expressing
        instructions in terms of smaller types. Operations on vector types are
        expressed in terms of vector types with fewer lanes, and integer
        operations are expressed in terms of smaller integer types.
    "#,
    );

    let mut widen = TransformGroupBuilder::new(
        "widen",
        r#"
        Legalize instructions by widening.

        The transformations in the 'widen' group work by expressing
        instructions in terms of larger types.
    "#,
    );

    let mut expand = TransformGroupBuilder::new(
        "expand",
        r#"
        Legalize instructions by expansion.

        Rewrite instructions in terms of other instructions, generally
        operating on the same types as the original instructions.
    "#,
    );

    // List of instructions.
    let band = insts.by_name("band");
    let band_imm = insts.by_name("band_imm");
    let band_not = insts.by_name("band_not");
    let bint = insts.by_name("bint");
    let bitrev = insts.by_name("bitrev");
    let bnot = insts.by_name("bnot");
    let bor = insts.by_name("bor");
    let bor_imm = insts.by_name("bor_imm");
    let bor_not = insts.by_name("bor_not");
    let brnz = insts.by_name("brnz");
    let brz = insts.by_name("brz");
    let br_icmp = insts.by_name("br_icmp");
    let br_table = insts.by_name("br_table");
    let bxor = insts.by_name("bxor");
    let bxor_imm = insts.by_name("bxor_imm");
    let bxor_not = insts.by_name("bxor_not");
    let cls = insts.by_name("cls");
    let clz = insts.by_name("clz");
    let ctz = insts.by_name("ctz");
    let copy = insts.by_name("copy");
    let fabs = insts.by_name("fabs");
    let f32const = insts.by_name("f32const");
    let f64const = insts.by_name("f64const");
    let fcopysign = insts.by_name("fcopysign");
    let fcvt_from_sint = insts.by_name("fcvt_from_sint");
    let fneg = insts.by_name("fneg");
    let iadd = insts.by_name("iadd");
    let iadd_cin = insts.by_name("iadd_cin");
    let iadd_cout = insts.by_name("iadd_cout");
    let iadd_carry = insts.by_name("iadd_carry");
    let iadd_ifcin = insts.by_name("iadd_ifcin");
    let iadd_ifcout = insts.by_name("iadd_ifcout");
    let iadd_imm = insts.by_name("iadd_imm");
    let icmp = insts.by_name("icmp");
    let icmp_imm = insts.by_name("icmp_imm");
    let iconcat = insts.by_name("iconcat");
    let iconst = insts.by_name("iconst");
    let ifcmp = insts.by_name("ifcmp");
    let ifcmp_imm = insts.by_name("ifcmp_imm");
    let imax = insts.by_name("imax");
    let imin = insts.by_name("imin");
    let imul = insts.by_name("imul");
    let imul_imm = insts.by_name("imul_imm");
    let ireduce = insts.by_name("ireduce");
    let irsub_imm = insts.by_name("irsub_imm");
    let ishl = insts.by_name("ishl");
    let ishl_imm = insts.by_name("ishl_imm");
    let isplit = insts.by_name("isplit");
    let istore8 = insts.by_name("istore8");
    let istore16 = insts.by_name("istore16");
    let isub = insts.by_name("isub");
    let isub_bin = insts.by_name("isub_bin");
    let isub_bout = insts.by_name("isub_bout");
    let isub_borrow = insts.by_name("isub_borrow");
    let isub_ifbin = insts.by_name("isub_ifbin");
    let isub_ifbout = insts.by_name("isub_ifbout");
    let jump = insts.by_name("jump");
    let load = insts.by_name("load");
    let popcnt = insts.by_name("popcnt");
    let resumable_trapnz = insts.by_name("resumable_trapnz");
    let rotl = insts.by_name("rotl");
    let rotl_imm = insts.by_name("rotl_imm");
    let rotr = insts.by_name("rotr");
    let rotr_imm = insts.by_name("rotr_imm");
    let sdiv = insts.by_name("sdiv");
    let sdiv_imm = insts.by_name("sdiv_imm");
    let select = insts.by_name("select");
    let sextend = insts.by_name("sextend");
    let sshr = insts.by_name("sshr");
    let sshr_imm = insts.by_name("sshr_imm");
    let srem = insts.by_name("srem");
    let srem_imm = insts.by_name("srem_imm");
    let store = insts.by_name("store");
    let udiv = insts.by_name("udiv");
    let udiv_imm = insts.by_name("udiv_imm");
    let uextend = insts.by_name("uextend");
    let uload8 = insts.by_name("uload8");
    let uload16 = insts.by_name("uload16");
    let umax = insts.by_name("umax");
    let umin = insts.by_name("umin");
    let umulhi = insts.by_name("umulhi");
    let ushr = insts.by_name("ushr");
    let ushr_imm = insts.by_name("ushr_imm");
    let urem = insts.by_name("urem");
    let urem_imm = insts.by_name("urem_imm");
    let trapif = insts.by_name("trapif");
    let trapnz = insts.by_name("trapnz");
    let trapz = insts.by_name("trapz");

    // Custom expansions for memory objects.
    expand.custom_legalize(insts.by_name("global_value"), "expand_global_value");
    expand.custom_legalize(insts.by_name("heap_addr"), "expand_heap_addr");
    expand.custom_legalize(insts.by_name("table_addr"), "expand_table_addr");

    // Custom expansions for calls.
    expand.custom_legalize(insts.by_name("call"), "expand_call");

    // Custom expansions that need to change the CFG.
    // TODO: Add sufficient XForm syntax that we don't need to hand-code these.
    expand.custom_legalize(trapz, "expand_cond_trap");
    expand.custom_legalize(trapnz, "expand_cond_trap");
    expand.custom_legalize(resumable_trapnz, "expand_cond_trap");
    expand.custom_legalize(br_table, "expand_br_table");
    expand.custom_legalize(select, "expand_select");
    widen.custom_legalize(select, "expand_select"); // small ints

    // Custom expansions for floating point constants.
    // These expansions require bit-casting or creating constant pool entries.
    expand.custom_legalize(f32const, "expand_fconst");
    expand.custom_legalize(f64const, "expand_fconst");

    // Custom expansions for stack memory accesses.
    expand.custom_legalize(insts.by_name("stack_load"), "expand_stack_load");
    expand.custom_legalize(insts.by_name("stack_store"), "expand_stack_store");

    // Custom expansions for small stack memory acccess.
    widen.custom_legalize(insts.by_name("stack_load"), "expand_stack_load");
    widen.custom_legalize(insts.by_name("stack_store"), "expand_stack_store");

    // List of variables to reuse in patterns.
    let x = var("x");
    let y = var("y");
    let z = var("z");
    let a = var("a");
    let a1 = var("a1");
    let a2 = var("a2");
    let a3 = var("a3");
    let a4 = var("a4");
    let b = var("b");
    let b1 = var("b1");
    let b2 = var("b2");
    let b3 = var("b3");
    let b4 = var("b4");
    let b_in = var("b_in");
    let b_int = var("b_int");
    let c = var("c");
    let c1 = var("c1");
    let c2 = var("c2");
    let c3 = var("c3");
    let c4 = var("c4");
    let c_in = var("c_in");
    let c_int = var("c_int");
    let d = var("d");
    let d1 = var("d1");
    let d2 = var("d2");
    let d3 = var("d3");
    let d4 = var("d4");
    let e = var("e");
    let e1 = var("e1");
    let e2 = var("e2");
    let e3 = var("e3");
    let e4 = var("e4");
    let f = var("f");
    let f1 = var("f1");
    let f2 = var("f2");
    let xl = var("xl");
    let xh = var("xh");
    let yl = var("yl");
    let yh = var("yh");
    let al = var("al");
    let ah = var("ah");
    let cc = var("cc");
    let block = var("block");
    let ptr = var("ptr");
    let flags = var("flags");
    let offset = var("off");
    let vararg = var("vararg");

    narrow.custom_legalize(load, "narrow_load");
    narrow.custom_legalize(store, "narrow_store");

    // iconst.i64 can't be legalized in the meta langage (because integer literals can't be
    // embedded as part of arguments), so use a custom legalization for now.
    narrow.custom_legalize(iconst, "narrow_iconst");

    for &(ty, ty_half) in &[(I128, I64), (I64, I32)] {
        let inst = uextend.bind(ty).bind(ty_half);
        narrow.legalize(
            def!(a = inst(x)),
            vec![
                def!(ah = iconst(Literal::constant(&imm.imm64, 0))),
                def!(a = iconcat(x, ah)),
            ],
        );
    }

    for &(ty, ty_half, shift) in &[(I128, I64, 63), (I64, I32, 31)] {
        let inst = sextend.bind(ty).bind(ty_half);
        narrow.legalize(
            def!(a = inst(x)),
            vec![
                def!(ah = sshr_imm(x, Literal::constant(&imm.imm64, shift))), // splat sign bit to whole number
                def!(a = iconcat(x, ah)),
            ],
        );
    }

    for &bin_op in &[band, bor, bxor, band_not, bor_not, bxor_not] {
        narrow.legalize(
            def!(a = bin_op(x, y)),
            vec![
                def!((xl, xh) = isplit(x)),
                def!((yl, yh) = isplit(y)),
                def!(al = bin_op(xl, yl)),
                def!(ah = bin_op(xh, yh)),
                def!(a = iconcat(al, ah)),
            ],
        );
    }

    narrow.legalize(
        def!(a = bnot(x)),
        vec![
            def!((xl, xh) = isplit(x)),
            def!(al = bnot(xl)),
            def!(ah = bnot(xh)),
            def!(a = iconcat(al, ah)),
        ],
    );

    narrow.legalize(
        def!(a = select(c, x, y)),
        vec![
            def!((xl, xh) = isplit(x)),
            def!((yl, yh) = isplit(y)),
            def!(al = select(c, xl, yl)),
            def!(ah = select(c, xh, yh)),
            def!(a = iconcat(al, ah)),
        ],
    );

    for &ty in &[I128, I64] {
        let block = var("block");
        let block1 = var("block1");
        let block2 = var("block2");

        narrow.legalize(
            def!(brz.ty(x, block, vararg)),
            vec![
                def!((xl, xh) = isplit(x)),
                def!(
                    a = icmp_imm(
                        Literal::enumerator_for(&imm.intcc, "eq"),
                        xl,
                        Literal::constant(&imm.imm64, 0)
                    )
                ),
                def!(
                    b = icmp_imm(
                        Literal::enumerator_for(&imm.intcc, "eq"),
                        xh,
                        Literal::constant(&imm.imm64, 0)
                    )
                ),
                def!(c = band(a, b)),
                def!(brnz(c, block, vararg)),
            ],
        );

        narrow.legalize(
            def!(brnz.ty(x, block1, vararg)),
            vec![
                def!((xl, xh) = isplit(x)),
                def!(brnz(xl, block1, vararg)),
                def!(jump(block2, Literal::empty_vararg())),
                block!(block2),
                def!(brnz(xh, block1, vararg)),
            ],
        );
    }

    narrow.legalize(
        def!(a = popcnt.I128(x)),
        vec![
            def!((xl, xh) = isplit(x)),
            def!(e1 = popcnt(xl)),
            def!(e2 = popcnt(xh)),
            def!(e3 = iadd(e1, e2)),
            def!(a = uextend(e3)),
        ],
    );

    // TODO(ryzokuken): benchmark this and decide if branching is a faster
    // approach than evaluating boolean expressions.

    narrow.custom_legalize(icmp_imm, "narrow_icmp_imm");

    let intcc_eq = Literal::enumerator_for(&imm.intcc, "eq");
    let intcc_ne = Literal::enumerator_for(&imm.intcc, "ne");
    for &(int_ty, int_ty_half) in &[(I64, I32), (I128, I64)] {
        narrow.legalize(
            def!(b = icmp.int_ty(intcc_eq, x, y)),
            vec![
                def!((xl, xh) = isplit(x)),
                def!((yl, yh) = isplit(y)),
                def!(b1 = icmp.int_ty_half(intcc_eq, xl, yl)),
                def!(b2 = icmp.int_ty_half(intcc_eq, xh, yh)),
                def!(b = band(b1, b2)),
            ],
        );

        narrow.legalize(
            def!(b = icmp.int_ty(intcc_ne, x, y)),
            vec![
                def!((xl, xh) = isplit(x)),
                def!((yl, yh) = isplit(y)),
                def!(b1 = icmp.int_ty_half(intcc_ne, xl, yl)),
                def!(b2 = icmp.int_ty_half(intcc_ne, xh, yh)),
                def!(b = bor(b1, b2)),
            ],
        );

        use IntCC::*;
        for cc in &[
            SignedGreaterThan,
            SignedGreaterThanOrEqual,
            SignedLessThan,
            SignedLessThanOrEqual,
            UnsignedGreaterThan,
            UnsignedGreaterThanOrEqual,
            UnsignedLessThan,
            UnsignedLessThanOrEqual,
        ] {
            let intcc_cc = Literal::enumerator_for(&imm.intcc, cc.to_static_str());
            let cc1 = Literal::enumerator_for(&imm.intcc, cc.without_equal().to_static_str());
            let cc2 =
                Literal::enumerator_for(&imm.intcc, cc.inverse().without_equal().to_static_str());
            let cc3 = Literal::enumerator_for(&imm.intcc, cc.unsigned().to_static_str());
            narrow.legalize(
                def!(b = icmp.int_ty(intcc_cc, x, y)),
                vec![
                    def!((xl, xh) = isplit(x)),
                    def!((yl, yh) = isplit(y)),
                    // X = cc1 || (!cc2 && cc3)
                    def!(b1 = icmp.int_ty_half(cc1, xh, yh)),
                    def!(b2 = icmp.int_ty_half(cc2, xh, yh)),
                    def!(b3 = icmp.int_ty_half(cc3, xl, yl)),
                    def!(c1 = bnot(b2)),
                    def!(c2 = band(c1, b3)),
                    def!(b = bor(b1, c2)),
                ],
            );
        }
    }

    // TODO(ryzokuken): explore the perf diff w/ x86_umulx and consider have a
    // separate legalization for x86.
    for &ty in &[I64, I128] {
        narrow.legalize(
            def!(a = imul.ty(x, y)),
            vec![
                def!((xl, xh) = isplit(x)),
                def!((yl, yh) = isplit(y)),
                def!(a1 = imul(xh, yl)),
                def!(a2 = imul(xl, yh)),
                def!(a3 = iadd(a1, a2)),
                def!(a4 = umulhi(xl, yl)),
                def!(ah = iadd(a3, a4)),
                def!(al = imul(xl, yl)),
                def!(a = iconcat(al, ah)),
            ],
        );
    }

    let zero = Literal::constant(&imm.imm64, 0);
    narrow.legalize(
        def!(a = iadd_imm.I128(x, c)),
        vec![
            def!(yh = iconst.I64(zero)),
            def!(yl = iconst.I64(c)),
            def!(y = iconcat.I64(yh, yl)),
            def!(a = iadd(x, y)),
        ],
    );

    // Widen instructions with one input operand.
    for &op in &[bnot, popcnt] {
        for &int_ty in &[I8, I16] {
            widen.legalize(
                def!(a = op.int_ty(b)),
                vec![
                    def!(x = uextend.I32(b)),
                    def!(z = op.I32(x)),
                    def!(a = ireduce.int_ty(z)),
                ],
            );
        }
    }

    // Widen instructions with two input operands.
    let mut widen_two_arg = |signed: bool, op: &Instruction| {
        for &int_ty in &[I8, I16] {
            let sign_ext_op = if signed { sextend } else { uextend };
            widen.legalize(
                def!(a = op.int_ty(b, c)),
                vec![
                    def!(x = sign_ext_op.I32(b)),
                    def!(y = sign_ext_op.I32(c)),
                    def!(z = op.I32(x, y)),
                    def!(a = ireduce.int_ty(z)),
                ],
            );
        }
    };

    for bin_op in &[
        iadd, isub, imul, udiv, urem, band, bor, bxor, band_not, bor_not, bxor_not,
    ] {
        widen_two_arg(false, bin_op);
    }
    for bin_op in &[sdiv, srem] {
        widen_two_arg(true, bin_op);
    }

    // Widen instructions using immediate operands.
    let mut widen_imm = |signed: bool, op: &Instruction| {
        for &int_ty in &[I8, I16] {
            let sign_ext_op = if signed { sextend } else { uextend };
            widen.legalize(
                def!(a = op.int_ty(b, c)),
                vec![
                    def!(x = sign_ext_op.I32(b)),
                    def!(z = op.I32(x, c)),
                    def!(a = ireduce.int_ty(z)),
                ],
            );
        }
    };

    for bin_op in &[
        iadd_imm, imul_imm, udiv_imm, urem_imm, band_imm, bor_imm, bxor_imm, irsub_imm,
    ] {
        widen_imm(false, bin_op);
    }
    for bin_op in &[sdiv_imm, srem_imm] {
        widen_imm(true, bin_op);
    }

    for &(int_ty, num) in &[(I8, 24), (I16, 16)] {
        let imm = Literal::constant(&imm.imm64, -num);

        widen.legalize(
            def!(a = clz.int_ty(b)),
            vec![
                def!(c = uextend.I32(b)),
                def!(d = clz.I32(c)),
                def!(e = iadd_imm(d, imm)),
                def!(a = ireduce.int_ty(e)),
            ],
        );

        widen.legalize(
            def!(a = cls.int_ty(b)),
            vec![
                def!(c = sextend.I32(b)),
                def!(d = cls.I32(c)),
                def!(e = iadd_imm(d, imm)),
                def!(a = ireduce.int_ty(e)),
            ],
        );
    }

    for &(int_ty, num) in &[(I8, 1 << 8), (I16, 1 << 16)] {
        let num = Literal::constant(&imm.imm64, num);
        widen.legalize(
            def!(a = ctz.int_ty(b)),
            vec![
                def!(c = uextend.I32(b)),
                // When `b` is zero, returns the size of x in bits.
                def!(d = bor_imm(c, num)),
                def!(e = ctz.I32(d)),
                def!(a = ireduce.int_ty(e)),
            ],
        );
    }

    // iconst
    for &int_ty in &[I8, I16] {
        widen.legalize(
            def!(a = iconst.int_ty(b)),
            vec![def!(c = iconst.I32(b)), def!(a = ireduce.int_ty(c))],
        );
    }

    for &extend_op in &[uextend, sextend] {
        // The sign extension operators have two typevars: the result has one and controls the
        // instruction, then the input has one.
        let bound = extend_op.bind(I16).bind(I8);
        widen.legalize(
            def!(a = bound(b)),
            vec![def!(c = extend_op.I32(b)), def!(a = ireduce(c))],
        );
    }

    widen.legalize(
        def!(store.I8(flags, a, ptr, offset)),
        vec![
            def!(b = uextend.I32(a)),
            def!(istore8(flags, b, ptr, offset)),
        ],
    );

    widen.legalize(
        def!(store.I16(flags, a, ptr, offset)),
        vec![
            def!(b = uextend.I32(a)),
            def!(istore16(flags, b, ptr, offset)),
        ],
    );

    widen.legalize(
        def!(a = load.I8(flags, ptr, offset)),
        vec![
            def!(b = uload8.I32(flags, ptr, offset)),
            def!(a = ireduce(b)),
        ],
    );

    widen.legalize(
        def!(a = load.I16(flags, ptr, offset)),
        vec![
            def!(b = uload16.I32(flags, ptr, offset)),
            def!(a = ireduce(b)),
        ],
    );

    for &int_ty in &[I8, I16] {
        widen.legalize(
            def!(br_table.int_ty(x, y, z)),
            vec![def!(b = uextend.I32(x)), def!(br_table(b, y, z))],
        );
    }

    for &int_ty in &[I8, I16] {
        widen.legalize(
            def!(a = bint.int_ty(b)),
            vec![def!(x = bint.I32(b)), def!(a = ireduce.int_ty(x))],
        );
    }

    for &int_ty in &[I8, I16] {
        for &op in &[ishl, ishl_imm, ushr, ushr_imm] {
            widen.legalize(
                def!(a = op.int_ty(b, c)),
                vec![
                    def!(x = uextend.I32(b)),
                    def!(z = op.I32(x, c)),
                    def!(a = ireduce.int_ty(z)),
                ],
            );
        }

        for &op in &[sshr, sshr_imm] {
            widen.legalize(
                def!(a = op.int_ty(b, c)),
                vec![
                    def!(x = sextend.I32(b)),
                    def!(z = op.I32(x, c)),
                    def!(a = ireduce.int_ty(z)),
                ],
            );
        }

        for cc in &["eq", "ne", "ugt", "ult", "uge", "ule"] {
            let w_cc = Literal::enumerator_for(&imm.intcc, cc);
            widen.legalize(
                def!(a = icmp_imm.int_ty(w_cc, b, c)),
                vec![def!(x = uextend.I32(b)), def!(a = icmp_imm(w_cc, x, c))],
            );
            widen.legalize(
                def!(a = icmp.int_ty(w_cc, b, c)),
                vec![
                    def!(x = uextend.I32(b)),
                    def!(y = uextend.I32(c)),
                    def!(a = icmp.I32(w_cc, x, y)),
                ],
            );
        }

        for cc in &["sgt", "slt", "sge", "sle"] {
            let w_cc = Literal::enumerator_for(&imm.intcc, cc);
            widen.legalize(
                def!(a = icmp_imm.int_ty(w_cc, b, c)),
                vec![def!(x = sextend.I32(b)), def!(a = icmp_imm(w_cc, x, c))],
            );

            widen.legalize(
                def!(a = icmp.int_ty(w_cc, b, c)),
                vec![
                    def!(x = sextend.I32(b)),
                    def!(y = sextend.I32(c)),
                    def!(a = icmp(w_cc, x, y)),
                ],
            );
        }
    }

    for &ty in &[I8, I16] {
        widen.legalize(
            def!(brz.ty(x, block, vararg)),
            vec![def!(a = uextend.I32(x)), def!(brz(a, block, vararg))],
        );

        widen.legalize(
            def!(brnz.ty(x, block, vararg)),
            vec![def!(a = uextend.I32(x)), def!(brnz(a, block, vararg))],
        );
    }

    for &(ty_half, ty) in &[(I64, I128), (I32, I64)] {
        let inst = ireduce.bind(ty_half).bind(ty);
        expand.legalize(
            def!(a = inst(x)),
            vec![def!((b, c) = isplit(x)), def!(a = copy(b))],
        );
    }

    // Expand integer operations with carry for RISC architectures that don't have
    // the flags.
    let intcc_ult = Literal::enumerator_for(&imm.intcc, "ult");
    expand.legalize(
        def!((a, c) = iadd_cout(x, y)),
        vec![def!(a = iadd(x, y)), def!(c = icmp(intcc_ult, a, x))],
    );

    let intcc_ugt = Literal::enumerator_for(&imm.intcc, "ugt");
    expand.legalize(
        def!((a, b) = isub_bout(x, y)),
        vec![def!(a = isub(x, y)), def!(b = icmp(intcc_ugt, a, x))],
    );

    expand.legalize(
        def!(a = iadd_cin(x, y, c)),
        vec![
            def!(a1 = iadd(x, y)),
            def!(c_int = bint(c)),
            def!(a = iadd(a1, c_int)),
        ],
    );

    expand.legalize(
        def!(a = isub_bin(x, y, b)),
        vec![
            def!(a1 = isub(x, y)),
            def!(b_int = bint(b)),
            def!(a = isub(a1, b_int)),
        ],
    );

    expand.legalize(
        def!((a, c) = iadd_carry(x, y, c_in)),
        vec![
            def!((a1, c1) = iadd_cout(x, y)),
            def!(c_int = bint(c_in)),
            def!((a, c2) = iadd_cout(a1, c_int)),
            def!(c = bor(c1, c2)),
        ],
    );

    expand.legalize(
        def!((a, b) = isub_borrow(x, y, b_in)),
        vec![
            def!((a1, b1) = isub_bout(x, y)),
            def!(b_int = bint(b_in)),
            def!((a, b2) = isub_bout(a1, b_int)),
            def!(b = bor(b1, b2)),
        ],
    );

    // Expansion for fcvt_from_sint for smaller integer types.
    // This uses expand and not widen because the controlling type variable for
    // this instruction is f32/f64, which is legalized as part of the expand
    // group.
    for &dest_ty in &[F32, F64] {
        for &src_ty in &[I8, I16] {
            let bound_inst = fcvt_from_sint.bind(dest_ty).bind(src_ty);
            expand.legalize(
                def!(a = bound_inst(b)),
                vec![
                    def!(x = sextend.I32(b)),
                    def!(a = fcvt_from_sint.dest_ty(x)),
                ],
            );
        }
    }

    // Expansions for immediate operands that are out of range.
    for &(inst_imm, inst) in &[
        (iadd_imm, iadd),
        (imul_imm, imul),
        (sdiv_imm, sdiv),
        (udiv_imm, udiv),
        (srem_imm, srem),
        (urem_imm, urem),
        (band_imm, band),
        (bor_imm, bor),
        (bxor_imm, bxor),
        (ifcmp_imm, ifcmp),
    ] {
        expand.legalize(
            def!(a = inst_imm(x, y)),
            vec![def!(a1 = iconst(y)), def!(a = inst(x, a1))],
        );
    }

    expand.legalize(
        def!(a = irsub_imm(y, x)),
        vec![def!(a1 = iconst(x)), def!(a = isub(a1, y))],
    );

    // Rotates and shifts.
    for &(inst_imm, inst) in &[
        (rotl_imm, rotl),
        (rotr_imm, rotr),
        (ishl_imm, ishl),
        (sshr_imm, sshr),
        (ushr_imm, ushr),
    ] {
        expand.legalize(
            def!(a = inst_imm(x, y)),
            vec![def!(a1 = iconst.I32(y)), def!(a = inst(x, a1))],
        );
    }

    // Expand scalar [i,u] min/max using icmp and select.
    for &(inst, cc) in &[(imin, "slt"), (imax, "sgt"), (umin, "ult"), (umax, "ugt")] {
        let w_cc = Literal::enumerator_for(&imm.intcc, cc);
        expand.legalize(
            def!(a = inst(x, y)),
            vec![def!(c = icmp(w_cc, x, y)), def!(a = select(c, x, y))],
        );
        for &int_ty in &[I8, I16] {
            widen.legalize(
                def!(a = inst.int_ty(x, y)),
                vec![def!(c = icmp(w_cc, x, y)), def!(a = select(c, x, y))],
            )
        }
    }

    expand.legalize(
        def!(a = icmp_imm(cc, x, y)),
        vec![def!(a1 = iconst(y)), def!(a = icmp(cc, x, a1))],
    );

    //# Expansions for *_not variants of bitwise ops.
    for &(inst_not, inst) in &[(band_not, band), (bor_not, bor), (bxor_not, bxor)] {
        expand.legalize(
            def!(a = inst_not(x, y)),
            vec![def!(a1 = bnot(y)), def!(a = inst(x, a1))],
        );
    }

    //# Expand bnot using xor.
    let minus_one = Literal::constant(&imm.imm64, -1);
    expand.legalize(
        def!(a = bnot(x)),
        vec![def!(y = iconst(minus_one)), def!(a = bxor(x, y))],
    );

    //# Expand bitrev
    //# Adapted from Stack Overflow.
    //# https://stackoverflow.com/questions/746171/most-efficient-algorithm-for-bit-reversal-from-msb-lsb-to-lsb-msb-in-c
    let imm64_1 = Literal::constant(&imm.imm64, 1);
    let imm64_2 = Literal::constant(&imm.imm64, 2);
    let imm64_4 = Literal::constant(&imm.imm64, 4);

    widen.legalize(
        def!(a = bitrev.I8(x)),
        vec![
            def!(a1 = band_imm(x, Literal::constant(&imm.imm64, 0xaa))),
            def!(a2 = ushr_imm(a1, imm64_1)),
            def!(a3 = band_imm(x, Literal::constant(&imm.imm64, 0x55))),
            def!(a4 = ishl_imm(a3, imm64_1)),
            def!(b = bor(a2, a4)),
            def!(b1 = band_imm(b, Literal::constant(&imm.imm64, 0xcc))),
            def!(b2 = ushr_imm(b1, imm64_2)),
            def!(b3 = band_imm(b, Literal::constant(&imm.imm64, 0x33))),
            def!(b4 = ishl_imm(b3, imm64_2)),
            def!(c = bor(b2, b4)),
            def!(c1 = band_imm(c, Literal::constant(&imm.imm64, 0xf0))),
            def!(c2 = ushr_imm(c1, imm64_4)),
            def!(c3 = band_imm(c, Literal::constant(&imm.imm64, 0x0f))),
            def!(c4 = ishl_imm(c3, imm64_4)),
            def!(a = bor(c2, c4)),
        ],
    );

    let imm64_8 = Literal::constant(&imm.imm64, 8);

    widen.legalize(
        def!(a = bitrev.I16(x)),
        vec![
            def!(a1 = band_imm(x, Literal::constant(&imm.imm64, 0xaaaa))),
            def!(a2 = ushr_imm(a1, imm64_1)),
            def!(a3 = band_imm(x, Literal::constant(&imm.imm64, 0x5555))),
            def!(a4 = ishl_imm(a3, imm64_1)),
            def!(b = bor(a2, a4)),
            def!(b1 = band_imm(b, Literal::constant(&imm.imm64, 0xcccc))),
            def!(b2 = ushr_imm(b1, imm64_2)),
            def!(b3 = band_imm(b, Literal::constant(&imm.imm64, 0x3333))),
            def!(b4 = ishl_imm(b3, imm64_2)),
            def!(c = bor(b2, b4)),
            def!(c1 = band_imm(c, Literal::constant(&imm.imm64, 0xf0f0))),
            def!(c2 = ushr_imm(c1, imm64_4)),
            def!(c3 = band_imm(c, Literal::constant(&imm.imm64, 0x0f0f))),
            def!(c4 = ishl_imm(c3, imm64_4)),
            def!(d = bor(c2, c4)),
            def!(d1 = band_imm(d, Literal::constant(&imm.imm64, 0xff00))),
            def!(d2 = ushr_imm(d1, imm64_8)),
            def!(d3 = band_imm(d, Literal::constant(&imm.imm64, 0x00ff))),
            def!(d4 = ishl_imm(d3, imm64_8)),
            def!(a = bor(d2, d4)),
        ],
    );

    let imm64_16 = Literal::constant(&imm.imm64, 16);

    expand.legalize(
        def!(a = bitrev.I32(x)),
        vec![
            def!(a1 = band_imm(x, Literal::constant(&imm.imm64, 0xaaaa_aaaa))),
            def!(a2 = ushr_imm(a1, imm64_1)),
            def!(a3 = band_imm(x, Literal::constant(&imm.imm64, 0x5555_5555))),
            def!(a4 = ishl_imm(a3, imm64_1)),
            def!(b = bor(a2, a4)),
            def!(b1 = band_imm(b, Literal::constant(&imm.imm64, 0xcccc_cccc))),
            def!(b2 = ushr_imm(b1, imm64_2)),
            def!(b3 = band_imm(b, Literal::constant(&imm.imm64, 0x3333_3333))),
            def!(b4 = ishl_imm(b3, imm64_2)),
            def!(c = bor(b2, b4)),
            def!(c1 = band_imm(c, Literal::constant(&imm.imm64, 0xf0f0_f0f0))),
            def!(c2 = ushr_imm(c1, imm64_4)),
            def!(c3 = band_imm(c, Literal::constant(&imm.imm64, 0x0f0f_0f0f))),
            def!(c4 = ishl_imm(c3, imm64_4)),
            def!(d = bor(c2, c4)),
            def!(d1 = band_imm(d, Literal::constant(&imm.imm64, 0xff00_ff00))),
            def!(d2 = ushr_imm(d1, imm64_8)),
            def!(d3 = band_imm(d, Literal::constant(&imm.imm64, 0x00ff_00ff))),
            def!(d4 = ishl_imm(d3, imm64_8)),
            def!(e = bor(d2, d4)),
            def!(e1 = ushr_imm(e, imm64_16)),
            def!(e2 = ishl_imm(e, imm64_16)),
            def!(a = bor(e1, e2)),
        ],
    );

    #[allow(overflowing_literals)]
    let imm64_0xaaaaaaaaaaaaaaaa = Literal::constant(&imm.imm64, 0xaaaa_aaaa_aaaa_aaaa);
    let imm64_0x5555555555555555 = Literal::constant(&imm.imm64, 0x5555_5555_5555_5555);
    #[allow(overflowing_literals)]
    let imm64_0xcccccccccccccccc = Literal::constant(&imm.imm64, 0xcccc_cccc_cccc_cccc);
    let imm64_0x3333333333333333 = Literal::constant(&imm.imm64, 0x3333_3333_3333_3333);
    #[allow(overflowing_literals)]
    let imm64_0xf0f0f0f0f0f0f0f0 = Literal::constant(&imm.imm64, 0xf0f0_f0f0_f0f0_f0f0);
    let imm64_0x0f0f0f0f0f0f0f0f = Literal::constant(&imm.imm64, 0x0f0f_0f0f_0f0f_0f0f);
    #[allow(overflowing_literals)]
    let imm64_0xff00ff00ff00ff00 = Literal::constant(&imm.imm64, 0xff00_ff00_ff00_ff00);
    let imm64_0x00ff00ff00ff00ff = Literal::constant(&imm.imm64, 0x00ff_00ff_00ff_00ff);
    #[allow(overflowing_literals)]
    let imm64_0xffff0000ffff0000 = Literal::constant(&imm.imm64, 0xffff_0000_ffff_0000);
    let imm64_0x0000ffff0000ffff = Literal::constant(&imm.imm64, 0x0000_ffff_0000_ffff);
    let imm64_32 = Literal::constant(&imm.imm64, 32);

    expand.legalize(
        def!(a = bitrev.I64(x)),
        vec![
            def!(a1 = band_imm(x, imm64_0xaaaaaaaaaaaaaaaa)),
            def!(a2 = ushr_imm(a1, imm64_1)),
            def!(a3 = band_imm(x, imm64_0x5555555555555555)),
            def!(a4 = ishl_imm(a3, imm64_1)),
            def!(b = bor(a2, a4)),
            def!(b1 = band_imm(b, imm64_0xcccccccccccccccc)),
            def!(b2 = ushr_imm(b1, imm64_2)),
            def!(b3 = band_imm(b, imm64_0x3333333333333333)),
            def!(b4 = ishl_imm(b3, imm64_2)),
            def!(c = bor(b2, b4)),
            def!(c1 = band_imm(c, imm64_0xf0f0f0f0f0f0f0f0)),
            def!(c2 = ushr_imm(c1, imm64_4)),
            def!(c3 = band_imm(c, imm64_0x0f0f0f0f0f0f0f0f)),
            def!(c4 = ishl_imm(c3, imm64_4)),
            def!(d = bor(c2, c4)),
            def!(d1 = band_imm(d, imm64_0xff00ff00ff00ff00)),
            def!(d2 = ushr_imm(d1, imm64_8)),
            def!(d3 = band_imm(d, imm64_0x00ff00ff00ff00ff)),
            def!(d4 = ishl_imm(d3, imm64_8)),
            def!(e = bor(d2, d4)),
            def!(e1 = band_imm(e, imm64_0xffff0000ffff0000)),
            def!(e2 = ushr_imm(e1, imm64_16)),
            def!(e3 = band_imm(e, imm64_0x0000ffff0000ffff)),
            def!(e4 = ishl_imm(e3, imm64_16)),
            def!(f = bor(e2, e4)),
            def!(f1 = ushr_imm(f, imm64_32)),
            def!(f2 = ishl_imm(f, imm64_32)),
            def!(a = bor(f1, f2)),
        ],
    );

    narrow.legalize(
        def!(a = bitrev.I128(x)),
        vec![
            def!((xl, xh) = isplit(x)),
            def!(yh = bitrev(xl)),
            def!(yl = bitrev(xh)),
            def!(a = iconcat(yl, yh)),
        ],
    );

    // Floating-point sign manipulations.
    for &(ty, const_inst, minus_zero) in &[
        (F32, f32const, &Literal::bits(&imm.ieee32, 0x8000_0000)),
        (
            F64,
            f64const,
            &Literal::bits(&imm.ieee64, 0x8000_0000_0000_0000),
        ),
    ] {
        expand.legalize(
            def!(a = fabs.ty(x)),
            vec![def!(b = const_inst(minus_zero)), def!(a = band_not(x, b))],
        );

        expand.legalize(
            def!(a = fneg.ty(x)),
            vec![def!(b = const_inst(minus_zero)), def!(a = bxor(x, b))],
        );

        expand.legalize(
            def!(a = fcopysign.ty(x, y)),
            vec![
                def!(b = const_inst(minus_zero)),
                def!(a1 = band_not(x, b)),
                def!(a2 = band(y, b)),
                def!(a = bor(a1, a2)),
            ],
        );
    }

    expand.custom_legalize(br_icmp, "expand_br_icmp");

    let mut groups = TransformGroups::new();

    let narrow_id = narrow.build_and_add_to(&mut groups);
    let expand_id = expand.build_and_add_to(&mut groups);

    // Expansions using CPU flags.
    let mut expand_flags = TransformGroupBuilder::new(
        "expand_flags",
        r#"
        Instruction expansions for architectures with flags.

        Expand some instructions using CPU flags, then fall back to the normal
        expansions. Not all architectures support CPU flags, so these patterns
        are kept separate.
    "#,
    )
    .chain_with(expand_id);

    let imm64_0 = Literal::constant(&imm.imm64, 0);
    let intcc_ne = Literal::enumerator_for(&imm.intcc, "ne");
    let intcc_eq = Literal::enumerator_for(&imm.intcc, "eq");

    expand_flags.legalize(
        def!(trapnz(x, c)),
        vec![
            def!(a = ifcmp_imm(x, imm64_0)),
            def!(trapif(intcc_ne, a, c)),
        ],
    );

    expand_flags.legalize(
        def!(trapz(x, c)),
        vec![
            def!(a = ifcmp_imm(x, imm64_0)),
            def!(trapif(intcc_eq, a, c)),
        ],
    );

    expand_flags.build_and_add_to(&mut groups);

    // Narrow legalizations using CPU flags.
    let mut narrow_flags = TransformGroupBuilder::new(
        "narrow_flags",
        r#"
        Narrow instructions for architectures with flags.

        Narrow some instructions using CPU flags, then fall back to the normal
        legalizations. Not all architectures support CPU flags, so these
        patterns are kept separate.
    "#,
    )
    .chain_with(narrow_id);

    narrow_flags.legalize(
        def!(a = iadd(x, y)),
        vec![
            def!((xl, xh) = isplit(x)),
            def!((yl, yh) = isplit(y)),
            def!((al, c) = iadd_ifcout(xl, yl)),
            def!(ah = iadd_ifcin(xh, yh, c)),
            def!(a = iconcat(al, ah)),
        ],
    );

    narrow_flags.legalize(
        def!(a = isub(x, y)),
        vec![
            def!((xl, xh) = isplit(x)),
            def!((yl, yh) = isplit(y)),
            def!((al, b) = isub_ifbout(xl, yl)),
            def!(ah = isub_ifbin(xh, yh, b)),
            def!(a = iconcat(al, ah)),
        ],
    );

    narrow_flags.build_and_add_to(&mut groups);

    // TODO(ryzokuken): figure out a way to legalize iadd_c* to iadd_ifc* (and
    // similarly isub_b* to isub_ifb*) on expand_flags so that this isn't required.
    // Narrow legalizations for ISAs that don't have CPU flags.
    let mut narrow_no_flags = TransformGroupBuilder::new(
        "narrow_no_flags",
        r#"
        Narrow instructions for architectures without flags.

        Narrow some instructions avoiding the use of CPU flags, then fall back
        to the normal legalizations. Not all architectures support CPU flags,
        so these patterns are kept separate.
    "#,
    )
    .chain_with(narrow_id);

    narrow_no_flags.legalize(
        def!(a = iadd(x, y)),
        vec![
            def!((xl, xh) = isplit(x)),
            def!((yl, yh) = isplit(y)),
            def!((al, c) = iadd_cout(xl, yl)),
            def!(ah = iadd_cin(xh, yh, c)),
            def!(a = iconcat(al, ah)),
        ],
    );

    narrow_no_flags.legalize(
        def!(a = isub(x, y)),
        vec![
            def!((xl, xh) = isplit(x)),
            def!((yl, yh) = isplit(y)),
            def!((al, b) = isub_bout(xl, yl)),
            def!(ah = isub_bin(xh, yh, b)),
            def!(a = iconcat(al, ah)),
        ],
    );

    narrow_no_flags.build_and_add_to(&mut groups);

    // TODO The order of declarations unfortunately matters to be compatible with the Python code.
    // When it's all migrated, we can put this next to the narrow/expand build_and_add_to calls
    // above.
    widen.build_and_add_to(&mut groups);

    groups
}
