use crate::cdsl::typevar::TypeVar;

#[derive(Debug, Hash, PartialEq, Eq)]
pub(crate) enum Constraint {
    /// Constraint specifying that a type var tv1 must be wider than or equal to type var tv2 at
    /// runtime. This requires that:
    /// 1) They have the same number of lanes
    /// 2) In a lane tv1 has at least as many bits as tv2.
    WiderOrEq(TypeVar, TypeVar),
}
