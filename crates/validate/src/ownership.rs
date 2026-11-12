//! Explicit document scopes and the bounded part graph used by owner-local joins.
use crate::{Package, Resolution, package};
use std::collections::{BTreeMap, BTreeSet};

/// A normalized package identity; original entry names remain available for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PartId(String);
impl PartId {
    /// Package paths compare case-insensitively, after relationship target normalization.
    pub(crate) fn new(path: &str) -> Self {
        Self(path.trim_start_matches('/').to_ascii_lowercase())
    }
    /// The normalized identity, never a file-system path.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Index into one graph's explicit owner contexts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct OwnerId(pub(crate) usize);

/// The source of an owner-local scope. Fallback is an explicit legacy supporting-part scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OwnerContext {
    Main(String),
    Glossary(String),
    Fallback,
}
impl OwnerContext {
    /// The established story root, or no root for the explicit fallback context.
    pub(crate) fn root(&self) -> Option<&str> {
        match self {
            Self::Main(name) | Self::Glossary(name) => Some(name),
            Self::Fallback => None,
        }
    }
}

/// Known memberships, established unowned status, and incomplete reachability are distinct.
#[derive(Debug)]
pub(crate) enum Membership {
    Owned(Vec<OwnerId>),
    Unowned,
    Unknown,
}

/// Root scopes do not depend on selecting a unique package glossary source.
pub(crate) fn owner_scopes(pkg: &Package) -> Vec<OwnerContext> {
    let mut scopes = Vec::new();
    if let Resolution::One(main) = &pkg.main {
        scopes.push(OwnerContext::Main(main.clone()));
    }
    if let Resolution::One(glossary) = &pkg.glossary
        && !scopes.iter().any(|o| o.root().is_some_and(|r| r.eq_ignore_ascii_case(glossary)))
    {
        scopes.push(OwnerContext::Glossary(glossary.clone()));
    }
    let roots: BTreeSet<_> = pkg
        .entries
        .iter()
        .filter(|e| !e.is_dir && pkg.content_types.of(&e.name) == Some(package::GLOSSARY_CONTENT_TYPE))
        .map(|e| e.name.clone())
        .collect();
    for root in roots {
        if !scopes.iter().any(|o| o.root().is_some_and(|r| r.eq_ignore_ascii_case(&root))) {
            scopes.push(OwnerContext::Glossary(root));
        }
    }
    if scopes.is_empty() {
        scopes.push(OwnerContext::Fallback);
    }
    scopes
}

/// Membership data, with no default membership in main for unreached entries.
#[derive(Debug)]
pub(crate) struct PartGraph {
    memberships: BTreeMap<PartId, Membership>,
}
impl PartGraph {
    /// Traverse each context independently, stopping at every other story root.
    pub(crate) fn new(pkg: &Package) -> Self {
        let owners = owner_scopes(pkg);
        let complete = crate::ownership_discovery_complete(pkg);
        let mut memberships = BTreeMap::new();
        for e in pkg.entries.iter().filter(|e| !e.is_dir) {
            memberships.insert(PartId::new(&e.name), if complete { Membership::Unowned } else { Membership::Unknown });
        }
        if owners == [OwnerContext::Fallback] {
            // Retain the documented no-main/lone-supporting-part scope without inventing main.
            if complete {
                for member in memberships.values_mut() {
                    *member = Membership::Owned(vec![OwnerId(0)]);
                }
            }
            return Self { memberships };
        }
        let roots: BTreeSet<_> = owners.iter().filter_map(OwnerContext::root).map(PartId::new).collect();
        for (index, owner) in owners.iter().enumerate() {
            let Some(root) = owner.root() else { continue };
            let root_id = PartId::new(root);
            let mut queue = vec![root.to_string()];
            let mut visited = BTreeSet::new();
            while let Some(part) = queue.pop() {
                let id = PartId::new(&part);
                if (roots.contains(&id) && id != root_id) || !visited.insert(id.clone()) {
                    continue;
                }
                match memberships.entry(id).or_insert(Membership::Unknown) {
                    Membership::Owned(contexts) => contexts.push(OwnerId(index)),
                    state => *state = Membership::Owned(vec![OwnerId(index)]),
                }
                let rel_name = package::rels_part_name(&part);
                if let Some(rp) = pkg.rels.iter().find(|r| r.name.eq_ignore_ascii_case(&rel_name)) {
                    for rel in &rp.rels {
                        if let Some(target) = &rel.target_abs
                            && let Some(name) = package::canonical_entry(pkg, target)
                        {
                            queue.push(name);
                        }
                    }
                }
            }
        }
        Self { memberships }
    }
    /// Every independently established context of this part.
    pub(crate) fn owners(&self, part: &str) -> &[OwnerId] {
        match self.memberships.get(&PartId::new(part)) {
            Some(Membership::Owned(owners)) => owners,
            Some(Membership::Unowned | Membership::Unknown) | None => &[],
        }
    }
    /// Whether discovery leaves membership uncertain, rather than established-unowned.
    pub(crate) fn unknown(&self, part: &str) -> bool {
        matches!(self.memberships.get(&PartId::new(part)), None | Some(Membership::Unknown))
    }
    /// Reached members of one context, including shared parts in each context.
    pub(crate) fn parts(&self, owner: OwnerId) -> impl Iterator<Item = &str> {
        self.memberships.iter().filter_map(move |(part, state)| match state {
            Membership::Owned(owners) if owners.contains(&owner) => Some(part.as_str()),
            _ => None,
        })
    }
}

/// Intentional package-registry scope for declarations in established-unowned parts.
/// Only CUSTOMXML-SCHEMA and informational unreferenced-comment coverage may use this.
/// Owner-local reference joins must use PartGraph directly.
pub(crate) struct PackageRegistryContext {
    contexts: BTreeMap<PartId, Vec<OwnerId>>,
}
impl PackageRegistryContext {
    /// Preserve reached contexts; attach established-unowned declarations to the primary registry.
    /// Unknown membership never establishes a package-registry absence conclusion.
    pub(crate) fn new(graph: &PartGraph) -> Self {
        let contexts = graph
            .memberships
            .iter()
            .filter_map(|(part, state)| {
                let owners = match state {
                    Membership::Owned(owners) => owners.clone(),
                    Membership::Unowned => vec![OwnerId(0)],
                    Membership::Unknown => return None,
                };
                Some((part.clone(), owners))
            })
            .collect();
        Self { contexts }
    }
    /// Registry contexts only; these are never story ownership memberships.
    pub(crate) fn contexts(&self, part: &str) -> &[OwnerId] {
        self.contexts.get(&PartId::new(part)).map_or(&[], Vec::as_slice)
    }
}
