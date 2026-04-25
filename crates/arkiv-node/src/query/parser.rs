//! Recursive-descent parser for the Arkiv query language.
//!
//! Grammar (informal):
//!
//! ```text
//! expr     := or_expr
//! or_expr  := and_expr ("||" and_expr)*
//! and_expr := unary    ("&&" unary)*
//! unary    := "!" IDENT             # attribute-presence negation
//!           | "(" or_expr ")"
//!           | cmp
//! cmp      := selector OP literal
//! selector := IDENT | "$key" | "$owner" | "$creator"
//! OP       := "=" | "!=" | "<" | "<=" | ">" | ">="
//! literal  := STRING | NUMBER | HEX
//! ```
//!
//! `!` followed by a parenthesised expression is currently treated as
//! the same identifier-negation (the SDK only emits `!IDENT`).

use eyre::{Result, bail};

use super::{CmpOp, Expr, Selector, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Ident(String),
    Selector(Selector),
    Op(CmpOp),
    String(String),
    Number(u64),
    Hex(Vec<u8>),
    LParen,
    RParen,
    And,
    Or,
    Bang,
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self { src: src.as_bytes(), pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn lex_string(&mut self) -> Result<Token> {
        // opening quote already consumed
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == b'"' {
                let s = std::str::from_utf8(&self.src[start..self.pos])?.to_string();
                self.pos += 1; // closing quote
                return Ok(Token::String(s));
            }
            self.pos += 1;
        }
        bail!("unterminated string literal");
    }

    fn lex_hex(&mut self) -> Result<Token> {
        // "0x" already consumed by caller
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_hexdigit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        let hex_str = std::str::from_utf8(&self.src[start..self.pos])?;
        if hex_str.is_empty() {
            bail!("empty hex literal");
        }
        // Pad to even length so hex::decode works for odd-digit literals.
        let bytes = if hex_str.len() % 2 == 1 {
            let mut s = String::with_capacity(hex_str.len() + 1);
            s.push('0');
            s.push_str(hex_str);
            hex::decode(&s)?
        } else {
            hex::decode(hex_str)?
        };
        Ok(Token::Hex(bytes))
    }

    fn lex_number(&mut self) -> Result<Token> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        let n: u64 = std::str::from_utf8(&self.src[start..self.pos])?.parse()?;
        Ok(Token::Number(n))
    }

    fn lex_ident_or_selector(&mut self, dollar: bool) -> Result<Token> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'-' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let ident = std::str::from_utf8(&self.src[start..self.pos])?.to_string();
        if dollar {
            let sel = match ident.as_str() {
                "key" => Selector::Key,
                "owner" => Selector::Owner,
                "creator" => Selector::Creator,
                other => bail!("unknown selector: ${}", other),
            };
            Ok(Token::Selector(sel))
        } else {
            Ok(Token::Ident(ident))
        }
    }

    fn next_token(&mut self) -> Result<Option<Token>> {
        self.skip_ws();
        let Some(c) = self.peek() else {
            return Ok(None);
        };
        let tok = match c {
            b'(' => {
                self.pos += 1;
                Token::LParen
            }
            b')' => {
                self.pos += 1;
                Token::RParen
            }
            b'&' => {
                if self.src.get(self.pos + 1) == Some(&b'&') {
                    self.pos += 2;
                    Token::And
                } else {
                    bail!("expected '&&'");
                }
            }
            b'|' => {
                if self.src.get(self.pos + 1) == Some(&b'|') {
                    self.pos += 2;
                    Token::Or
                } else {
                    bail!("expected '||'");
                }
            }
            b'=' => {
                self.pos += 1;
                Token::Op(CmpOp::Eq)
            }
            b'!' => {
                if self.src.get(self.pos + 1) == Some(&b'=') {
                    self.pos += 2;
                    Token::Op(CmpOp::Neq)
                } else {
                    self.pos += 1;
                    Token::Bang
                }
            }
            b'<' => {
                if self.src.get(self.pos + 1) == Some(&b'=') {
                    self.pos += 2;
                    Token::Op(CmpOp::Lte)
                } else {
                    self.pos += 1;
                    Token::Op(CmpOp::Lt)
                }
            }
            b'>' => {
                if self.src.get(self.pos + 1) == Some(&b'=') {
                    self.pos += 2;
                    Token::Op(CmpOp::Gte)
                } else {
                    self.pos += 1;
                    Token::Op(CmpOp::Gt)
                }
            }
            b'"' => {
                self.pos += 1;
                self.lex_string()?
            }
            b'$' => {
                self.pos += 1;
                self.lex_ident_or_selector(true)?
            }
            b'0' if self.src.get(self.pos + 1) == Some(&b'x')
                || self.src.get(self.pos + 1) == Some(&b'X') =>
            {
                self.pos += 2;
                self.lex_hex()?
            }
            c if c.is_ascii_digit() => self.lex_number()?,
            c if c.is_ascii_alphabetic() || c == b'_' => self.lex_ident_or_selector(false)?,
            c => bail!("unexpected character: {:?}", c as char),
        };
        Ok(Some(tok))
    }

    fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut out = Vec::new();
        while let Some(t) = self.next_token()? {
            out.push(t);
        }
        Ok(out)
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let first = self.parse_and()?;
        let mut parts = vec![first];
        while matches!(self.peek(), Some(Token::Or)) {
            self.bump();
            parts.push(self.parse_and()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Expr::Or(parts) })
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let first = self.parse_unary()?;
        let mut parts = vec![first];
        while matches!(self.peek(), Some(Token::And)) {
            self.bump();
            parts.push(self.parse_unary()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Expr::And(parts) })
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Some(Token::Bang)) {
            self.bump();
            // The SDK only emits `!IDENT`. Accept selector or identifier.
            let sel = match self.bump() {
                Some(Token::Ident(n)) => Selector::Attr(n),
                Some(Token::Selector(s)) => s,
                Some(other) => bail!("expected identifier after '!', got {:?}", other),
                None => bail!("expected identifier after '!'"),
            };
            return Ok(Expr::Not(sel));
        }
        if matches!(self.peek(), Some(Token::LParen)) {
            self.bump();
            let inner = self.parse_or()?;
            match self.bump() {
                Some(Token::RParen) => Ok(inner),
                other => bail!("expected ')', got {:?}", other),
            }
        } else {
            self.parse_cmp()
        }
    }

    fn parse_cmp(&mut self) -> Result<Expr> {
        let selector = match self.bump() {
            Some(Token::Ident(n)) => Selector::Attr(n),
            Some(Token::Selector(s)) => s,
            other => bail!("expected identifier or selector, got {:?}", other),
        };
        let op = match self.bump() {
            Some(Token::Op(o)) => o,
            other => bail!("expected comparison operator, got {:?}", other),
        };
        let value = match self.bump() {
            Some(Token::String(s)) => Value::String(s),
            Some(Token::Number(n)) => Value::Number(n),
            Some(Token::Hex(b)) => Value::Hex(b),
            other => bail!("expected literal value, got {:?}", other),
        };
        Ok(Expr::Cmp { selector, op, value })
    }
}

/// Parse a query string into an [`Expr`]. An empty/whitespace-only query
/// returns [`Expr::True`] (matches all entities).
pub fn parse(input: &str) -> Result<Expr> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Expr::True);
    }
    let tokens = Lexer::new(trimmed).tokenize()?;
    if tokens.is_empty() {
        return Ok(Expr::True);
    }
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.parse_expr()?;
    if parser.pos != parser.tokens.len() {
        bail!("trailing tokens at position {}", parser.pos);
    }
    Ok(expr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{CmpOp, Selector, Value};

    #[test]
    fn parses_empty_query_as_true() {
        assert_eq!(parse("").unwrap(), Expr::True);
        assert_eq!(parse("   ").unwrap(), Expr::True);
    }

    #[test]
    fn parses_simple_eq() {
        let e = parse("name = \"John\"").unwrap();
        assert_eq!(
            e,
            Expr::Cmp {
                selector: Selector::Attr("name".into()),
                op: CmpOp::Eq,
                value: Value::String("John".into()),
            }
        );
    }

    #[test]
    fn parses_numeric_comparisons() {
        let e = parse("age >= 30").unwrap();
        assert!(matches!(
            e,
            Expr::Cmp { op: CmpOp::Gte, value: Value::Number(30), .. }
        ));
    }

    #[test]
    fn parses_and_or_with_parens() {
        let e = parse("name = \"John\" && (age = 30 || age = 31)").unwrap();
        match e {
            Expr::And(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(&parts[1], Expr::Or(p) if p.len() == 2));
            }
            _ => panic!("expected And"),
        }
    }

    #[test]
    fn parses_not() {
        assert_eq!(parse("!deprecated").unwrap(), Expr::Not(Selector::Attr("deprecated".into())));
    }

    #[test]
    fn parses_selectors_and_hex() {
        let e = parse("$owner = 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266").unwrap();
        match e {
            Expr::Cmp { selector: Selector::Owner, op: CmpOp::Eq, value: Value::Hex(b) } => {
                assert_eq!(b.len(), 20);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn parses_short_hex() {
        let e = parse("$key = 0x1").unwrap();
        match e {
            Expr::Cmp { value: Value::Hex(b), .. } => assert_eq!(b, vec![1u8]),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn rejects_unknown_selector() {
        assert!(parse("$foo = 1").is_err());
    }

    #[test]
    fn rejects_unterminated_string() {
        assert!(parse("name = \"oops").is_err());
    }
}
