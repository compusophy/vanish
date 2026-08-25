//! L4 — ports: wiring a SET of cartridges by what they provide and require
//! (CARTRIDGE_PLAN §7, build-order item 6).
//!
//! pure. `wire` takes manifests and answers with a `Wiring` or a named
//! refusal, touching no bytes and no runtime — so every composition rule is
//! pinned by tests that need nothing but manifests, and a mis-wired set is
//! refused BEFORE a single linear memory is instantiated.
//!
//! the rules, each a D4 door:
//! - slugs are unique across the set (two "reasoner" cartridges cannot both
//!   be addressed).
//! - a port is provided by EXACTLY one cartridge in the set. names match
//!   exactly — no globs, no prefixes — because a fuzzy match is a silent
//!   mis-wiring, and two providers is an ambiguity the composer must not
//!   resolve by luck.
//! - every required port has its provider, or the refusal names the port
//!   AND every cartridge that wanted it, so the fix ("add a cartridge that
//!   provides X") is one read away — not a call-time failure hours later.
//! - the requirement graph is acyclic. lateral / up / down hierarchy is just
//!   a graph, so "apps made of apps" falls out of ports rather than being a
//!   special case; a cycle is refused with the loop written out.
//! - initialization order is providers-first and DETERMINISTIC (ties broken
//!   by slug), so two runs of the same set wire and boot identically.

use std::collections::{BTreeMap, BTreeSet};

use super::manifest::CartridgeManifest;

/// one satisfied requirement: `requirer` needs `port`, `provider` has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub requirer: String,
    pub port: String,
    pub provider: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Wiring {
    /// port name → the one slug that provides it.
    pub providers: BTreeMap<String, String>,
    /// every satisfied requirement, in manifest order then port order.
    pub edges: Vec<Edge>,
    /// initialization order: every provider strictly before anything that
    /// requires it; among the ready, smallest slug first.
    pub order: Vec<String>,
}

/// why a set of cartridges cannot be composed. every variant carries what
/// the fix needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// one manifest failed its own validation (see CartridgeManifest).
    Manifest { slug: String, reason: String },
    DuplicateSlug(String),
    AmbiguousProvider { port: String, providers: Vec<String> },
    MissingProvider { port: String, required_by: Vec<String> },
    /// the slugs around the loop, closed (first == last): a → b → a.
    Cycle(Vec<String>),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Manifest { slug, reason } => write!(f, "manifest '{slug}': {reason}"),
            WireError::DuplicateSlug(s) => {
                write!(f, "two cartridges are both named '{s}' — slugs must be unique in a set")
            }
            WireError::AmbiguousProvider { port, providers } => write!(
                f,
                "port '{port}' is provided by {} — exactly one provider per port; remove \
                 one or rename its port",
                providers
                    .iter()
                    .map(|p| format!("'{p}'"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
            WireError::MissingProvider { port, required_by } => write!(
                f,
                "port '{port}' is required by {} but nothing in the set provides it — add a \
                 cartridge whose manifest lists it under `provides`",
                required_by
                    .iter()
                    .map(|p| format!("'{p}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            WireError::Cycle(path) => write!(
                f,
                "requirement cycle: {} — a cartridge cannot (transitively) require itself",
                path.join(" → ")
            ),
        }
    }
}

/// compose a set. see the module doc for every rule; the order of checks is
/// the order a reader would want the news: bad manifest, duplicate slug,
/// ambiguous port, missing port, cycle.
pub fn wire(manifests: &[CartridgeManifest]) -> Result<Wiring, WireError> {
    for m in manifests {
        m.validate().map_err(|reason| WireError::Manifest {
            slug: m.slug.clone(),
            reason,
        })?;
    }

    let mut slugs = BTreeSet::new();
    for m in manifests {
        if !slugs.insert(m.slug.as_str()) {
            return Err(WireError::DuplicateSlug(m.slug.clone()));
        }
    }

    // providers: port → slugs. more than one is refused, not resolved.
    let mut by_port: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for m in manifests {
        for p in &m.provides {
            by_port
                .entry(p.name.as_str())
                .or_default()
                .push(m.slug.as_str());
        }
    }
    let mut providers: BTreeMap<String, String> = BTreeMap::new();
    for (port, ps) in &by_port {
        if ps.len() > 1 {
            let mut providers: Vec<String> = ps.iter().map(|s| s.to_string()).collect();
            providers.sort();
            return Err(WireError::AmbiguousProvider {
                port: port.to_string(),
                providers,
            });
        }
        providers.insert(port.to_string(), ps[0].to_string());
    }

    // requirements → edges; the first missing port (by name) is reported
    // with EVERY requirer, so one refusal names the whole blast radius.
    let mut missing: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut edges = Vec::new();
    for m in manifests {
        for r in &m.requires {
            match providers.get(&r.name) {
                Some(p) => edges.push(Edge {
                    requirer: m.slug.clone(),
                    port: r.name.clone(),
                    provider: p.clone(),
                }),
                None => missing
                    .entry(r.name.as_str())
                    .or_default()
                    .push(m.slug.clone()),
            }
        }
    }
    if let Some((port, required_by)) = missing.into_iter().next() {
        return Err(WireError::MissingProvider {
            port: port.to_string(),
            required_by,
        });
    }

    // dependency graph: requirer depends on provider. kahn's algorithm with
    // a sorted ready-set gives providers-first AND deterministic order; a
    // leftover node means a cycle, which a dfs then writes out by name.
    let mut unresolved: BTreeMap<&str, usize> = slugs.iter().map(|s| (*s, 0)).collect();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for e in &edges {
        *unresolved.entry(e.requirer.as_str()).or_default() += 1;
        dependents
            .entry(e.provider.as_str())
            .or_default()
            .push(e.requirer.as_str());
    }
    let mut ready: BTreeSet<&str> = unresolved
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(s, _)| *s)
        .collect();
    let mut order: Vec<String> = Vec::with_capacity(slugs.len());
    while let Some(next) = ready.iter().next().copied() {
        ready.remove(next);
        order.push(next.to_string());
        if let Some(ds) = dependents.get(next) {
            for d in ds {
                let n = unresolved.get_mut(d).expect("every requirer is a known slug");
                *n -= 1;
                if *n == 0 {
                    ready.insert(d);
                }
            }
        }
    }
    if order.len() < slugs.len() {
        let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for e in &edges {
            adjacency
                .entry(e.requirer.as_str())
                .or_default()
                .push(e.provider.as_str());
        }
        let path = find_cycle(&slugs, &adjacency).unwrap_or_default();
        return Err(WireError::Cycle(path));
    }

    Ok(Wiring {
        providers,
        edges,
        order,
    })
}

/// one cycle in `adjacency` (requirer → providers), as a closed path of
/// slugs. iterative dfs with three colors; the first back edge found from
/// the smallest start slug wins, so the report is deterministic.
fn find_cycle(
    nodes: &BTreeSet<&str>,
    adjacency: &BTreeMap<&str, Vec<&str>>,
) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color: BTreeMap<&str, Color> = nodes.iter().map(|n| (*n, Color::White)).collect();
    for &start in nodes {
        if color[start] != Color::White {
            continue;
        }
        // stack of (node, next child index); `path` mirrors the gray chain.
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        let mut path: Vec<&str> = vec![start];
        color.insert(start, Color::Gray);
        while let Some((node, idx)) = stack.last_mut() {
            let children = adjacency.get(node).map(|v| v.as_slice()).unwrap_or(&[]);
            if *idx < children.len() {
                let child = children[*idx];
                *idx += 1;
                match color.get(child).copied().unwrap_or(Color::Black) {
                    Color::Gray => {
                        let from = path.iter().position(|p| *p == child).unwrap_or(0);
                        let mut cycle: Vec<String> =
                            path[from..].iter().map(|s| s.to_string()).collect();
                        cycle.push(child.to_string());
                        return Some(cycle);
                    }
                    Color::White => {
                        color.insert(child, Color::Gray);
                        stack.push((child, 0));
                        path.push(child);
                    }
                    Color::Black => {}
                }
            } else {
                color.insert(node, Color::Black);
                stack.pop();
                path.pop();
            }
        }
    }
    None
}
