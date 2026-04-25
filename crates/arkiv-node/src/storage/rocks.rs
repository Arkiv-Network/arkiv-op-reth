//! RocksDB-backed Arkiv entity storage and query implementation.

use crate::storage::{
    Annotation, ArkivBlock, ArkivBlockRef, ArkivOperation, CreateOp, Storage, UpdateOp,
};
use alloy_primitives::{Address, B256, Bytes};
use eyre::{Context, Result, bail};
use rocksdb::{DB, Options};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::Path;

const ENTITY_PREFIX: &[u8] = b"entity:";
const BLOCK_PREFIX: &[u8] = b"block:";
const TIP_KEY: &[u8] = b"tip";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AttributeValue {
    String(String),
    Numeric(u64),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Attribute {
    pub key: String,
    pub value: AttributeValue,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityState {
    pub key: B256,
    pub payload: Bytes,
    pub content_type: String,
    pub expires_at: u64,
    pub owner: Address,
    pub creator: Address,
    pub created_at_block: u64,
    pub last_modified_at_block: u64,
    pub transaction_index_in_block: u32,
    pub operation_index_in_transaction: u32,
    pub attributes: Vec<Attribute>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Undo {
    key: B256,
    previous: Option<EntityState>,
}

pub struct RockDbStore {
    db: DB,
}

impl RockDbStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        Ok(Self {
            db: DB::open(&opts, path).wrap_err("failed to open Arkiv RocksDB store")?,
        })
    }

    #[cfg(test)]
    fn temporary() -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "arkiv-rocks-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        Self::open(path)
    }

    pub fn query(&self, query: &str, options: QueryOptions) -> Result<QueryResponse> {
        let predicate = Parser::new(query).parse()?;
        let at_block = options.at_block.as_deref().map(parse_hex_u64).transpose()?;
        let tip = self.tip()?;
        let block_number = at_block.unwrap_or(tip);
        let include = options.include_data.unwrap_or_default();
        let mut entities = self.all_entities()?;

        entities.retain(|entity| entity.created_at_block <= block_number && predicate.eval(entity));
        apply_ordering(&mut entities, options.order_by.as_deref());

        let start = options
            .cursor
            .as_deref()
            .map(parse_cursor)
            .transpose()?
            .unwrap_or(0);
        let limit = options
            .results_per_page
            .as_deref()
            .map(parse_hex_u64)
            .transpose()?
            .map(|n| n as usize)
            .unwrap_or(entities.len().saturating_sub(start));

        let end = start.saturating_add(limit).min(entities.len());
        let cursor = (end < entities.len()).then(|| format!("0x{end:x}"));
        let data = entities
            .into_iter()
            .skip(start)
            .take(end.saturating_sub(start))
            .map(|entity| RpcEntity::from_state(entity, &include))
            .collect();

        Ok(QueryResponse {
            data,
            block_number: format!("0x{block_number:x}"),
            cursor,
        })
    }

    pub fn entity_count(&self) -> Result<usize> {
        Ok(self.all_entities()?.len())
    }

    fn apply_block(&self, block: &ArkivBlock) -> Result<()> {
        let mut undo = Vec::new();

        for tx in &block.transactions {
            for op in &tx.operations {
                let key = op.entity_key();
                let previous = self.get_entity(key)?;
                undo.push(Undo { key, previous });

                match op {
                    ArkivOperation::Create(create) => {
                        self.put_entity(&entity_from_create(
                            create,
                            tx.sender,
                            block.header.number,
                            tx.index,
                        ))?;
                    }
                    ArkivOperation::Update(update) => {
                        let mut entity = self.get_entity(update.entity_key)?.unwrap_or_else(|| {
                            entity_from_update(update, tx.sender, block.header.number, tx.index)
                        });
                        entity.payload = update.payload.clone();
                        entity.content_type = update.content_type.clone();
                        entity.owner = update.owner;
                        entity.last_modified_at_block = block.header.number;
                        entity.transaction_index_in_block = tx.index;
                        entity.operation_index_in_transaction = update.op_index;
                        entity.attributes = attributes_from_annotations(&update.annotations);
                        self.put_entity(&entity)?;
                    }
                    ArkivOperation::Extend(extend) => {
                        if let Some(mut entity) = self.get_entity(extend.entity_key)? {
                            entity.expires_at = extend.expires_at;
                            entity.last_modified_at_block = block.header.number;
                            entity.transaction_index_in_block = tx.index;
                            entity.operation_index_in_transaction = extend.op_index;
                            self.put_entity(&entity)?;
                        }
                    }
                    ArkivOperation::ChangeOwner(change) => {
                        if let Some(mut entity) = self.get_entity(change.entity_key)? {
                            entity.owner = change.owner;
                            entity.last_modified_at_block = block.header.number;
                            entity.transaction_index_in_block = tx.index;
                            entity.operation_index_in_transaction = change.op_index;
                            self.put_entity(&entity)?;
                        }
                    }
                    ArkivOperation::Delete(delete) => self.delete_entity(delete.entity_key)?,
                    ArkivOperation::Expire(expire) => self.delete_entity(expire.entity_key)?,
                }
            }
        }

        self.db
            .put(block_key(block.header.number), serde_json::to_vec(&undo)?)
            .wrap_err("failed to store block undo log")?;
        self.db
            .put(TIP_KEY, block.header.number.to_be_bytes())
            .wrap_err("failed to store Arkiv tip")?;
        Ok(())
    }

    fn revert_block(&self, block: &ArkivBlockRef) -> Result<()> {
        let Some(raw) = self.db.get(block_key(block.number))? else {
            return Ok(());
        };
        let mut undo: Vec<Undo> = serde_json::from_slice(&raw)?;
        undo.reverse();
        for op in undo {
            match op.previous {
                Some(entity) => self.put_entity(&entity)?,
                None => self.delete_entity(op.key)?,
            }
        }
        self.db.delete(block_key(block.number))?;
        self.db
            .put(TIP_KEY, block.number.saturating_sub(1).to_be_bytes())?;
        Ok(())
    }

    fn tip(&self) -> Result<u64> {
        Ok(self
            .db
            .get(TIP_KEY)?
            .and_then(|raw| raw.as_slice().try_into().ok().map(u64::from_be_bytes))
            .unwrap_or_default())
    }

    fn all_entities(&self) -> Result<Vec<EntityState>> {
        let mut entities = Vec::new();
        for item in self.db.prefix_iterator(ENTITY_PREFIX) {
            let (_, value) = item?;
            if !value.starts_with(b"{") {
                continue;
            }
            entities.push(serde_json::from_slice(&value)?);
        }
        Ok(entities)
    }

    fn get_entity(&self, key: B256) -> Result<Option<EntityState>> {
        self.db
            .get(entity_key(key))?
            .map(|raw| serde_json::from_slice(&raw).wrap_err("failed to decode entity"))
            .transpose()
    }

    fn put_entity(&self, entity: &EntityState) -> Result<()> {
        self.db
            .put(entity_key(entity.key), serde_json::to_vec(entity)?)?;
        Ok(())
    }

    fn delete_entity(&self, key: B256) -> Result<()> {
        self.db.delete(entity_key(key))?;
        Ok(())
    }
}

impl Storage for RockDbStore {
    fn handle_commit(&self, blocks: &[ArkivBlock]) -> Result<Option<B256>> {
        for block in blocks {
            self.apply_block(block)?;
        }
        Ok(None)
    }

    fn handle_revert(&self, blocks: &[ArkivBlockRef]) -> Result<Option<B256>> {
        for block in blocks {
            self.revert_block(block)?;
        }
        Ok(None)
    }

    fn handle_reorg(
        &self,
        reverted: &[ArkivBlockRef],
        new_blocks: &[ArkivBlock],
    ) -> Result<Option<B256>> {
        self.handle_revert(reverted)?;
        self.handle_commit(new_blocks)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryOptions {
    pub at_block: Option<String>,
    pub include_data: Option<IncludeData>,
    pub order_by: Option<Vec<OrderBy>>,
    pub results_per_page: Option<String>,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IncludeData {
    #[serde(default = "default_true")]
    pub key: bool,
    #[serde(default)]
    pub attributes: bool,
    #[serde(default)]
    pub payload: bool,
    #[serde(default)]
    pub content_type: bool,
    #[serde(default, alias = "expiration")]
    pub expires_at: bool,
    #[serde(default)]
    pub owner: bool,
    #[serde(default)]
    pub creator: bool,
    #[serde(default)]
    pub created_at_block: bool,
    #[serde(default)]
    pub last_modified_at_block: bool,
    #[serde(default)]
    pub transaction_index_in_block: bool,
    #[serde(default)]
    pub operation_index_in_transaction: bool,
}

impl Default for IncludeData {
    fn default() -> Self {
        Self {
            key: true,
            attributes: false,
            payload: false,
            content_type: false,
            expires_at: false,
            owner: false,
            creator: false,
            created_at_block: false,
            last_modified_at_block: false,
            transaction_index_in_block: false,
            operation_index_in_transaction: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderBy {
    pub name: String,
    #[serde(rename = "type")]
    pub attribute_type: AttributeType,
    pub desc: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum AttributeType {
    String,
    Numeric,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResponse {
    pub data: Vec<RpcEntity>,
    pub block_number: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcEntity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<B256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Bytes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at_block: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified_at_block: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_index_in_block: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_index_in_transaction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub string_attributes: Option<Vec<StringAttribute>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub numeric_attributes: Option<Vec<NumericAttribute>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StringAttribute {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct NumericAttribute {
    pub key: String,
    pub value: String,
}

impl RpcEntity {
    fn from_state(entity: EntityState, include: &IncludeData) -> Self {
        let (string_attributes, numeric_attributes) = if include.attributes {
            let strings = entity
                .attributes
                .iter()
                .filter_map(|attr| match &attr.value {
                    AttributeValue::String(value) => Some(StringAttribute {
                        key: attr.key.clone(),
                        value: value.clone(),
                    }),
                    AttributeValue::Numeric(_) => None,
                })
                .collect();
            let numerics = entity
                .attributes
                .iter()
                .filter_map(|attr| match attr.value {
                    AttributeValue::String(_) => None,
                    AttributeValue::Numeric(value) => Some(NumericAttribute {
                        key: attr.key.clone(),
                        value: format!("0x{value:x}"),
                    }),
                })
                .collect();
            (Some(strings), Some(numerics))
        } else {
            (None, None)
        };

        Self {
            key: include.key.then_some(entity.key),
            content_type: include.content_type.then_some(entity.content_type),
            value: include.payload.then_some(entity.payload),
            expires_at: include
                .expires_at
                .then(|| format!("0x{:x}", entity.expires_at)),
            created_at_block: include
                .created_at_block
                .then(|| format!("0x{:x}", entity.created_at_block)),
            last_modified_at_block: include
                .last_modified_at_block
                .then(|| format!("0x{:x}", entity.last_modified_at_block)),
            transaction_index_in_block: include
                .transaction_index_in_block
                .then(|| format!("0x{:x}", entity.transaction_index_in_block)),
            operation_index_in_transaction: include
                .operation_index_in_transaction
                .then(|| format!("0x{:x}", entity.operation_index_in_transaction)),
            owner: include.owner.then_some(entity.owner),
            creator: include.creator.then_some(entity.creator),
            string_attributes,
            numeric_attributes,
        }
    }
}

fn default_true() -> bool {
    true
}

fn entity_from_create(
    op: &CreateOp,
    sender: Address,
    block_number: u64,
    transaction_index: u32,
) -> EntityState {
    EntityState {
        key: op.entity_key,
        payload: op.payload.clone(),
        content_type: op.content_type.clone(),
        expires_at: op.expires_at,
        owner: op.owner,
        creator: sender,
        created_at_block: block_number,
        last_modified_at_block: block_number,
        transaction_index_in_block: transaction_index,
        operation_index_in_transaction: op.op_index,
        attributes: attributes_from_annotations(&op.annotations),
    }
}

fn entity_from_update(
    op: &UpdateOp,
    sender: Address,
    block_number: u64,
    transaction_index: u32,
) -> EntityState {
    EntityState {
        key: op.entity_key,
        payload: op.payload.clone(),
        content_type: op.content_type.clone(),
        expires_at: 0,
        owner: op.owner,
        creator: sender,
        created_at_block: block_number,
        last_modified_at_block: block_number,
        transaction_index_in_block: transaction_index,
        operation_index_in_transaction: op.op_index,
        attributes: attributes_from_annotations(&op.annotations),
    }
}

fn attributes_from_annotations(annotations: &[Annotation]) -> Vec<Attribute> {
    annotations
        .iter()
        .map(|annotation| match annotation {
            Annotation::String { key, string_value } => Attribute {
                key: key.clone(),
                value: AttributeValue::String(string_value.clone()),
            },
            Annotation::Numeric { key, numeric_value } => Attribute {
                key: key.clone(),
                value: AttributeValue::Numeric(*numeric_value),
            },
        })
        .collect()
}

trait OperationKey {
    fn entity_key(&self) -> B256;
}

impl OperationKey for ArkivOperation {
    fn entity_key(&self) -> B256 {
        match self {
            ArkivOperation::Create(op) => op.entity_key,
            ArkivOperation::Update(op) => op.entity_key,
            ArkivOperation::Extend(op) => op.entity_key,
            ArkivOperation::ChangeOwner(op) => op.entity_key,
            ArkivOperation::Delete(op) => op.entity_key,
            ArkivOperation::Expire(op) => op.entity_key,
        }
    }
}

fn apply_ordering(entities: &mut [EntityState], order_by: Option<&[OrderBy]>) {
    let Some(order_by) = order_by else {
        entities.sort_by_key(|entity| entity.key);
        return;
    };

    entities.sort_by(|left, right| {
        for order in order_by {
            let ord = compare_attribute(left, right, order);
            if ord != Ordering::Equal {
                return if order.desc { ord.reverse() } else { ord };
            }
        }
        left.key.cmp(&right.key)
    });
}

fn compare_attribute(left: &EntityState, right: &EntityState, order: &OrderBy) -> Ordering {
    let left = left.attribute(&order.name);
    let right = right.attribute(&order.name);
    match (left, right, order.attribute_type) {
        (Some(AttributeValue::String(left)), Some(AttributeValue::String(right)), _) => {
            left.cmp(right)
        }
        (Some(AttributeValue::Numeric(left)), Some(AttributeValue::Numeric(right)), _) => {
            left.cmp(right)
        }
        (Some(_), None, _) => Ordering::Less,
        (None, Some(_), _) => Ordering::Greater,
        _ => Ordering::Equal,
    }
}

impl EntityState {
    fn attribute(&self, key: &str) -> Option<&AttributeValue> {
        self.attributes
            .iter()
            .find(|attribute| attribute.key == key)
            .map(|attribute| &attribute.value)
    }
}

#[derive(Clone, Debug)]
enum Predicate {
    True,
    Compare {
        key: String,
        op: CompareOp,
        value: AttributeValue,
    },
    NotExists(String),
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
}

#[derive(Clone, Copy, Debug)]
enum CompareOp {
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl Predicate {
    fn eval(&self, entity: &EntityState) -> bool {
        match self {
            Predicate::True => true,
            Predicate::Compare { key, op, value } if key == "$owner" => {
                compare_strings(&entity.owner.to_string(), *op, &value_string(value))
            }
            Predicate::Compare { key, op, value } => entity
                .attribute(key)
                .is_some_and(|attr| compare_values(attr, *op, value)),
            Predicate::NotExists(key) => entity.attribute(key).is_none(),
            Predicate::And(predicates) => predicates.iter().all(|predicate| predicate.eval(entity)),
            Predicate::Or(predicates) => predicates.iter().any(|predicate| predicate.eval(entity)),
        }
    }
}

fn compare_values(left: &AttributeValue, op: CompareOp, right: &AttributeValue) -> bool {
    match (left, right) {
        (AttributeValue::String(left), AttributeValue::String(right)) => {
            compare_strings(left, op, right)
        }
        (AttributeValue::Numeric(left), AttributeValue::Numeric(right)) => {
            compare_ord(left, op, right)
        }
        _ => false,
    }
}

fn compare_strings(left: &str, op: CompareOp, right: &str) -> bool {
    compare_ord(&left, op, &right)
}

fn compare_ord<T: Ord>(left: &T, op: CompareOp, right: &T) -> bool {
    match op {
        CompareOp::Eq => left == right,
        CompareOp::Neq => left != right,
        CompareOp::Gt => left > right,
        CompareOp::Gte => left >= right,
        CompareOp::Lt => left < right,
        CompareOp::Lte => left <= right,
    }
}

fn value_string(value: &AttributeValue) -> String {
    match value {
        AttributeValue::String(value) => value.clone(),
        AttributeValue::Numeric(value) => value.to_string(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Token {
    Ident(String),
    String(String),
    Number(u64),
    LParen,
    RParen,
    And,
    Or,
    Not,
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn new(input: &str) -> Self {
        Self {
            tokens: tokenize(input),
            position: 0,
        }
    }

    fn parse(mut self) -> Result<Predicate> {
        if self.tokens.is_empty() {
            return Ok(Predicate::True);
        }
        let predicate = self.parse_or()?;
        if self.peek().is_some() {
            bail!("unexpected token at end of Arkiv query");
        }
        Ok(predicate)
    }

    fn parse_or(&mut self) -> Result<Predicate> {
        let mut predicates = vec![self.parse_and()?];
        while self.consume(&Token::Or) {
            predicates.push(self.parse_and()?);
        }
        Ok(if predicates.len() == 1 {
            predicates.remove(0)
        } else {
            Predicate::Or(predicates)
        })
    }

    fn parse_and(&mut self) -> Result<Predicate> {
        let mut predicates = vec![self.parse_primary()?];
        while self.consume(&Token::And) {
            predicates.push(self.parse_primary()?);
        }
        Ok(if predicates.len() == 1 {
            predicates.remove(0)
        } else {
            Predicate::And(predicates)
        })
    }

    fn parse_primary(&mut self) -> Result<Predicate> {
        match self.next() {
            Some(Token::LParen) => {
                let predicate = self.parse_or()?;
                self.expect(Token::RParen)?;
                Ok(predicate)
            }
            Some(Token::Not) => match self.next() {
                Some(Token::Ident(key)) => Ok(Predicate::NotExists(key)),
                _ => bail!("expected attribute key after !"),
            },
            Some(Token::Ident(key)) => {
                let op = match self.next() {
                    Some(Token::Eq) => CompareOp::Eq,
                    Some(Token::Neq) => CompareOp::Neq,
                    Some(Token::Gt) => CompareOp::Gt,
                    Some(Token::Gte) => CompareOp::Gte,
                    Some(Token::Lt) => CompareOp::Lt,
                    Some(Token::Lte) => CompareOp::Lte,
                    _ => bail!("expected comparison operator after {key}"),
                };
                let value = match self.next() {
                    Some(Token::String(value)) => AttributeValue::String(value),
                    Some(Token::Number(value)) => AttributeValue::Numeric(value),
                    Some(Token::Ident(value)) => AttributeValue::String(value),
                    _ => bail!("expected comparison value for {key}"),
                };
                Ok(Predicate::Compare { key, op, value })
            }
            _ => bail!("expected Arkiv query predicate"),
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.peek()?.clone();
        self.position += 1;
        Some(token)
    }

    fn consume(&mut self, token: &Token) -> bool {
        if self.peek() == Some(token) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: Token) -> Result<()> {
        if self.consume(&token) {
            Ok(())
        } else {
            bail!("unexpected Arkiv query token")
        }
    }
}

fn tokenize(input: &str) -> Vec<Token> {
    let mut chars = input.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(ch) = chars.next() {
        match ch {
            c if c.is_whitespace() => {}
            '(' => tokens.push(Token::LParen),
            ')' => tokens.push(Token::RParen),
            '&' if chars.peek() == Some(&'&') => {
                chars.next();
                tokens.push(Token::And);
            }
            '|' if chars.peek() == Some(&'|') => {
                chars.next();
                tokens.push(Token::Or);
            }
            '!' if chars.peek() == Some(&'=') => {
                chars.next();
                tokens.push(Token::Neq);
            }
            '!' => tokens.push(Token::Not),
            '=' => tokens.push(Token::Eq),
            '>' if chars.peek() == Some(&'=') => {
                chars.next();
                tokens.push(Token::Gte);
            }
            '>' => tokens.push(Token::Gt),
            '<' if chars.peek() == Some(&'=') => {
                chars.next();
                tokens.push(Token::Lte);
            }
            '<' => tokens.push(Token::Lt),
            '"' => {
                let mut value = String::new();
                while let Some(ch) = chars.next() {
                    if ch == '"' {
                        break;
                    }
                    value.push(ch);
                }
                tokens.push(Token::String(value));
            }
            c if c.is_ascii_digit() => {
                let mut value = c.to_string();
                while let Some(next) = chars.peek().copied() {
                    if next.is_ascii_alphanumeric() {
                        value.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if let Ok(number) = value.parse() {
                    tokens.push(Token::Number(number));
                } else {
                    tokens.push(Token::Ident(value));
                }
            }
            c => {
                let mut ident = c.to_string();
                while let Some(next) = chars.peek().copied() {
                    if next.is_whitespace()
                        || matches!(next, '(' | ')' | '=' | '!' | '<' | '>' | '&' | '|')
                    {
                        break;
                    }
                    ident.push(next);
                    chars.next();
                }
                tokens.push(Token::Ident(ident));
            }
        }
    }
    tokens
}

fn entity_key(key: B256) -> Vec<u8> {
    [ENTITY_PREFIX, key.as_slice()].concat()
}

fn block_key(number: u64) -> Vec<u8> {
    [BLOCK_PREFIX, &number.to_be_bytes()].concat()
}

fn parse_hex_u64(value: &str) -> Result<u64> {
    let raw = value.strip_prefix("0x").unwrap_or(value);
    Ok(u64::from_str_radix(raw, 16)?)
}

fn parse_cursor(value: &str) -> Result<usize> {
    parse_hex_u64(value).map(|value| value as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{ArkivBlockHeader, ArkivTransaction, ChangeOwnerOp, DeleteOp, ExtendOp};

    fn key(n: u8) -> B256 {
        B256::with_last_byte(n)
    }

    fn addr(n: u8) -> Address {
        Address::with_last_byte(n)
    }

    fn block(number: u64, operations: Vec<ArkivOperation>) -> ArkivBlock {
        ArkivBlock {
            header: ArkivBlockHeader {
                number,
                hash: key(number as u8),
                parent_hash: B256::ZERO,
                changeset_hash: None,
            },
            transactions: vec![ArkivTransaction {
                hash: key(100),
                index: 7,
                sender: addr(9),
                operations,
            }],
        }
    }

    fn create(n: u8, owner: Address, attrs: Vec<Annotation>) -> ArkivOperation {
        ArkivOperation::Create(CreateOp {
            op_index: n as u32,
            entity_key: key(n),
            owner,
            expires_at: 100,
            entity_hash: key(50 + n),
            changeset_hash: key(60 + n),
            payload: Bytes::from(vec![n]),
            content_type: "application/octet-stream".to_string(),
            annotations: attrs,
        })
    }

    #[test]
    fn stores_queries_and_projects_entities() -> Result<()> {
        let store = RockDbStore::temporary()?;
        store.handle_commit(&[block(
            1,
            vec![
                create(
                    1,
                    addr(1),
                    vec![
                        Annotation::String {
                            key: "kind".into(),
                            string_value: "image".into(),
                        },
                        Annotation::Numeric {
                            key: "size".into(),
                            numeric_value: 10,
                        },
                    ],
                ),
                create(
                    2,
                    addr(2),
                    vec![Annotation::Numeric {
                        key: "size".into(),
                        numeric_value: 3,
                    }],
                ),
            ],
        )])?;

        let response = store.query(
            r#"kind = "image" && size >= 10"#,
            QueryOptions {
                include_data: Some(IncludeData {
                    attributes: true,
                    payload: true,
                    owner: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )?;

        assert_eq!(response.data.len(), 1);
        assert_eq!(response.data[0].key, Some(key(1)));
        assert_eq!(response.data[0].value, Some(Bytes::from(vec![1])));
        assert_eq!(response.data[0].owner, Some(addr(1)));
        assert_eq!(
            response.data[0].string_attributes.as_ref().unwrap().len(),
            1
        );
        assert_eq!(
            response.data[0].numeric_attributes.as_ref().unwrap().len(),
            1
        );
        Ok(())
    }

    #[test]
    fn applies_updates_transfers_deletes_and_reverts() -> Result<()> {
        let store = RockDbStore::temporary()?;
        store.handle_commit(&[block(1, vec![create(1, addr(1), Vec::new())])])?;
        store.handle_commit(&[block(
            2,
            vec![
                ArkivOperation::Extend(ExtendOp {
                    op_index: 0,
                    entity_key: key(1),
                    owner: addr(1),
                    expires_at: 200,
                    entity_hash: key(11),
                    changeset_hash: key(12),
                }),
                ArkivOperation::ChangeOwner(ChangeOwnerOp {
                    op_index: 1,
                    entity_key: key(1),
                    owner: addr(3),
                    entity_hash: key(13),
                    changeset_hash: key(14),
                }),
            ],
        )])?;

        let entity = store.query(
            "$owner=0x0000000000000000000000000000000000000003",
            QueryOptions::default(),
        )?;
        assert_eq!(entity.data.len(), 1);

        store.handle_commit(&[block(
            3,
            vec![ArkivOperation::Delete(DeleteOp {
                op_index: 0,
                entity_key: key(1),
                owner: addr(3),
                entity_hash: key(15),
                changeset_hash: key(16),
            })],
        )])?;
        assert_eq!(store.entity_count()?, 0);

        store.handle_revert(&[ArkivBlockRef {
            number: 3,
            hash: key(3),
        }])?;
        assert_eq!(store.entity_count()?, 1);
        Ok(())
    }
}
