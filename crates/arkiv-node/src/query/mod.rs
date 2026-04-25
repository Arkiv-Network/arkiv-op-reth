//! Arkiv query language — parser, AST, and evaluator.
//!
//! The query language is the one produced by the arkiv-sdk-js
//! `processPredicates` function in `src/query/engine.ts`:
//!
//! ```text
//! name = "John" && age >= 30 && (priority = 5 || priority = 10)
//! !deprecated && $owner = 0xabc... && $key = 0x123...
//! ```
//!
//! Special selectors: `$key`, `$owner`, `$creator` match entity metadata
//! rather than attributes.

pub mod parser;

use alloy_primitives::{Address, B256};

use crate::storage::entity::EntityRecord;

/// Special selectors that match metadata fields rather than attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// `$key` — matches the entity's own key (B256).
    Key,
    /// `$owner` — matches the entity's current owner address.
    Owner,
    /// `$creator` — matches the entity's creator address.
    Creator,
    /// A user-defined attribute name.
    Attr(String),
}

/// A literal value in a query predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    String(String),
    Number(u64),
    /// Hex literal (e.g. `0xabc...`). Only valid for `$key`, `$owner`, `$creator`.
    Hex(Vec<u8>),
}

/// Comparison operator between a [`Selector`] and a [`Value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}

/// Parsed query expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// `selector OP value`
    Cmp { selector: Selector, op: CmpOp, value: Value },
    /// `!attr` — matches when the attribute is NOT present on the entity.
    Not(Selector),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    /// Empty query (matches everything).
    True,
}

impl Expr {
    /// Evaluate the expression against the given entity record.
    pub fn matches(&self, entity: &EntityRecord) -> bool {
        match self {
            Expr::True => true,
            Expr::And(parts) => parts.iter().all(|p| p.matches(entity)),
            Expr::Or(parts) => parts.iter().any(|p| p.matches(entity)),
            Expr::Not(sel) => match sel {
                Selector::Attr(name) => !entity.has_attribute(name),
                // `!$key` etc. are not meaningful — treat as false.
                _ => false,
            },
            Expr::Cmp { selector, op, value } => eval_cmp(entity, selector, *op, value),
        }
    }
}

fn eval_cmp(entity: &EntityRecord, selector: &Selector, op: CmpOp, value: &Value) -> bool {
    match selector {
        Selector::Key => {
            let bytes = match value {
                Value::Hex(b) => b.as_slice(),
                _ => return false,
            };
            if bytes.len() != 32 {
                return false;
            }
            let key = B256::from_slice(bytes);
            cmp_bytes(entity.key.as_slice(), key.as_slice(), op)
        }
        Selector::Owner => match value {
            Value::Hex(b) if b.len() == 20 => {
                cmp_bytes(entity.owner.as_slice(), b.as_slice(), op)
            }
            _ => false,
        },
        Selector::Creator => match value {
            Value::Hex(b) if b.len() == 20 => {
                cmp_bytes(entity.creator.as_slice(), b.as_slice(), op)
            }
            _ => false,
        },
        Selector::Attr(name) => match value {
            Value::String(s) => match entity.get_string(name) {
                Some(actual) => cmp_str(actual, s, op),
                None => false,
            },
            Value::Number(n) => match entity.get_numeric(name) {
                Some(actual) => cmp_u64(actual, *n, op),
                None => false,
            },
            Value::Hex(_) => false,
        },
    }
}

fn cmp_bytes(a: &[u8], b: &[u8], op: CmpOp) -> bool {
    apply_op(a.cmp(b), op)
}

fn cmp_str(a: &str, b: &str, op: CmpOp) -> bool {
    apply_op(a.cmp(b), op)
}

fn cmp_u64(a: u64, b: u64, op: CmpOp) -> bool {
    apply_op(a.cmp(&b), op)
}

fn apply_op(ord: std::cmp::Ordering, op: CmpOp) -> bool {
    use std::cmp::Ordering::*;
    match op {
        CmpOp::Eq => ord == Equal,
        CmpOp::Neq => ord != Equal,
        CmpOp::Lt => ord == Less,
        CmpOp::Lte => ord != Greater,
        CmpOp::Gt => ord == Greater,
        CmpOp::Gte => ord != Less,
    }
}

/// Walk the expression and collect a top-level conjunctive `$key = X` constraint.
/// Used by the query planner to short-circuit lookups by key.
pub fn extract_key_constraint(expr: &Expr) -> Option<B256> {
    match expr {
        Expr::Cmp {
            selector: Selector::Key,
            op: CmpOp::Eq,
            value: Value::Hex(b),
        } if b.len() == 32 => Some(B256::from_slice(b)),
        Expr::And(parts) => parts.iter().find_map(extract_key_constraint),
        _ => None,
    }
}

/// Walk the expression and collect a top-level conjunctive `$owner = X` constraint.
pub fn extract_owner_constraint(expr: &Expr) -> Option<Address> {
    match expr {
        Expr::Cmp {
            selector: Selector::Owner,
            op: CmpOp::Eq,
            value: Value::Hex(b),
        } if b.len() == 20 => Some(Address::from_slice(b)),
        Expr::And(parts) => parts.iter().find_map(extract_owner_constraint),
        _ => None,
    }
}

/// Walk the expression and collect a top-level conjunctive `$creator = X` constraint.
pub fn extract_creator_constraint(expr: &Expr) -> Option<Address> {
    match expr {
        Expr::Cmp {
            selector: Selector::Creator,
            op: CmpOp::Eq,
            value: Value::Hex(b),
        } if b.len() == 20 => Some(Address::from_slice(b)),
        Expr::And(parts) => parts.iter().find_map(extract_creator_constraint),
        _ => None,
    }
}
