//! L2 — rustlite: a Rust subset that compiles in the browser.
//!
//! the whole bet (CARTRIDGE_PLAN §5): full rustc-on-wasm fails on the
//! LINKER; a language small enough to emit final wasm bytes directly has no
//! linking step at all. this module is the front half — lexer + Pratt
//! parser + typed AST. emission is `wasm.rs`; the AST below is designed so
//! every construct maps 1:1 to a wasm instruction or control frame.
//! anything that cannot so map is refused HERE, at parse time, with the
//! reason — not discovered mid-emission.
//!
//! host access: `extern "C" { fn … ; }` declares imports from the L1 ABI
//! (`abi.rs`); `pub fn` exports a function by name. memory and packed-
//! result handling go through the intrinsics in `wasm.rs` (load_u8,
//! store_u8, load_i32, store_i32, memory_size, pack, unpack_ptr,
//! unpack_len, data_end) — they parse as ordinary calls and lower inline.
//! a string literal is an i64 expression: the packed (ptr, len) of its
//! bytes, placed in the module's data segment by the emitter.

/// the closed type set. deliberately tiny: each variant maps to exactly one
/// wasm valtype, which is what makes type checking trivial and codegen
/// mechanical. no generics, no traits, no references-as-types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    I32,
    I64,
    F32,
    F64,
    Bool,
}

impl Ty {
    /// the name as written in source.
    pub fn from_name(name: &str) -> Option<Ty> {
        match name {
            "i32" => Some(Ty::I32),
            "i64" => Some(Ty::I64),
            "f32" => Some(Ty::F32),
            "f64" => Some(Ty::F64),
            "bool" => Some(Ty::Bool),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Ty::I32 => "i32",
            Ty::I64 => "i64",
            Ty::F32 => "f32",
            Ty::F64 => "f64",
            Ty::Bool => "bool",
        }
    }

    /// the wasm valtype byte this type lowers to. bools are i32 on the wire;
    /// the distinction lives in the checker only. ONE mapping shared by the
    /// emitter (writing signatures) and the ABI table (checking them), so a
    /// declared import cannot lower to a different shape than the host expects.
    pub fn valtype(self) -> u8 {
        match self {
            Ty::I32 | Ty::Bool => 0x7f,
            Ty::I64 => 0x7e,
            Ty::F32 => 0x7d,
            Ty::F64 => 0x7c,
        }
    }
}

// ---- tokens ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    /// integer literal (i64-valued; small enough for any cartridge constant)
    Int(i64),
    Float(f64),
    /// string literal: the ABI name of an `extern "C"` block, or an
    /// expression whose value is the packed (ptr, len) of its bytes in the
    /// module's data segment (see Expr::StrLit).
    Str(String),
    Kw(&'static str),
    // punctuation / operators
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Semi,
    Arrow,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Lt,
    Gt,
    Le,
    Ge,
    EqEq,
    Ne,
    AndAnd,
    OrOr,
    Assign,
}

const KEYWORDS: &[&str] = &[
    "fn", "let", "if", "else", "while", "return", "true", "false", "struct", "pub", "extern",
];

/// one lex error, with byte offset for editor-grade reporting later.
#[derive(Debug, Clone, PartialEq)]
pub struct LexError {
    pub pos: usize,
    pub msg: String,
}

pub fn lex(src: &str) -> Result<Vec<Tok>, LexError> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // comments: `//` to end of line. block comments are refused (v1).
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &src[start..i];
            out.push(match KEYWORDS.iter().find(|k| **k == word) {
                Some(k) => Tok::Kw(k),
                None => Tok::Ident(word.to_string()),
            });
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            let mut is_float = false;
            while i < b.len()
                && (b[i].is_ascii_digit() || (b[i] == b'.' && !is_float))
            {
                if b[i] == b'.' {
                    // "1.foo" is not a float; a digit must follow the dot.
                    if i + 1 >= b.len() || !b[i + 1].is_ascii_digit() {
                        break;
                    }
                    is_float = true;
                }
                i += 1;
            }
            let text = &src[start..i];
            out.push(if is_float {
                Tok::Float(text.parse::<f64>().map_err(|_| LexError {
                    pos: start,
                    msg: format!("bad float literal '{text}'"),
                })?)
            } else {
                Tok::Int(text.parse::<i64>().map_err(|_| LexError {
                    pos: start,
                    msg: format!("integer literal '{text}' out of range"),
                })?)
            });
            continue;
        }
        // string literal: bytes up to the closing quote, no escapes in v1
        // (an escape would need a decoder on both the lexer and the future
        // data-segment emitter; refusing keeps them from drifting apart).
        if c == '"' {
            let start = i;
            i += 1;
            let body_start = i;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' {
                    return Err(LexError {
                        pos: i,
                        msg: "escape sequences in string literals are not supported in v1"
                            .to_string(),
                    });
                }
                i += 1;
            }
            if i >= b.len() {
                return Err(LexError {
                    pos: start,
                    msg: "unterminated string literal".to_string(),
                });
            }
            out.push(Tok::Str(src[body_start..i].to_string()));
            i += 1; // closing quote
            continue;
        }
        // multi-char operators first, then singles.
        let two = if i + 1 < b.len() { &src[i..i + 2] } else { "" };
        let (tok, len) = match two {
            "->" => (Tok::Arrow, 2),
            "<=" => (Tok::Le, 2),
            ">=" => (Tok::Ge, 2),
            "==" => (Tok::EqEq, 2),
            "!=" => (Tok::Ne, 2),
            "&&" => (Tok::AndAnd, 2),
            "||" => (Tok::OrOr, 2),
            _ => match c {
                '(' => (Tok::LParen, 1),
                ')' => (Tok::RParen, 1),
                '{' => (Tok::LBrace, 1),
                '}' => (Tok::RBrace, 1),
                ',' => (Tok::Comma, 1),
                ':' => (Tok::Colon, 1),
                ';' => (Tok::Semi, 1),
                '+' => (Tok::Plus, 1),
                '-' => (Tok::Minus, 1),
                '*' => (Tok::Star, 1),
                '/' => (Tok::Slash, 1),
                '%' => (Tok::Percent, 1),
                '<' => (Tok::Lt, 1),
                '>' => (Tok::Gt, 1),
                '=' => (Tok::Assign, 1),
                other => {
                    return Err(LexError {
                        pos: i,
                        msg: format!("unexpected character '{other}' — rustlite v1 \
                                      has no chars, attributes, or paths yet"),
                    })
                }
            },
        };
        out.push(tok);
        i += len;
    }
    Ok(out)
}

// ---- AST -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct FnSig {
    pub name: String,
    pub params: Vec<(String, Ty)>,
    pub ret: Option<Ty>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FnDecl {
    pub sig: FnSig,
    pub body: Block,
    /// `pub fn` = exported from the module under its own name. that is the
    /// whole visibility story in v1: no attributes, no `#[no_mangle]` — the
    /// L1 lifecycle exports (cart_init/cart_handle/cart_alloc) are simply
    /// `pub fn`s with the ABI's signatures, which the checker verifies.
    pub is_pub: bool,
}

/// one translation unit. `externs` are host imports declared in
/// `extern "C" { … }` blocks — every one must name a function in the L1 ABI
/// table (checked at compile time, and again by the runtime at load, so a
/// cartridge can never reach a host function the host does not have).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Program {
    pub externs: Vec<FnSig>,
    pub fns: Vec<FnDecl>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// typed binding: `let x: i32 = expr;`. the annotation is REQUIRED in
    /// v1 — it keeps type checking a one-pass walk instead of an inference
    /// engine, which is the whole reason the checker stays small.
    Let { name: String, ty: Ty, init: Expr },
    /// reassignment of a binding or parameter in scope: `x = expr;`.
    /// emission maps to wasm local.set; assigning to an undeclared name is
    /// a checker error, not a parse error (parsing stays scope-blind).
    Assign { name: String, value: Expr },
    While { cond: Expr, body: Block },
    /// `if cond { … } [else { … }]`. `else if` parses as an else block
    /// holding a single nested If, exactly the desugaring rust uses. maps
    /// to wasm's if/else/end control frame (void blocktype — arms are
    /// statements, never values).
    If { cond: Expr, then: Block, els: Option<Block> },
    Return(Option<Expr>),
    Expr(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Lt,
    Gt,
    Le,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

impl BinOp {
    /// result type of applying this op. arithmetic preserves the operand
    /// type; comparisons always yield bool. pure so the checker pins it.
    pub fn result_ty(self, operand: Ty) -> Result<Ty, String> {
        match self {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => Ok(operand),
            BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne | BinOp::And
            | BinOp::Or => Ok(Ty::Bool),
        }
    }
}

/// prefix unary: only minus exists in v1 (bool-not arrives with its use case).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    Neg,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    IntLit(i64),
    FloatLit(f64),
    BoolLit(bool),
    Var(String),
    /// a string literal. its VALUE is an i64: the packed (ptr, len) of the
    /// bytes in the module's data segment — the ABI's own representation
    /// of a string — so `unpack_ptr("inc")` / `unpack_len("inc")` feed any
    /// host call directly. no string type, no new intrinsics: one node.
    StrLit(String),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call { callee: String, args: Vec<Expr> },
}

// ---- parser ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub msg: String,
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

/// parse a whole translation unit: `extern "C"` blocks and function
/// declarations in any order.
pub fn parse(src: &str) -> Result<Program, ParseError> {
    let toks = lex(src).map_err(|e| ParseError {
        msg: format!("lex error at byte {}: {}", e.pos, e.msg),
    })?;
    let mut p = Parser { toks, pos: 0 };
    let mut program = Program::default();
    while !p.at_end() {
        match p.peek() {
            Some(Tok::Kw("extern")) => p.parse_extern_block(&mut program.externs)?,
            _ => {
                let f = p.parse_fn()?;
                program.fns.push(f);
            }
        }
    }
    if program.fns.is_empty() {
        return Err(ParseError {
            msg: "source contains no functions".to_string(),
        });
    }
    Ok(program)
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }

    fn expect(&mut self, t: &Tok) -> Result<(), ParseError> {
        match self.peek() {
            Some(got) if got == t => {
                self.pos += 1;
                Ok(())
            }
            Some(got) => Err(ParseError {
                msg: format!("expected {t:?}, found {got:?}"),
            }),
            None => Err(ParseError {
                msg: format!("expected {t:?}, reached end of source"),
            }),
        }
    }

    fn expect_kw(&mut self, kw: &'static str) -> Result<(), ParseError> {
        match self.peek() {
            Some(Tok::Kw(k)) if *k == kw => {
                self.pos += 1;
                Ok(())
            }
            other => Err(ParseError {
                msg: format!("expected keyword '{kw}', found {other:?}"),
            }),
        }
    }

    fn ident(&mut self) -> Result<String, ParseError> {
        match self.next() {
            Some(Tok::Ident(s)) => Ok(s),
            other => Err(ParseError {
                msg: format!("expected identifier, found {other:?}"),
            }),
        }
    }

    fn ty(&mut self) -> Result<Ty, ParseError> {
        match self.next() {
            Some(Tok::Ident(s)) => Ty::from_name(&s).ok_or_else(|| ParseError {
                msg: format!(
                    "unknown type '{s}' — rustlite v1 has exactly i32, i64, \
                     f32, f64, bool"
                ),
            }),
            other => Err(ParseError {
                msg: format!("expected a type name, found {other:?}"),
            }),
        }
    }

    /// `extern "C" { fn name(params) [-> ty]; … }`. only the ABI name "C"
    /// exists — it is the calling convention the L1 host surface speaks —
    /// so anything else is refused with the reason rather than accepted and
    /// then silently linked against nothing.
    fn parse_extern_block(&mut self, externs: &mut Vec<FnSig>) -> Result<(), ParseError> {
        self.expect_kw("extern")?;
        match self.next() {
            Some(Tok::Str(abi)) if abi == "C" => {}
            Some(Tok::Str(abi)) => {
                return Err(ParseError {
                    msg: format!("extern \"{abi}\" is not supported — only extern \"C\" exists"),
                })
            }
            other => {
                return Err(ParseError {
                    msg: format!("expected \"C\" after extern, found {other:?}"),
                })
            }
        }
        self.expect(&Tok::LBrace)?;
        loop {
            if matches!(self.peek(), Some(Tok::RBrace)) {
                self.next();
                return Ok(());
            }
            if self.at_end() {
                return Err(ParseError {
                    msg: "extern block never closed — '}' missing".to_string(),
                });
            }
            let sig = self.parse_sig()?;
            self.expect(&Tok::Semi)?;
            externs.push(sig);
        }
    }

    /// `fn name(params) [-> ty]` — the head shared by declarations and
    /// extern prototypes.
    fn parse_sig(&mut self) -> Result<FnSig, ParseError> {
        self.expect_kw("fn")?;
        let name = self.ident()?;
        self.expect(&Tok::LParen)?;
        let mut params = Vec::new();
        loop {
            if matches!(self.peek(), Some(Tok::RParen)) {
                self.next();
                break;
            }
            let pname = self.ident()?;
            self.expect(&Tok::Colon)?;
            let pty = self.ty()?;
            params.push((pname, pty));
            match self.next() {
                Some(Tok::Comma) => continue,
                Some(Tok::RParen) => break,
                other => {
                    return Err(ParseError {
                        msg: format!("expected ',' or ')' in parameter list, found {other:?}"),
                    })
                }
            }
        }
        let ret = if matches!(self.peek(), Some(Tok::Arrow)) {
            self.next();
            Some(self.ty()?)
        } else {
            None
        };
        Ok(FnSig { name, params, ret })
    }

    fn parse_fn(&mut self) -> Result<FnDecl, ParseError> {
        let is_pub = if matches!(self.peek(), Some(Tok::Kw("pub"))) {
            self.next();
            true
        } else {
            false
        };
        let sig = self.parse_sig()?;
        let body = self.parse_block()?;
        Ok(FnDecl { sig, body, is_pub })
    }

    fn parse_block(&mut self) -> Result<Block, ParseError> {
        self.expect(&Tok::LBrace)?;
        let mut stmts = Vec::new();
        loop {
            match self.peek() {
                Some(Tok::RBrace) => {
                    self.next();
                    return Ok(Block { stmts });
                }
                None => {
                    return Err(ParseError {
                        msg: "block never closed — '}}' missing".to_string(),
                    })
                }
                _ => stmts.push(self.parse_stmt()?),
            }
        }
    }

    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        match self.peek().cloned() {
            Some(Tok::Kw("let")) => {
                self.next();
                let name = self.ident()?;
                self.expect(&Tok::Colon)?;
                let ty = self.ty()?;
                self.expect(&Tok::Assign)?;
                let init = self.parse_expr(0)?;
                self.expect(&Tok::Semi)?;
                Ok(Stmt::Let { name, ty, init })
            }
            Some(Tok::Kw("while")) => {
                self.next();
                let cond = self.parse_expr(0)?;
                let body = self.parse_block()?;
                Ok(Stmt::While { cond, body })
            }
            Some(Tok::Kw("if")) => {
                self.next();
                self.parse_if_tail()
            }
            Some(Tok::Kw("return")) => {
                self.next();
                // `return;` vs `return expr;` — decided by the semicolon.
                let value = if matches!(self.peek(), Some(Tok::Semi)) {
                    None
                } else {
                    Some(self.parse_expr(0)?)
                };
                self.expect(&Tok::Semi)?;
                Ok(Stmt::Return(value))
            }
            Some(Tok::Ident(_)) if self.is_assign_stmt() => {
                let name = self.ident()?;
                self.next(); // consume the '=' peeked at below
                let value = self.parse_expr(0)?;
                self.expect(&Tok::Semi)?;
                Ok(Stmt::Assign { name, value })
            }
            _ => {
                let e = self.parse_expr(0)?;
                self.expect(&Tok::Semi)?;
                Ok(Stmt::Expr(e))
            }
        }
    }

    /// the part after `if`: condition, then-block, optional else. `else if`
    /// nests: the else block holds exactly one If statement, so the checker
    /// and emitter never see a special form.
    fn parse_if_tail(&mut self) -> Result<Stmt, ParseError> {
        let cond = self.parse_expr(0)?;
        let then = self.parse_block()?;
        let els = if matches!(self.peek(), Some(Tok::Kw("else"))) {
            self.next();
            if matches!(self.peek(), Some(Tok::Kw("if"))) {
                self.next();
                let nested = self.parse_if_tail()?;
                Some(Block {
                    stmts: vec![nested],
                })
            } else {
                Some(self.parse_block()?)
            }
        } else {
            None
        };
        Ok(Stmt::If { cond, then, els })
    }

    /// disambiguates `x = ...;` (assignment) from an expression statement.
    /// only lookahead, never consumes. a bare `=` can never start a valid
    /// expression in v1 (no compound ops yet), so two tokens decide it.
    fn is_assign_stmt(&self) -> bool {
        matches!(self.toks.get(self.pos + 1), Some(Tok::Assign))
    }

    /// Pratt parser over the fixed precedence ladder. levels are indices:
    /// higher binds tighter. `||` < `&&` < comparisons < additive <
    /// multiplicative. this ordering is load-bearing for emission, where
    /// precedence becomes tree shape.
    fn parse_expr(&mut self, min_level: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let Some(op) = self.peek_binop() else {
                return Ok(lhs);
            };
            let level = binop_level(op);
            if level < min_level {
                return Ok(lhs);
            }
            self.next();
            let rhs = self.parse_expr(level + 1)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
    }

    fn peek_binop(&self) -> Option<BinOp> {
        match self.peek()? {
            Tok::Plus => Some(BinOp::Add),
            Tok::Minus => Some(BinOp::Sub),
            Tok::Star => Some(BinOp::Mul),
            Tok::Slash => Some(BinOp::Div),
            Tok::Percent => Some(BinOp::Rem),
            Tok::Lt => Some(BinOp::Lt),
            Tok::Gt => Some(BinOp::Gt),
            Tok::Le => Some(BinOp::Le),
            Tok::Ge => Some(BinOp::Ge),
            Tok::EqEq => Some(BinOp::Eq),
            Tok::Ne => Some(BinOp::Ne),
            Tok::AndAnd => Some(BinOp::And),
            Tok::OrOr => Some(BinOp::Or),
            _ => None,
        }
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if matches!(self.peek(), Some(Tok::Minus)) {
            self.next();
            return Ok(Expr::Unary(UnOp::Neg, Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        match self.next() {
            Some(Tok::Int(v)) => Ok(Expr::IntLit(v)),
            Some(Tok::Float(v)) => Ok(Expr::FloatLit(v)),
            Some(Tok::Kw("true")) => Ok(Expr::BoolLit(true)),
            Some(Tok::Kw("false")) => Ok(Expr::BoolLit(false)),
            Some(Tok::Ident(name)) => {
                if matches!(self.peek(), Some(Tok::LParen)) {
                    self.next();
                    let mut args = Vec::new();
                    loop {
                        if matches!(self.peek(), Some(Tok::RParen)) {
                            self.next();
                            break;
                        }
                        args.push(self.parse_expr(0)?);
                        match self.next() {
                            Some(Tok::Comma) => continue,
                            Some(Tok::RParen) => break,
                            other => {
                                return Err(ParseError {
                                    msg: format!(
                                        "expected ',' or ')' in call arguments, found {other:?}"
                                    ),
                                })
                            }
                        }
                    }
                    Ok(Expr::Call { callee: name, args })
                } else {
                    Ok(Expr::Var(name))
                }
            }
            Some(Tok::LParen) => {
                let e = self.parse_expr(0)?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Some(Tok::Str(s)) => Ok(Expr::StrLit(s)),
            other => Err(ParseError {
                msg: format!("expected an expression, found {other:?}"),
            }),
        }
    }
}

fn binop_level(op: BinOp) -> u8 {
    match op {
        BinOp::Or => 1,
        BinOp::And => 2,
        BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne => 3,
        BinOp::Add | BinOp::Sub => 4,
        BinOp::Mul | BinOp::Div | BinOp::Rem => 5,
    }
}
