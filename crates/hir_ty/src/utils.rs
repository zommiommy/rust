//! Helper functions for working with def, which don't need to be a separate
//! query, but can't be computed directly from `*Data` (ie, which need a `db`).

use std::iter;

use base_db::CrateId;
use chalk_ir::{fold::Shift, BoundVar, DebruijnIndex};
use hir_def::{
    db::DefDatabase,
    generics::{
        GenericParams, TypeOrConstParamData, TypeParamData, TypeParamProvenance, WherePredicate,
        WherePredicateTypeTarget,
    },
    intern::Interned,
    path::Path,
    resolver::{HasResolver, TypeNs},
    type_ref::{TraitBoundModifier, TypeRef},
    ConstParamId, FunctionId, GenericDefId, ItemContainerId, Lookup, TraitId, TypeAliasId,
    TypeOrConstParamId, TypeParamId,
};
use hir_expand::name::{known, name, Name};
use itertools::Either;
use rustc_hash::FxHashSet;
use smallvec::{smallvec, SmallVec};
use syntax::SmolStr;

use crate::{
    db::HirDatabase, ChalkTraitId, ConstData, ConstValue, GenericArgData, Interner, Substitution,
    TraitRef, TraitRefExt, TyKind, WhereClause,
};

pub(crate) fn fn_traits(db: &dyn DefDatabase, krate: CrateId) -> impl Iterator<Item = TraitId> {
    [
        db.lang_item(krate, SmolStr::new_inline("fn")),
        db.lang_item(krate, SmolStr::new_inline("fn_mut")),
        db.lang_item(krate, SmolStr::new_inline("fn_once")),
    ]
    .into_iter()
    .flatten()
    .flat_map(|it| it.as_trait())
}

fn direct_super_traits(db: &dyn DefDatabase, trait_: TraitId) -> SmallVec<[TraitId; 4]> {
    let resolver = trait_.resolver(db);
    // returning the iterator directly doesn't easily work because of
    // lifetime problems, but since there usually shouldn't be more than a
    // few direct traits this should be fine (we could even use some kind of
    // SmallVec if performance is a concern)
    let generic_params = db.generic_params(trait_.into());
    let trait_self = generic_params.find_trait_self_param();
    generic_params
        .where_predicates
        .iter()
        .filter_map(|pred| match pred {
            WherePredicate::ForLifetime { target, bound, .. }
            | WherePredicate::TypeBound { target, bound } => match target {
                WherePredicateTypeTarget::TypeRef(type_ref) => match &**type_ref {
                    TypeRef::Path(p) if p == &Path::from(name![Self]) => bound.as_path(),
                    _ => None,
                },
                WherePredicateTypeTarget::TypeOrConstParam(local_id)
                    if Some(*local_id) == trait_self =>
                {
                    bound.as_path()
                }
                _ => None,
            },
            WherePredicate::Lifetime { .. } => None,
        })
        .filter_map(|(path, bound_modifier)| match bound_modifier {
            TraitBoundModifier::None => Some(path),
            TraitBoundModifier::Maybe => None,
        })
        .filter_map(|path| match resolver.resolve_path_in_type_ns_fully(db, path.mod_path()) {
            Some(TypeNs::TraitId(t)) => Some(t),
            _ => None,
        })
        .collect()
}

fn direct_super_trait_refs(db: &dyn HirDatabase, trait_ref: &TraitRef) -> Vec<TraitRef> {
    // returning the iterator directly doesn't easily work because of
    // lifetime problems, but since there usually shouldn't be more than a
    // few direct traits this should be fine (we could even use some kind of
    // SmallVec if performance is a concern)
    let generic_params = db.generic_params(trait_ref.hir_trait_id().into());
    let trait_self = match generic_params.find_trait_self_param() {
        Some(p) => TypeOrConstParamId { parent: trait_ref.hir_trait_id().into(), local_id: p },
        None => return Vec::new(),
    };
    db.generic_predicates_for_param(trait_self.parent, trait_self, None)
        .iter()
        .filter_map(|pred| {
            pred.as_ref().filter_map(|pred| match pred.skip_binders() {
                // FIXME: how to correctly handle higher-ranked bounds here?
                WhereClause::Implemented(tr) => Some(
                    tr.clone()
                        .shifted_out_to(Interner, DebruijnIndex::ONE)
                        .expect("FIXME unexpected higher-ranked trait bound"),
                ),
                _ => None,
            })
        })
        .map(|pred| pred.substitute(Interner, &trait_ref.substitution))
        .collect()
}

/// Returns an iterator over the whole super trait hierarchy (including the
/// trait itself).
pub fn all_super_traits(db: &dyn DefDatabase, trait_: TraitId) -> SmallVec<[TraitId; 4]> {
    // we need to take care a bit here to avoid infinite loops in case of cycles
    // (i.e. if we have `trait A: B; trait B: A;`)

    let mut result = smallvec![trait_];
    let mut i = 0;
    while let Some(&t) = result.get(i) {
        // yeah this is quadratic, but trait hierarchies should be flat
        // enough that this doesn't matter
        for tt in direct_super_traits(db, t) {
            if !result.contains(&tt) {
                result.push(tt);
            }
        }
        i += 1;
    }
    result
}

/// Given a trait ref (`Self: Trait`), builds all the implied trait refs for
/// super traits. The original trait ref will be included. So the difference to
/// `all_super_traits` is that we keep track of type parameters; for example if
/// we have `Self: Trait<u32, i32>` and `Trait<T, U>: OtherTrait<U>` we'll get
/// `Self: OtherTrait<i32>`.
pub(super) fn all_super_trait_refs(db: &dyn HirDatabase, trait_ref: TraitRef) -> SuperTraits {
    SuperTraits { db, seen: iter::once(trait_ref.trait_id).collect(), stack: vec![trait_ref] }
}

pub(super) struct SuperTraits<'a> {
    db: &'a dyn HirDatabase,
    stack: Vec<TraitRef>,
    seen: FxHashSet<ChalkTraitId>,
}

impl<'a> SuperTraits<'a> {
    fn elaborate(&mut self, trait_ref: &TraitRef) {
        let mut trait_refs = direct_super_trait_refs(self.db, trait_ref);
        trait_refs.retain(|tr| !self.seen.contains(&tr.trait_id));
        self.stack.extend(trait_refs);
    }
}

impl<'a> Iterator for SuperTraits<'a> {
    type Item = TraitRef;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(next) = self.stack.pop() {
            self.elaborate(&next);
            Some(next)
        } else {
            None
        }
    }
}

pub(super) fn associated_type_by_name_including_super_traits(
    db: &dyn HirDatabase,
    trait_ref: TraitRef,
    name: &Name,
) -> Option<(TraitRef, TypeAliasId)> {
    all_super_trait_refs(db, trait_ref).find_map(|t| {
        let assoc_type = db.trait_data(t.hir_trait_id()).associated_type_by_name(name)?;
        Some((t, assoc_type))
    })
}

pub(crate) fn generics(db: &dyn DefDatabase, def: GenericDefId) -> Generics {
    let parent_generics = parent_generic_def(db, def).map(|def| Box::new(generics(db, def)));
    if parent_generics.is_some() && matches!(def, GenericDefId::TypeAliasId(_)) {
        // XXX: treat generic associated types as not existing to avoid crashes (#)
        //
        // Chalk expects the inner associated type's parameters to come
        // *before*, not after the trait's generics as we've always done it.
        // Adapting to this requires a larger refactoring
        cov_mark::hit!(ignore_gats);
        return Generics { def, params: Interned::new(Default::default()), parent_generics };
    }
    Generics { def, params: db.generic_params(def), parent_generics }
}

#[derive(Debug)]
pub(crate) struct Generics {
    def: GenericDefId,
    pub(crate) params: Interned<GenericParams>,
    parent_generics: Option<Box<Generics>>,
}

impl Generics {
    // FIXME: we should drop this and handle const and type generics at the same time
    pub(crate) fn type_iter<'a>(
        &'a self,
    ) -> impl Iterator<Item = (TypeOrConstParamId, &'a TypeParamData)> + 'a {
        self.parent_generics
            .as_ref()
            .into_iter()
            .flat_map(|it| {
                it.params
                    .type_iter()
                    .map(move |(local_id, p)| (TypeOrConstParamId { parent: it.def, local_id }, p))
            })
            .chain(
                self.params.type_iter().map(move |(local_id, p)| {
                    (TypeOrConstParamId { parent: self.def, local_id }, p)
                }),
            )
    }

    pub(crate) fn iter_id<'a>(
        &'a self,
    ) -> impl Iterator<Item = Either<TypeParamId, ConstParamId>> + 'a {
        self.iter().map(|(id, data)| match data {
            TypeOrConstParamData::TypeParamData(_) => Either::Left(TypeParamId::from_unchecked(id)),
            TypeOrConstParamData::ConstParamData(_) => {
                Either::Right(ConstParamId::from_unchecked(id))
            }
        })
    }

    /// Iterator over types and const params of parent, then self.
    pub(crate) fn iter<'a>(
        &'a self,
    ) -> impl DoubleEndedIterator<Item = (TypeOrConstParamId, &'a TypeOrConstParamData)> + 'a {
        self.parent_generics
            .as_ref()
            .into_iter()
            .flat_map(|it| {
                it.params
                    .iter()
                    .map(move |(local_id, p)| (TypeOrConstParamId { parent: it.def, local_id }, p))
            })
            .chain(
                self.params.iter().map(move |(local_id, p)| {
                    (TypeOrConstParamId { parent: self.def, local_id }, p)
                }),
            )
    }

    /// Iterator over types and const params of parent.
    pub(crate) fn iter_parent<'a>(
        &'a self,
    ) -> impl Iterator<Item = (TypeOrConstParamId, &'a TypeOrConstParamData)> + 'a {
        self.parent_generics.as_ref().into_iter().flat_map(|it| {
            it.params
                .type_or_consts
                .iter()
                .map(move |(local_id, p)| (TypeOrConstParamId { parent: it.def, local_id }, p))
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.len_split().0
    }

    /// (total, parents, child)
    pub(crate) fn len_split(&self) -> (usize, usize, usize) {
        let parent = self.parent_generics.as_ref().map_or(0, |p| p.len());
        let child = self.params.type_or_consts.len();
        (parent + child, parent, child)
    }

    /// (parent total, self param, type param list, const param list, impl trait)
    pub(crate) fn provenance_split(&self) -> (usize, usize, usize, usize, usize) {
        let parent = self.parent_generics.as_ref().map_or(0, |p| p.len());
        let self_params = self
            .params
            .iter()
            .filter_map(|x| x.1.type_param())
            .filter(|p| p.provenance == TypeParamProvenance::TraitSelf)
            .count();
        let type_params = self
            .params
            .type_or_consts
            .iter()
            .filter_map(|x| x.1.type_param())
            .filter(|p| p.provenance == TypeParamProvenance::TypeParamList)
            .count();
        let const_params = self.params.iter().filter_map(|x| x.1.const_param()).count();
        let impl_trait_params = self
            .params
            .iter()
            .filter_map(|x| x.1.type_param())
            .filter(|p| p.provenance == TypeParamProvenance::ArgumentImplTrait)
            .count();
        (parent, self_params, type_params, const_params, impl_trait_params)
    }

    pub(crate) fn param_idx(&self, param: TypeOrConstParamId) -> Option<usize> {
        Some(self.find_param(param)?.0)
    }

    fn find_param(&self, param: TypeOrConstParamId) -> Option<(usize, &TypeOrConstParamData)> {
        if param.parent == self.def {
            let (idx, (_local_id, data)) = self
                .params
                .type_or_consts
                .iter()
                .enumerate()
                .find(|(_, (idx, _))| *idx == param.local_id)
                .unwrap();
            let (_total, parent_len, _child) = self.len_split();
            Some((parent_len + idx, data))
        } else {
            self.parent_generics.as_ref().and_then(|g| g.find_param(param))
        }
    }

    /// Returns a Substitution that replaces each parameter by a bound variable.
    pub(crate) fn bound_vars_subst(
        &self,
        db: &dyn HirDatabase,
        debruijn: DebruijnIndex,
    ) -> Substitution {
        Substitution::from_iter(
            Interner,
            self.iter_id().enumerate().map(|(idx, id)| match id {
                Either::Left(_) => GenericArgData::Ty(
                    TyKind::BoundVar(BoundVar::new(debruijn, idx)).intern(Interner),
                )
                .intern(Interner),
                Either::Right(id) => GenericArgData::Const(
                    ConstData {
                        value: ConstValue::BoundVar(BoundVar::new(debruijn, idx)),
                        ty: db.const_param_ty(id),
                    }
                    .intern(Interner),
                )
                .intern(Interner),
            }),
        )
    }

    /// Returns a Substitution that replaces each parameter by itself (i.e. `Ty::Param`).
    pub(crate) fn placeholder_subst(&self, db: &dyn HirDatabase) -> Substitution {
        Substitution::from_iter(
            Interner,
            self.iter_id().map(|id| match id {
                Either::Left(id) => GenericArgData::Ty(
                    TyKind::Placeholder(crate::to_placeholder_idx(db, id.into())).intern(Interner),
                )
                .intern(Interner),
                Either::Right(id) => GenericArgData::Const(
                    ConstData {
                        value: ConstValue::Placeholder(crate::to_placeholder_idx(db, id.into())),
                        ty: db.const_param_ty(id),
                    }
                    .intern(Interner),
                )
                .intern(Interner),
            }),
        )
    }
}

fn parent_generic_def(db: &dyn DefDatabase, def: GenericDefId) -> Option<GenericDefId> {
    let container = match def {
        GenericDefId::FunctionId(it) => it.lookup(db).container,
        GenericDefId::TypeAliasId(it) => it.lookup(db).container,
        GenericDefId::ConstId(it) => it.lookup(db).container,
        GenericDefId::EnumVariantId(it) => return Some(it.parent.into()),
        GenericDefId::AdtId(_) | GenericDefId::TraitId(_) | GenericDefId::ImplId(_) => return None,
    };

    match container {
        ItemContainerId::ImplId(it) => Some(it.into()),
        ItemContainerId::TraitId(it) => Some(it.into()),
        ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => None,
    }
}

pub fn is_fn_unsafe_to_call(db: &dyn HirDatabase, func: FunctionId) -> bool {
    let data = db.function_data(func);
    if data.has_unsafe_kw() {
        return true;
    }

    match func.lookup(db.upcast()).container {
        hir_def::ItemContainerId::ExternBlockId(block) => {
            // Function in an `extern` block are always unsafe to call, except when it has
            // `"rust-intrinsic"` ABI there are a few exceptions.
            let id = block.lookup(db.upcast()).id;
            match id.item_tree(db.upcast())[id.value].abi.as_deref() {
                Some("rust-intrinsic") => is_intrinsic_fn_unsafe(&data.name),
                _ => true,
            }
        }
        _ => false,
    }
}

/// Returns `true` if the given intrinsic is unsafe to call, or false otherwise.
fn is_intrinsic_fn_unsafe(name: &Name) -> bool {
    // Should be kept in sync with https://github.com/rust-lang/rust/blob/532d2b14c05f9bc20b2d27cbb5f4550d28343a36/compiler/rustc_typeck/src/check/intrinsic.rs#L72-L106
    ![
        known::abort,
        known::add_with_overflow,
        known::bitreverse,
        known::black_box,
        known::bswap,
        known::caller_location,
        known::ctlz,
        known::ctpop,
        known::cttz,
        known::discriminant_value,
        known::forget,
        known::likely,
        known::maxnumf32,
        known::maxnumf64,
        known::min_align_of,
        known::minnumf32,
        known::minnumf64,
        known::mul_with_overflow,
        known::needs_drop,
        known::ptr_guaranteed_eq,
        known::ptr_guaranteed_ne,
        known::rotate_left,
        known::rotate_right,
        known::rustc_peek,
        known::saturating_add,
        known::saturating_sub,
        known::size_of,
        known::sub_with_overflow,
        known::type_id,
        known::type_name,
        known::unlikely,
        known::variant_count,
        known::wrapping_add,
        known::wrapping_mul,
        known::wrapping_sub,
    ]
    .contains(name)
}
