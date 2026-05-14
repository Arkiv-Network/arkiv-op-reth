//! Tree-walking evaluator for the parsed query AST.
//!
//! `Query::evaluate` recursively turns a [`Query`] into a [`Bitmap`] of
//! matching entity IDs by issuing point reads against a
//! [`StateAdapter`]. No normalization pass — `Not` is evaluated as
//! `$all \ eval(inner)`, which means each `Not` (or `!=` / `NOT IN`)
//! costs one extra `$all` read. Acceptable for the small queries we
//! expect; can revisit if profiling says otherwise.
//!
//! Integration tests live in `crates/arkiv-entitydb/tests/query_eval.rs`.

use eyre::Result;

use super::parser::{AnnotKey, AnnotVal, Query};
use crate::{Bitmap, StateAdapter, all_entities, read_pair_bitmap};

impl Query {
    /// Evaluate the AST against `state`, returning the bitmap of
    /// matching entity IDs.
    pub fn evaluate<S: StateAdapter>(&self, state: &mut S) -> Result<Bitmap> {
        eval(self, state)
    }
}

fn eval<S: StateAdapter>(query: &Query, state: &mut S) -> Result<Bitmap> {
    match query {
        Query::All => all_entities(state),

        Query::Eq { key, value } => read_eq(state, key, value),
        Query::Neq { key, value } => {
            let mut all = all_entities(state)?;
            let hit = read_eq(state, key, value)?;
            all.subtract(&hit);
            Ok(all)
        }

        Query::In { key, values } => read_in(state, key, values),
        Query::NotIn { key, values } => {
            let mut all = all_entities(state)?;
            let hit = read_in(state, key, values)?;
            all.subtract(&hit);
            Ok(all)
        }

        Query::And(left, right) => {
            let mut l = eval(left, state)?;
            // Short-circuit: AND with empty is empty, no need to load
            // the right side.
            if l.is_empty() {
                return Ok(l);
            }
            let r = eval(right, state)?;
            l.intersect_with(&r);
            Ok(l)
        }
        Query::Or(left, right) => {
            let mut l = eval(left, state)?;
            let r = eval(right, state)?;
            l.union_with(&r);
            Ok(l)
        }
        Query::Not(inner) => {
            let mut all = all_entities(state)?;
            let hit = eval(inner, state)?;
            all.subtract(&hit);
            Ok(all)
        }
    }
}

fn read_eq<S: StateAdapter>(state: &mut S, key: &AnnotKey, value: &AnnotVal) -> Result<Bitmap> {
    read_pair_bitmap(state, key.pair_key_bytes(), &value.0)
}

/// OR-union of the bitmaps for each value in an `IN (...)` list.
fn read_in<S: StateAdapter>(
    state: &mut S,
    key: &AnnotKey,
    values: &[AnnotVal],
) -> Result<Bitmap> {
    let mut acc = Bitmap::new();
    for v in values {
        let bm = read_pair_bitmap(state, key.pair_key_bytes(), &v.0)?;
        acc.union_with(&bm);
    }
    Ok(acc)
}
