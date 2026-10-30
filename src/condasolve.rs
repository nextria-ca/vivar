#![allow(dead_code)]
//! Solving conda match specs against a channel: newest first, one build per
//! package, chronological backtracking. Virtual packages describe the
//! machine; `constrains` entries bound packages that happen to be present.

use crate::condaspec::{MatchSpec, Record};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Where candidates come from: a channel, or a fixed set in tests.
pub trait Provider {
    /// Every record of `name`, newest first.
    fn candidates(&self, name: &str) -> Result<Vec<Record>>;
}

impl Provider for crate::condaindex::Channel {
    fn candidates(&self, name: &str) -> Result<Vec<Record>> {
        self.records(name)
    }
}

/// Several channels, first match wins (a name found in one is never looked
/// up in the next: channel priority, as conda's strict mode).
pub struct Channels<'a>(pub Vec<&'a dyn Provider>);

impl Provider for Channels<'_> {
    fn candidates(&self, name: &str) -> Result<Vec<Record>> {
        for c in &self.0 {
            let r = c.candidates(name)?;
            if !r.is_empty() {
                return Ok(r);
            }
        }
        Ok(Vec::new())
    }
}

#[derive(Clone)]
struct State {
    assigned: Vec<Record>,
    specs: BTreeMap<String, Vec<MatchSpec>>,
    pending: VecDeque<(String, MatchSpec)>,
}

struct Frame {
    name: String,
    candidates: Vec<Record>,
    next: usize,
    before: State,
}

pub struct Solver<'a> {
    provider: &'a dyn Provider,
    virtuals: Vec<Record>,
    /// What the current lock has: kept when still acceptable.
    prefer: BTreeMap<String, (String, String)>,
    upgrade_all: bool,
    cache: BTreeMap<String, Vec<Record>>,
}

impl<'a> Solver<'a> {
    pub fn new(provider: &'a dyn Provider, virtuals: Vec<Record>) -> Solver<'a> {
        Solver {
            provider,
            virtuals,
            prefer: BTreeMap::new(),
            upgrade_all: false,
            cache: BTreeMap::new(),
        }
    }

    /// Versions and builds to keep (the current lock) unless `upgrade`.
    pub fn prefer(mut self, locked: BTreeMap<String, (String, String)>, upgrade: bool) -> Self {
        self.prefer = locked;
        self.upgrade_all = upgrade;
        self
    }

    fn all_candidates(&mut self, name: &str) -> Result<Vec<Record>> {
        if name.starts_with("__") {
            return Ok(self
                .virtuals
                .iter()
                .filter(|r| r.name == name)
                .cloned()
                .collect());
        }
        if let Some(c) = self.cache.get(name) {
            return Ok(c.clone());
        }
        let mut recs = self.provider.candidates(name)?;
        recs.sort_by_key(|r| std::cmp::Reverse(r.sort_key()));
        if !self.upgrade_all {
            if let Some((v, b)) = self.prefer.get(name) {
                if let Some(i) = recs.iter().position(|r| r.version == *v && r.build == *b) {
                    let r = recs.remove(i);
                    recs.insert(0, r);
                }
            }
        }
        self.cache.insert(name.to_owned(), recs.clone());
        Ok(recs)
    }

    /// The `constrains` entries of assigned packages that name `name`.
    fn constraints_on(state: &State, name: &str) -> Vec<MatchSpec> {
        let mut out = Vec::new();
        for r in &state.assigned {
            for c in &r.constrains {
                if let Ok(spec) = MatchSpec::parse(c) {
                    if spec.name == name {
                        out.push(spec);
                    }
                }
            }
        }
        out
    }

    /// Candidates of `name` acceptable to every spec and constraint in force.
    fn acceptable(&mut self, state: &State, name: &str) -> Result<Vec<Record>> {
        let specs = state.specs.get(name).cloned().unwrap_or_default();
        let constraints = Self::constraints_on(state, name);
        let all = self.all_candidates(name)?;
        Ok(all
            .into_iter()
            .filter(|r| specs.iter().all(|s| s.matches(r)))
            .filter(|r| constraints.iter().all(|c| c.matches(r)))
            // A record's own constrains must hold for what is already chosen.
            .filter(|r| {
                r.constrains.iter().all(|c| match MatchSpec::parse(c) {
                    Ok(spec) => state
                        .assigned
                        .iter()
                        .find(|a| a.name == spec.name)
                        .is_none_or(|a| spec.matches(a)),
                    Err(_) => true,
                })
            })
            .collect())
    }

    pub fn solve(mut self, roots: &[MatchSpec]) -> Result<Vec<Record>> {
        let mut state = State {
            assigned: Vec::new(),
            specs: BTreeMap::new(),
            pending: VecDeque::new(),
        };
        for r in roots {
            state
                .pending
                .push_back(("the project".to_owned(), r.clone()));
        }
        let mut frames: Vec<Frame> = Vec::new();
        let mut steps = 0usize;
        loop {
            steps += 1;
            if steps > 200_000 {
                bail!("the native solve did not converge after {steps} steps; the specs may be contradictory");
            }
            let Some((from, spec)) = state.pending.pop_front() else {
                break;
            };
            state
                .specs
                .entry(spec.name.clone())
                .or_default()
                .push(spec.clone());
            if let Some(current) = state.assigned.iter().find(|r| r.name == spec.name) {
                if spec.matches(current) {
                    continue;
                }
                // The chosen build no longer fits: try the next one.
                state = self.backtrack(&mut frames, &spec.name, &from, &spec)?;
                continue;
            }
            let candidates = self.acceptable(&state, &spec.name)?;
            if candidates.is_empty() {
                // Nothing of that name at all: no other choice would help.
                if frames.is_empty() || self.all_candidates(&spec.name)?.is_empty() {
                    bail!("{}", self.explain(&state, &spec.name, &from, &spec)?);
                }
                state = self.backtrack(&mut frames, &spec.name, &from, &spec)?;
                continue;
            }
            let chosen = candidates[0].clone();
            frames.push(Frame {
                name: spec.name.clone(),
                candidates,
                next: 1,
                before: state.clone(),
            });
            self.assign(&mut state, chosen)?;
        }
        // Virtual packages are the machine's, not the environment's.
        Ok(state
            .assigned
            .into_iter()
            .filter(|r| !r.name.starts_with("__"))
            .collect())
    }

    fn assign(&mut self, state: &mut State, record: Record) -> Result<()> {
        for d in &record.depends {
            let spec = MatchSpec::parse(d)
                .with_context(|| format!("{} {}: dependency {d:?}", record.name, record.version))?;
            state
                .pending
                .push_back((format!("{} {}", record.name, record.version), spec));
        }
        state.assigned.push(record);
        Ok(())
    }

    fn backtrack(
        &mut self,
        frames: &mut Vec<Frame>,
        culprit: &str,
        from: &str,
        spec: &MatchSpec,
    ) -> Result<State> {
        loop {
            let Some(frame) = frames.last_mut() else {
                bail!("no build of {culprit} satisfies every requirement; {from} needs `{spec}`");
            };
            if frame.next >= frame.candidates.len() {
                frames.pop();
                continue;
            }
            let record = frame.candidates[frame.next].clone();
            frame.next += 1;
            let mut state = frame.before.clone();
            let name = frame.name.clone();
            // Only candidates still acceptable to the specs known then.
            let specs = state.specs.get(&name).cloned().unwrap_or_default();
            if !specs.iter().all(|s| s.matches(&record)) {
                continue;
            }
            self.assign(&mut state, record)?;
            return Ok(state);
        }
    }

    /// Why nothing fits `name`: every spec on it, or that it does not exist.
    fn explain(
        &mut self,
        state: &State,
        name: &str,
        from: &str,
        spec: &MatchSpec,
    ) -> Result<String> {
        let all = self.all_candidates(name)?;
        if all.is_empty() {
            if name.starts_with("__") {
                let hint = match name {
                    "__cuda" => {
                        " (no NVIDIA driver found here; CONDA_OVERRIDE_CUDA=12.4 says otherwise)"
                    }
                    "__glibc" => " (CONDA_OVERRIDE_GLIBC=2.28 sets it for another machine)",
                    _ => "",
                };
                return Ok(format!(
                    "{from} needs `{spec}`, a virtual package this machine does not provide{hint}"
                ));
            }
            return Ok(format!(
                "{from} needs `{spec}`, but the channel has no package named {name}"
            ));
        }
        let mut wants: BTreeSet<String> = state
            .specs
            .get(name)
            .map(|s| s.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        wants.insert(spec.to_string());
        let newest = all.first().map(|r| r.version.clone()).unwrap_or_default();
        Ok(format!(
            "no build of {name} satisfies {} (newest available: {newest}); {from} needs `{spec}`",
            wants.into_iter().collect::<Vec<_>>().join(" and ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::condaindex::virtual_record;

    struct Fixed(Vec<Record>);

    impl Provider for Fixed {
        fn candidates(&self, name: &str) -> Result<Vec<Record>> {
            Ok(self.0.iter().filter(|r| r.name == name).cloned().collect())
        }
    }

    fn rec(name: &str, version: &str, build: &str, number: u64, depends: &[&str]) -> Record {
        let mut r = virtual_record(name, version, build, "linux-64");
        r.build_number = number;
        r.depends = depends.iter().map(|d| d.to_string()).collect();
        r.filename = format!("{name}-{version}-{build}.conda");
        r
    }

    fn solve(provider: &Fixed, roots: &[&str]) -> Result<Vec<String>> {
        let virtuals = vec![virtual_record("__glibc", "2.35", "0", "linux-64")];
        let roots: Vec<MatchSpec> = roots.iter().map(|r| MatchSpec::parse(r).unwrap()).collect();
        let mut out: Vec<String> = Solver::new(provider, virtuals)
            .solve(&roots)?
            .iter()
            .map(|r| format!("{}={}={}", r.name, r.version, r.build))
            .collect();
        out.sort();
        Ok(out)
    }

    #[test]
    fn picks_newest_builds_and_backtracks() {
        let provider = Fixed(vec![
            rec(
                "python",
                "3.12.7",
                "h1_0",
                0,
                &["libz >=1.3", "__glibc >=2.17"],
            ),
            rec(
                "python",
                "3.13.0",
                "h1_0",
                0,
                &["libz >=1.3", "__glibc >=2.17"],
            ),
            rec(
                "python",
                "3.13.0",
                "h1_1",
                1,
                &["libz >=1.3", "__glibc >=2.17"],
            ),
            rec("libz", "1.3.1", "h0_0", 0, &[]),
            rec("libz", "1.2.13", "h0_0", 0, &[]),
            rec(
                "numpy",
                "2.1.0",
                "py313_0",
                0,
                &["python 3.13.*", "libz <1.3"],
            ),
            rec(
                "numpy",
                "2.0.0",
                "py313_0",
                0,
                &["python 3.13.*", "libz >=1.3"],
            ),
            rec(
                "numpy",
                "2.0.0",
                "py312_0",
                0,
                &["python 3.12.*", "libz >=1.3"],
            ),
            rec("old", "1.0", "0", 0, &["python 3.12.*"]),
            rec("needs-cuda", "1.0", "0", 0, &["__cuda >=12"]),
        ]);
        // Newest python, highest build number; numpy 2.1 wants an old libz
        // that python refuses, so numpy steps back to 2.0.
        assert_eq!(
            solve(&provider, &["numpy"]).unwrap(),
            vec![
                "libz=1.3.1=h0_0",
                "numpy=2.0.0=py313_0",
                "python=3.13.0=h1_1"
            ]
        );
        // A later spec on python forces the earlier choice back.
        assert_eq!(
            solve(&provider, &["numpy", "old"]).unwrap(),
            vec![
                "libz=1.3.1=h0_0",
                "numpy=2.0.0=py312_0",
                "old=1.0=0",
                "python=3.12.7=h1_0"
            ]
        );
        let err = solve(&provider, &["needs-cuda"]).unwrap_err().to_string();
        assert!(
            err.contains("__cuda") && err.contains("virtual package"),
            "{err}"
        );
        let err = solve(&provider, &["nothing"]).unwrap_err().to_string();
        assert!(err.contains("no package named nothing"), "{err}");
        let err = solve(&provider, &["python >=4"]).unwrap_err().to_string();
        assert!(err.contains("newest available: 3.13.0"), "{err}");
    }

    #[test]
    fn constrains_bound_what_is_present_and_the_lock_is_preferred() {
        let mut mkl = rec("libblas", "3.9.0", "mkl", 0, &[]);
        mkl.constrains = vec!["liblapack 3.9.0 mkl".into()];
        let provider = Fixed(vec![
            rec("libblas", "3.9.0", "netlib", 1, &[]),
            mkl,
            rec("liblapack", "3.9.0", "netlib", 0, &["libblas 3.9.0 netlib"]),
            rec("liblapack", "3.9.0", "mkl", 0, &["libblas 3.9.0 mkl"]),
            rec("app", "1.0", "0", 0, &["liblapack", "libblas"]),
        ]);
        let out = solve(&provider, &["app"]).unwrap();
        assert!(
            out.contains(&"libblas=3.9.0=netlib".to_owned())
                && out.contains(&"liblapack=3.9.0=netlib".to_owned()),
            "{out:?}"
        );
        // With the mkl pair locked, the solve keeps it.
        let roots = vec![MatchSpec::parse("app").unwrap()];
        let locked = BTreeMap::from([
            ("libblas".to_owned(), ("3.9.0".to_owned(), "mkl".to_owned())),
            (
                "liblapack".to_owned(),
                ("3.9.0".to_owned(), "mkl".to_owned()),
            ),
        ]);
        let kept: Vec<String> = Solver::new(&provider, vec![])
            .prefer(locked, false)
            .solve(&roots)
            .unwrap()
            .iter()
            .map(|r| format!("{}={}", r.name, r.build))
            .collect();
        assert!(
            kept.contains(&"libblas=mkl".to_owned()) && kept.contains(&"liblapack=mkl".to_owned()),
            "{kept:?}"
        );
    }
}
