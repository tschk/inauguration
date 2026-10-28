//! Borrow-region facts owned by a module, independent of its memory address.
use crate::core_ir::{BorrowRegion, RegionKind, UnifiedModule};

pub fn mark_noalias(module: &mut UnifiedModule, params: &[&str]) {
    if params.is_empty() {
        return;
    }
    let regions = &mut module.regions;
    let index = if let Some(index) = regions
        .iter()
        .position(|region| region.kind == RegionKind::Mut)
    {
        index
    } else {
        regions.push(BorrowRegion {
            id: "mut".into(),
            kind: RegionKind::Mut,
            noalias: Vec::new(),
        });
        regions.len() - 1
    };
    for name in params {
        if !regions[index]
            .noalias
            .iter()
            .any(|existing| existing == name)
        {
            regions[index].noalias.push((*name).into());
        }
    }
}

pub fn noalias_params(module: &UnifiedModule) -> Vec<String> {
    let mut names: Vec<_> = module
        .regions
        .iter()
        .filter(|region| region.kind == RegionKind::Mut)
        .flat_map(|region| region.noalias.iter().cloned())
        .collect();
    names.sort();
    names.dedup();
    names
}

pub fn regions(module: &UnifiedModule) -> Vec<BorrowRegion> {
    module.regions.clone()
}

pub fn clear(module: &mut UnifiedModule) {
    module.regions.clear();
}
