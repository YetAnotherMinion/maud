use std::mem;
use syntax::ast::{Expr, ExprParen, Lit, Stmt, TokenTree, TtDelimited, TtToken};
use syntax::codemap::Span;
use syntax::ext::base::ExtCtxt;
use syntax::parse;
use syntax::parse::parser::Parser as RustParser;
use syntax::parse::token::{self, DelimToken};
use syntax::ptr::P;

use super::render::{Escape, Renderer};

macro_rules! dollar {
    () => (TtToken(_, token::Dollar))
}
macro_rules! dot {
    () => (TtToken(_, token::Dot))
}
macro_rules! eq {
    () => (TtToken(_, token::Eq))
}
macro_rules! not {
    () => (TtToken(_, token::Not))
}
macro_rules! question {
    () => (TtToken(_, token::Question))
}
macro_rules! semi {
    () => (TtToken(_, token::Semi))
}
macro_rules! minus {
    () => (TtToken(_, token::BinOp(token::Minus)))
}
macro_rules! slash {
    () => (TtToken(_, token::BinOp(token::Slash)))
}
macro_rules! literal {
    () => (TtToken(_, token::Literal(..)))
}
macro_rules! ident {
    ($x:pat) => (ident!(_, $x));
    ($sp:pat, $x:pat) => (TtToken($sp, token::Ident($x, token::IdentStyle::Plain)))
}

pub fn parse(cx: &ExtCtxt, input: &[TokenTree], sp: Span) -> P<Expr> {
    let mut parser = Parser {
        in_attr: false,
        input: input,
        span: sp,
        render: Renderer::new(cx),
    };
    parser.markups();
    parser.into_render().into_expr()
}

struct Parser<'cx, 's: 'cx, 'i> {
    in_attr: bool,
    input: &'i [TokenTree],
    span: Span,
    render: Renderer<'cx, 's>,
}

impl<'cx, 's, 'i> Parser<'cx, 's, 'i> {
    /// Finalize the `Parser`, returning the `Renderer` underneath.
    fn into_render(self) -> Renderer<'cx, 's> {
        let Parser { render, .. } = self;
        render
    }

    /// Consume `n` items from the input.
    fn shift(&mut self, n: usize) {
        self.input = &self.input[n..];
    }

    /// Construct a Rust AST parser from the given token tree.
    fn new_rust_parser(&self, tts: Vec<TokenTree>) -> RustParser<'s> {
        parse::tts_to_parser(self.render.cx.parse_sess, tts, self.render.cx.cfg.clone())
    }

    fn markups(&mut self) {
        loop {
            match self.input {
                [] => return,
                [semi!(), ..] => self.shift(1),
                [_, ..] => if !self.markup() { return },
            }
        }
    }

    fn markup(&mut self) -> bool {
        match self.input {
            // Literal
            [minus!(), ref tt @ literal!(), ..] => {
                self.shift(2);
                self.literal(tt, true)
            },
            [ref tt @ literal!(), ..] => {
                self.shift(1);
                self.literal(tt, false)
            },
            // If
            [dollar!(), ident!(sp, name), ..] if name.as_str() == "if" => {
                self.shift(2);
                self.if_expr(sp);
            },
            // Splice
            [ref tt @ dollar!(), dollar!(), ..] => {
                self.shift(2);
                let expr = self.splice(tt.get_span());
                self.render.splice(expr, Escape::PassThru);
            },
            [ref tt @ dollar!(), ..] => {
                self.shift(1);
                let expr = self.splice(tt.get_span());
                self.render.splice(expr, Escape::Escape);
            },
            // Element
            [ident!(sp, name), ..] => {
                self.shift(1);
                self.element(sp, name.as_str())
            },
            // Block
            [TtDelimited(sp, ref d), ..] if d.delim == token::DelimToken::Brace => {
                self.shift(1);
                let stmts = self.block(sp, &d.tts);
                self.render.push_stmts(stmts);
            },
            // ???
            _ => {
                if let [ref tt, ..] = self.input {
                    self.render.cx.span_err(tt.get_span(), "invalid syntax");
                } else {
                    self.render.cx.span_err(self.span, "unexpected end of block");
                }
                return false;
            },
        }
        true
    }

    fn literal(&mut self, tt: &TokenTree, minus: bool) {
        let lit = self.new_rust_parser(vec![tt.clone()]).parse_lit();
        match lit_to_string(self.render.cx, lit, minus) {
            Some(s) => self.render.string(&s, Escape::Escape),
            None => {},
        }
    }

    fn if_expr(&mut self, sp: Span) {
        // Parse the initial if
        let mut cond_tts = vec![];
        let if_body;
        loop { match self.input {
            [TtDelimited(sp, ref d), ..] if d.delim == DelimToken::Brace => {
                self.shift(1);
                if_body = self.block(sp, &d.tts);
                break;
            },
            [ref tt, ..] => {
                self.shift(1);
                cond_tts.push(tt.clone());
            },
            [] => self.render.cx.span_fatal(sp, "expected body for this `if`"),
        }}
        let if_cond = self.new_rust_parser(cond_tts).parse_expr();
        // Parse the (optional) else
        let else_body = match self.input {
            [dollar!(), ident!(else_), ..] if else_.as_str() == "else" => {
                self.shift(2);
                match self.input {
                    [ident!(sp, if_), ..] if if_.as_str() == "if" => {
                        self.shift(1);
                        let else_body = {
                            // Parse an if expression, but capture the result
                            // rather than emitting it right away
                            let mut render = self.render.fork();
                            mem::swap(&mut self.render, &mut render);
                            self.if_expr(sp);
                            mem::swap(&mut self.render, &mut render);
                            render.into_stmts()
                        };
                        Some(else_body)
                    },
                    [TtDelimited(sp, ref d), ..] if d.delim == DelimToken::Brace => {
                        self.shift(1);
                        Some(self.block(sp, &d.tts))
                    },
                    _ => self.render.cx.span_fatal(sp, "invalid syntax"),
                }
            },
            _ => None,
        };
        self.render.emit_if(if_cond, if_body, else_body);
    }

    fn splice(&mut self, sp: Span) -> P<Expr> {
        let mut tts = vec![];
        // First, munch a single token tree
        if let [ref tt, ..] = self.input {
            self.shift(1);
            tts.push(tt.clone());
        }
        loop {
            match self.input {
                // Munch attribute lookups e.g. `$person.address.street`
                [ref dot @ dot!(), ref ident @ ident!(_), ..] => {
                    self.shift(2);
                    tts.push(dot.clone());
                    tts.push(ident.clone());
                },
                // Munch function calls `()` and indexing operations `[]`
                [TtDelimited(sp, ref d), ..] if d.delim != token::DelimToken::Brace => {
                    self.shift(1);
                    tts.push(TtDelimited(sp, d.clone()));
                },
                _ => break,
            }
        }
        if tts.is_empty() {
            self.render.cx.span_fatal(sp, "expected expression for this splice");
        } else {
            self.new_rust_parser(tts).parse_expr()
        }
    }

    fn element(&mut self, sp: Span, name: &str) {
        if self.in_attr {
            self.render.cx.span_err(sp, "unexpected element, you silly bumpkin");
            return;
        }
        self.render.element_open_start(name);
        self.attrs();
        self.render.element_open_end();
        if let [slash!(), ..] = self.input {
            self.shift(1);
        } else {
            self.markup();
            self.render.element_close(name);
        }
    }

    fn attrs(&mut self) {
        loop { match self.input {
            [ident!(name), eq!(), ..] => {
                // Non-empty attribute
                self.shift(2);
                self.render.attribute_start(name.as_str());
                {
                    // Parse a value under an attribute context
                    let mut in_attr = true;
                    mem::swap(&mut self.in_attr, &mut in_attr);
                    self.markup();
                    mem::swap(&mut self.in_attr, &mut in_attr);
                }
                self.render.attribute_end();
            },
            [ident!(name), question!(), ..] => {
                // Empty attribute
                self.shift(2);
                if let [ref tt @ eq!(), ..] = self.input {
                    // Toggle the attribute based on a boolean expression
                    self.shift(1);
                    let cond = self.splice(tt.get_span());
                    // Silence "unnecessary parentheses" warnings
                    let cond = strip_outer_parens(cond);
                    let body = {
                        let mut r = self.render.fork();
                        r.attribute_empty(name.as_str());
                        r.into_stmts()
                    };
                    self.render.emit_if(cond, body, None);
                } else {
                    // Write the attribute unconditionally
                    self.render.attribute_empty(name.as_str());
                }
            },
            _ => return,
        }}
    }

    fn block(&mut self, sp: Span, tts: &[TokenTree]) -> Vec<P<Stmt>> {
        let mut parse = Parser {
            in_attr: self.in_attr,
            input: tts,
            span: sp,
            render: self.render.fork(),
        };
        parse.markups();
        parse.into_render().into_stmts()
    }
}

/// Convert a literal to a string.
fn lit_to_string(cx: &ExtCtxt, lit: Lit, minus: bool) -> Option<String> {
    use syntax::ast::Lit_::*;
    let mut result = String::new();
    if minus {
        result.push('-');
    }
    match lit.node {
        LitStr(s, _) => result.push_str(&s),
        LitBinary(..) | LitByte(..) => {
            cx.span_err(lit.span, "cannot splice binary data");
            return None;
        },
        LitChar(c) => result.push(c),
        LitInt(x, _) => result.push_str(&x.to_string()),
        LitFloat(s, _) | LitFloatUnsuffixed(s) => result.push_str(&s),
        LitBool(b) => result.push_str(if b { "true" } else { "false" }),
    };
    Some(result)
}

/// If the expression is wrapped in parentheses, strip them off.
fn strip_outer_parens(expr: P<Expr>) -> P<Expr> {
    expr.and_then(|expr| match expr {
        Expr { node: ExprParen(inner), .. } => inner,
        expr => P(expr),
    })
}
