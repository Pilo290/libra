// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Error};
use codespan::{ByteIndex, Span};
use std::fmt;
use std::str::FromStr;

use crate::lexer::*;
use hex;
use libra_types::identifier::Identifier;
use libra_types::{account_address::AccountAddress, byte_array::ByteArray};
use move_ir_types::{ast::*, spec_language_ast::*};

// FIXME: The following simplified version of ParseError copied from
// lalrpop-util should be replaced.

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParseError<L, E> {
    InvalidToken { location: L },
    User { error: E },
}

impl<L> From<Error> for ParseError<L, Error> {
    fn from(error: Error) -> Self {
        ParseError::User { error }
    }
}

impl<L, E> fmt::Display for ParseError<L, E>
where
    L: fmt::Display,
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::ParseError::*;
        match *self {
            User { ref error } => write!(f, "{}", error),
            InvalidToken { ref location } => write!(f, "Invalid token at {}", location),
        }
    }
}

fn spanned<T>(start: usize, end: usize, value: T) -> Spanned<T> {
    Spanned {
        value,
        span: Span::new(ByteIndex(start as u32), ByteIndex(end as u32)),
    }
}

fn consume_token<'input>(
    tokens: &mut Lexer<'input>,
    tok: Tok,
) -> Result<(), ParseError<usize, anyhow::Error>> {
    if tokens.peek() != tok {
        return Err(ParseError::InvalidToken {
            location: tokens.start_loc(),
        });
    }
    tokens.advance()?;
    Ok(())
}

fn adjust_token<'input>(
    tokens: &mut Lexer<'input>,
    list_end_tokens: &[Tok],
) -> Result<(), ParseError<usize, anyhow::Error>> {
    if tokens.peek() == Tok::GreaterGreater && list_end_tokens.contains(&Tok::Greater) {
        tokens.replace_token(Tok::Greater, 1)?;
    }
    Ok(())
}

fn parse_comma_list<'input, F, R>(
    tokens: &mut Lexer<'input>,
    list_end_tokens: &[Tok],
    parse_list_item: F,
    allow_trailing_comma: bool,
) -> Result<Vec<R>, ParseError<usize, anyhow::Error>>
where
    F: Fn(&mut Lexer<'input>) -> Result<R, ParseError<usize, anyhow::Error>>,
{
    let mut v = vec![];
    adjust_token(tokens, list_end_tokens)?;
    if !list_end_tokens.contains(&tokens.peek()) {
        loop {
            v.push(parse_list_item(tokens)?);
            adjust_token(tokens, list_end_tokens)?;
            if list_end_tokens.contains(&tokens.peek()) {
                break;
            }
            consume_token(tokens, Tok::Comma)?;
            adjust_token(tokens, list_end_tokens)?;
            if list_end_tokens.contains(&tokens.peek()) && allow_trailing_comma {
                break;
            }
        }
    }
    Ok(v)
}

fn parse_name<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<String, ParseError<usize, anyhow::Error>> {
    if tokens.peek() != Tok::NameValue {
        return Err(ParseError::InvalidToken {
            location: tokens.start_loc(),
        });
    }
    let name = tokens.content().to_string();
    tokens.advance()?;
    Ok(name)
}

fn parse_name_begin_ty<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<String, ParseError<usize, anyhow::Error>> {
    if tokens.peek() != Tok::NameBeginTyValue {
        return Err(ParseError::InvalidToken {
            location: tokens.start_loc(),
        });
    }
    let s = tokens.content();
    // The token includes a "<" at the end, so chop that off to get the name.
    let name = s[..s.len() - 1].to_string();
    tokens.advance()?;
    Ok(name)
}

fn parse_dot_name<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<String, ParseError<usize, anyhow::Error>> {
    if tokens.peek() != Tok::DotNameValue {
        return Err(ParseError::InvalidToken {
            location: tokens.start_loc(),
        });
    }
    let name = tokens.content().to_string();
    tokens.advance()?;
    Ok(name)
}

// AccountAddress: AccountAddress = {
//     < s: r"0[xX][0-9a-fA-F]+" > => { ... }
// };

fn parse_account_address<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<AccountAddress, ParseError<usize, anyhow::Error>> {
    if tokens.peek() != Tok::AccountAddressValue {
        return Err(ParseError::InvalidToken {
            location: tokens.start_loc(),
        });
    }
    let addr = AccountAddress::from_hex_literal(&tokens.content())
        .with_context(|| {
            format!(
                "The address {:?} is of invalid length. Addresses are at most 32-bytes long",
                tokens.content()
            )
        })
        .unwrap();
    tokens.advance()?;
    Ok(addr)
}

// Var: Var = {
//     <n:Name> =>? Var::parse(n),
// };

fn parse_var_<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Var_, ParseError<usize, anyhow::Error>> {
    Ok(Var_::parse(parse_name(tokens)?)?)
}

fn parse_var<'input>(tokens: &mut Lexer<'input>) -> Result<Var, ParseError<usize, anyhow::Error>> {
    let start_loc = tokens.start_loc();
    let var = parse_var_(tokens)?;
    let end_loc = tokens.previous_end_loc();
    Ok(spanned(start_loc, end_loc, var))
}

// Field: Field = {
//     <n:Name> =>? parse_field(n),
// };

fn parse_field<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Field, ParseError<usize, anyhow::Error>> {
    let start_loc = tokens.start_loc();
    let f = parse_field_(parse_name(tokens)?)?;
    let end_loc = tokens.previous_end_loc();
    Ok(spanned(start_loc, end_loc, f))
}

// CopyableVal: CopyableVal = {
//     AccountAddress => CopyableVal::Address(<>),
//     "true" => CopyableVal::Bool(true),
//     "false" => CopyableVal::Bool(false),
//     <i: U64> => CopyableVal::U64(i),
//     <buf: ByteArray> => CopyableVal::ByteArray(buf),
// }

fn parse_copyable_val<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<CopyableVal, ParseError<usize, anyhow::Error>> {
    let start_loc = tokens.start_loc();
    let val = match tokens.peek() {
        Tok::AccountAddressValue => {
            let addr = parse_account_address(tokens)?;
            CopyableVal_::Address(addr)
        }
        Tok::True => {
            tokens.advance()?;
            CopyableVal_::Bool(true)
        }
        Tok::False => {
            tokens.advance()?;
            CopyableVal_::Bool(false)
        }
        Tok::U8Value => {
            let mut s = tokens.content();
            if s.ends_with("u8") {
                s = &s[..s.len() - 2]
            }
            let i = u8::from_str(s).unwrap();
            tokens.advance()?;
            CopyableVal_::U8(i)
        }
        Tok::U64Value => {
            let mut s = tokens.content();
            if s.ends_with("u64") {
                s = &s[..s.len() - 3]
            }
            let i = u64::from_str(s).unwrap();
            tokens.advance()?;
            CopyableVal_::U64(i)
        }
        Tok::U128Value => {
            let mut s = tokens.content();
            if s.ends_with("u128") {
                s = &s[..s.len() - 4]
            }
            let i = u128::from_str(s).unwrap();
            tokens.advance()?;
            CopyableVal_::U128(i)
        }
        Tok::ByteArrayValue => {
            let s = tokens.content();
            let buf = ByteArray::new(hex::decode(&s[2..s.len() - 1]).unwrap_or_else(|_| {
                // The lexer guarantees this, but tracking this knowledge all the way to here is tedious
                unreachable!("The string {:?} is not a valid hex-encoded byte array", s)
            }));
            tokens.advance()?;
            CopyableVal_::ByteArray(buf)
        }
        _ => {
            return Err(ParseError::InvalidToken {
                location: tokens.start_loc(),
            })
        }
    };
    let end_loc = tokens.previous_end_loc();
    Ok(spanned(start_loc, end_loc, val))
}

// Get the precedence of a binary operator. The minimum precedence value
// is 1, and larger values have higher precedence. For tokens that are not
// binary operators, this returns a value of zero so that they will be
// below the minimum value and will mark the end of the binary expression
// for the code in parse_rhs_of_binary_exp.
fn get_precedence(token: &Tok) -> u32 {
    match token {
        // Reserved minimum precedence value is 1 (specified in parse_exp_)
        Tok::EqualEqualGreater => 1,
        Tok::PipePipe => 2,
        Tok::AmpAmp => 3,
        Tok::EqualEqual => 4,
        Tok::ExclaimEqual => 4,
        Tok::Less => 4,
        Tok::Greater => 4,
        Tok::LessEqual => 4,
        Tok::GreaterEqual => 4,
        Tok::Pipe => 5,
        Tok::Caret => 6,
        Tok::Amp => 7,
        Tok::LessLess => 8,
        Tok::GreaterGreater => 8,
        Tok::Plus => 9,
        Tok::Minus => 9,
        Tok::Star => 10,
        Tok::Slash => 10,
        Tok::Percent => 10,
        _ => 0, // anything else is not a binary operator
    }
}

fn parse_exp<'input>(tokens: &mut Lexer<'input>) -> Result<Exp, ParseError<usize, anyhow::Error>> {
    let lhs = parse_unary_exp(tokens)?;
    parse_rhs_of_binary_exp(tokens, lhs, /* min_prec */ 1)
}

fn parse_rhs_of_binary_exp<'input>(
    tokens: &mut Lexer<'input>,
    lhs: Exp,
    min_prec: u32,
) -> Result<Exp, ParseError<usize, anyhow::Error>> {
    let mut result = lhs;
    let mut next_tok_prec = get_precedence(&tokens.peek());

    // Continue parsing binary expressions as long as they have they
    // specified minimum precedence.
    while next_tok_prec >= min_prec {
        let op_token = tokens.peek();
        tokens.advance()?;

        let mut rhs = parse_unary_exp(tokens)?;

        // If the next token is another binary operator with a higher
        // precedence, then recursively parse that expression as the RHS.
        let this_prec = next_tok_prec;
        next_tok_prec = get_precedence(&tokens.peek());
        if this_prec < next_tok_prec {
            rhs = parse_rhs_of_binary_exp(tokens, rhs, this_prec + 1)?;
            next_tok_prec = get_precedence(&tokens.peek());
        }

        let op = match op_token {
            Tok::EqualEqual => BinOp::Eq,
            Tok::ExclaimEqual => BinOp::Neq,
            Tok::Less => BinOp::Lt,
            Tok::Greater => BinOp::Gt,
            Tok::LessEqual => BinOp::Le,
            Tok::GreaterEqual => BinOp::Ge,
            Tok::PipePipe => BinOp::Or,
            Tok::AmpAmp => BinOp::And,
            Tok::Caret => BinOp::Xor,
            Tok::LessLess => BinOp::Shl,
            Tok::GreaterGreater => BinOp::Shr,
            Tok::Pipe => BinOp::BitOr,
            Tok::Amp => BinOp::BitAnd,
            Tok::Plus => BinOp::Add,
            Tok::Minus => BinOp::Sub,
            Tok::Star => BinOp::Mul,
            Tok::Slash => BinOp::Div,
            Tok::Percent => BinOp::Mod,
            _ => panic!("Unexpected token that is not a binary operator"),
        };
        let start_loc = result.span.start();
        let end_loc = tokens.previous_end_loc();
        let e = Exp_::BinopExp(Box::new(result), op, Box::new(rhs));
        result = Spanned {
            span: Span::new(start_loc, ByteIndex(end_loc as u32)),
            value: e,
        };
    }

    Ok(result)
}

// QualifiedFunctionName : FunctionCall = {
//     <f: Builtin> => FunctionCall::Builtin(f),
//     <module_dot_name: DotName> <type_actuals: TypeActuals> =>? { ... }
// }

fn parse_qualified_function_name<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<FunctionCall, ParseError<usize, anyhow::Error>> {
    let start_loc = tokens.start_loc();
    let call = match tokens.peek() {
        Tok::Exists
        | Tok::BorrowGlobal
        | Tok::BorrowGlobalMut
        | Tok::GetTxnSender
        | Tok::MoveFrom
        | Tok::MoveToSender
        | Tok::Freeze
        | Tok::ToU8
        | Tok::ToU64
        | Tok::ToU128 => {
            let f = parse_builtin(tokens)?;
            FunctionCall_::Builtin(f)
        }
        Tok::DotNameValue => {
            let module_dot_name = parse_dot_name(tokens)?;
            let type_actuals = parse_type_actuals(tokens)?;
            let v: Vec<&str> = module_dot_name.split('.').collect();
            assert!(v.len() == 2);
            FunctionCall_::ModuleFunctionCall {
                module: ModuleName::parse(v[0])?,
                name: FunctionName::parse(v[1])?,
                type_actuals,
            }
        }
        _ => {
            return Err(ParseError::InvalidToken {
                location: tokens.start_loc(),
            })
        }
    };
    let end_loc = tokens.previous_end_loc();
    Ok(spanned(start_loc, end_loc, call))
}

// UnaryExp : Exp = {
//     "!" <e: Sp<UnaryExp>> => Exp::UnaryExp(UnaryOp::Not, Box::new(e)),
//     "*" <e: Sp<UnaryExp>> => Exp::Dereference(Box::new(e)),
//     "&mut " <e: Sp<UnaryExp>> "." <f: Field> => { ... },
//     "&" <e: Sp<UnaryExp>> "." <f: Field> => { ... },
//     CallOrTerm,
// }

fn parse_borrow_field_<'input>(
    tokens: &mut Lexer<'input>,
    mutable: bool,
) -> Result<Exp_, ParseError<usize, anyhow::Error>> {
    // This could be either a field borrow (from UnaryExp) or
    // a borrow of a local variable (from Term). In the latter case,
    // only a simple name token is allowed, and it must not be
    // the start of a pack expression.
    let e = if tokens.peek() == Tok::NameValue {
        if tokens.lookahead()? != Tok::LBrace {
            let var = parse_var(tokens)?;
            return Ok(Exp_::BorrowLocal(mutable, var));
        }
        let start_loc = tokens.start_loc();
        let name = parse_name(tokens)?;
        let end_loc = tokens.previous_end_loc();
        let type_actuals: Vec<Type> = vec![];
        spanned(
            start_loc,
            end_loc,
            parse_pack_(tokens, &name, type_actuals)?,
        )
    } else {
        parse_unary_exp(tokens)?
    };
    consume_token(tokens, Tok::Period)?;
    let f = parse_field_(parse_name(tokens)?)?;
    Ok(Exp_::Borrow {
        is_mutable: mutable,
        exp: Box::new(e),
        field: f,
    })
}

fn parse_unary_exp_<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Exp_, ParseError<usize, anyhow::Error>> {
    match tokens.peek() {
        Tok::Exclaim => {
            tokens.advance()?;
            let e = parse_unary_exp(tokens)?;
            Ok(Exp_::UnaryExp(UnaryOp::Not, Box::new(e)))
        }
        Tok::Star => {
            tokens.advance()?;
            let e = parse_unary_exp(tokens)?;
            Ok(Exp_::Dereference(Box::new(e)))
        }
        Tok::AmpMut => {
            tokens.advance()?;
            parse_borrow_field_(tokens, true)
        }
        Tok::Amp => {
            tokens.advance()?;
            parse_borrow_field_(tokens, false)
        }
        _ => parse_call_or_term_(tokens),
    }
}

fn parse_unary_exp<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Exp, ParseError<usize, anyhow::Error>> {
    let start_loc = tokens.start_loc();
    let e = parse_unary_exp_(tokens)?;
    let end_loc = tokens.previous_end_loc();
    Ok(spanned(start_loc, end_loc, e))
}

// Call: Exp = {
//     <f: Sp<QualifiedFunctionName>> <exp: Sp<CallOrTerm>> => Exp::FunctionCall(f, Box::new(exp)),
// }

fn parse_call<'input>(tokens: &mut Lexer<'input>) -> Result<Exp, ParseError<usize, anyhow::Error>> {
    let start_loc = tokens.start_loc();
    let f = parse_qualified_function_name(tokens)?;
    let exp = parse_call_or_term(tokens)?;
    let end_loc = tokens.previous_end_loc();
    Ok(spanned(
        start_loc,
        end_loc,
        Exp_::FunctionCall(f, Box::new(exp)),
    ))
}

// CallOrTerm: Exp = {
//     <f: Sp<QualifiedFunctionName>> <exp: Sp<CallOrTerm>> => Exp::FunctionCall(f, Box::new(exp)),
//     Term,
// }

fn parse_call_or_term_<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Exp_, ParseError<usize, anyhow::Error>> {
    match tokens.peek() {
        Tok::Exists
        | Tok::BorrowGlobal
        | Tok::BorrowGlobalMut
        | Tok::GetTxnSender
        | Tok::MoveFrom
        | Tok::MoveToSender
        | Tok::Freeze
        | Tok::DotNameValue
        | Tok::ToU8
        | Tok::ToU64
        | Tok::ToU128 => {
            let f = parse_qualified_function_name(tokens)?;
            let exp = parse_call_or_term(tokens)?;
            Ok(Exp_::FunctionCall(f, Box::new(exp)))
        }
        _ => parse_term_(tokens),
    }
}

fn parse_call_or_term<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Exp, ParseError<usize, anyhow::Error>> {
    let start_loc = tokens.start_loc();
    let v = parse_call_or_term_(tokens)?;
    let end_loc = tokens.previous_end_loc();
    Ok(spanned(start_loc, end_loc, v))
}

// FieldExp: (Field_, Exp_) = {
//     <f: Sp<Field>> ":" <e: Sp<Exp>> => (f, e)
// }

fn parse_field_exp<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<(Field, Exp), ParseError<usize, anyhow::Error>> {
    let f = parse_field(tokens)?;
    consume_token(tokens, Tok::Colon)?;
    let e = parse_exp(tokens)?;
    Ok((f, e))
}

// Term: Exp = {
//     "move(" <v: Sp<Var>> ")" => Exp::Move(v),
//     "copy(" <v: Sp<Var>> ")" => Exp::Copy(v),
//     "&mut " <v: Sp<Var>> => Exp::BorrowLocal(true, v),
//     "&" <v: Sp<Var>> => Exp::BorrowLocal(false, v),
//     Sp<CopyableVal> => Exp::Value(<>),
//     <name_and_type_actuals: NameAndTypeActuals> "{" <fs:Comma<FieldExp>> "}" =>? { ... },
//     "(" <exps: Comma<Sp<Exp>>> ")" => Exp::ExprList(exps),
// }

fn parse_pack_<'input>(
    tokens: &mut Lexer<'input>,
    name: &str,
    type_actuals: Vec<Type>,
) -> Result<Exp_, ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::LBrace)?;
    let fs = parse_comma_list(tokens, &[Tok::RBrace], parse_field_exp, true)?;
    consume_token(tokens, Tok::RBrace)?;
    Ok(Exp_::Pack(
        StructName::parse(name)?,
        type_actuals,
        fs.into_iter().collect::<Vec<_>>(),
    ))
}

fn parse_term_<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Exp_, ParseError<usize, anyhow::Error>> {
    match tokens.peek() {
        Tok::Move => {
            tokens.advance()?;
            let v = parse_var(tokens)?;
            consume_token(tokens, Tok::RParen)?;
            Ok(Exp_::Move(v))
        }
        Tok::Copy => {
            tokens.advance()?;
            let v = parse_var(tokens)?;
            consume_token(tokens, Tok::RParen)?;
            Ok(Exp_::Copy(v))
        }
        Tok::AmpMut => {
            tokens.advance()?;
            let v = parse_var(tokens)?;
            Ok(Exp_::BorrowLocal(true, v))
        }
        Tok::Amp => {
            tokens.advance()?;
            let v = parse_var(tokens)?;
            Ok(Exp_::BorrowLocal(false, v))
        }
        Tok::AccountAddressValue
        | Tok::True
        | Tok::False
        | Tok::U8Value
        | Tok::U64Value
        | Tok::U128Value
        | Tok::ByteArrayValue => Ok(Exp_::Value(parse_copyable_val(tokens)?)),
        Tok::NameValue | Tok::NameBeginTyValue => {
            let (name, type_actuals) = parse_name_and_type_actuals(tokens)?;
            parse_pack_(tokens, &name, type_actuals)
        }
        Tok::LParen => {
            tokens.advance()?;
            let exps = parse_comma_list(tokens, &[Tok::RParen], parse_exp, true)?;
            consume_token(tokens, Tok::RParen)?;
            Ok(Exp_::ExprList(exps))
        }
        _ => Err(ParseError::InvalidToken {
            location: tokens.start_loc(),
        }),
    }
}

// StructName: StructName = {
//     <n: Name> =>? StructName::parse(n),
// }

fn parse_struct_name<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<StructName, ParseError<usize, anyhow::Error>> {
    Ok(StructName::parse(parse_name(tokens)?)?)
}

// QualifiedStructIdent : QualifiedStructIdent = {
//     <module_dot_struct: DotName> =>? { ... }
// }

fn parse_qualified_struct_ident<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<QualifiedStructIdent, ParseError<usize, anyhow::Error>> {
    let module_dot_struct = parse_dot_name(tokens)?;
    let v: Vec<&str> = module_dot_struct.split('.').collect();
    assert!(v.len() == 2);
    let m: ModuleName = ModuleName::parse(v[0])?;
    let n: StructName = StructName::parse(v[1])?;
    Ok(QualifiedStructIdent::new(m, n))
}

// ModuleName: ModuleName = {
//     <n: Name> =>? ModuleName::parse(n),
// }

fn parse_module_name<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<ModuleName, ParseError<usize, anyhow::Error>> {
    Ok(ModuleName::parse(parse_name(tokens)?)?)
}

fn consume_end_of_generics<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<(), ParseError<usize, anyhow::Error>> {
    match tokens.peek() {
        Tok::Greater => tokens.advance(),
        Tok::GreaterGreater => {
            tokens.replace_token(Tok::Greater, 1)?;
            tokens.advance()?;
            Ok(())
        }
        _ => Err(ParseError::InvalidToken {
            location: tokens.start_loc(),
        }),
    }
}

// Builtin: Builtin = {
//     "exists<" <name_and_type_actuals: NameAndTypeActuals> ">" =>? { ... },
//     "borrow_global<" <name_and_type_actuals: NameAndTypeActuals> ">" =>? { ... },
//     "borrow_global_mut<" <name_and_type_actuals: NameAndTypeActuals> ">" =>? { ... },
//     "get_txn_sender" => Builtin::GetTxnSender,
//     "move_from<" <name_and_type_actuals: NameAndTypeActuals> ">" =>? { ... },
//     "move_to_sender<" <name_and_type_actuals: NameAndTypeActuals> ">" =>? { ...},
//     "freeze" => Builtin::Freeze,
// }

fn parse_builtin<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Builtin, ParseError<usize, anyhow::Error>> {
    match tokens.peek() {
        Tok::Exists => {
            tokens.advance()?;
            let (name, type_actuals) = parse_name_and_type_actuals(tokens)?;
            consume_end_of_generics(tokens)?;
            Ok(Builtin::Exists(StructName::parse(name)?, type_actuals))
        }
        Tok::BorrowGlobal => {
            tokens.advance()?;
            let (name, type_actuals) = parse_name_and_type_actuals(tokens)?;
            consume_end_of_generics(tokens)?;
            Ok(Builtin::BorrowGlobal(
                false,
                StructName::parse(name)?,
                type_actuals,
            ))
        }
        Tok::BorrowGlobalMut => {
            tokens.advance()?;
            let (name, type_actuals) = parse_name_and_type_actuals(tokens)?;
            consume_end_of_generics(tokens)?;
            Ok(Builtin::BorrowGlobal(
                true,
                StructName::parse(name)?,
                type_actuals,
            ))
        }
        Tok::GetTxnSender => {
            tokens.advance()?;
            Ok(Builtin::GetTxnSender)
        }
        Tok::MoveFrom => {
            tokens.advance()?;
            let (name, type_actuals) = parse_name_and_type_actuals(tokens)?;
            consume_end_of_generics(tokens)?;
            Ok(Builtin::MoveFrom(StructName::parse(name)?, type_actuals))
        }
        Tok::MoveToSender => {
            tokens.advance()?;
            let (name, type_actuals) = parse_name_and_type_actuals(tokens)?;
            consume_end_of_generics(tokens)?;
            Ok(Builtin::MoveToSender(
                StructName::parse(name)?,
                type_actuals,
            ))
        }
        Tok::Freeze => {
            tokens.advance()?;
            Ok(Builtin::Freeze)
        }
        Tok::ToU8 => {
            tokens.advance()?;
            Ok(Builtin::ToU8)
        }
        Tok::ToU64 => {
            tokens.advance()?;
            Ok(Builtin::ToU64)
        }
        Tok::ToU128 => {
            tokens.advance()?;
            Ok(Builtin::ToU128)
        }
        _ => Err(ParseError::InvalidToken {
            location: tokens.start_loc(),
        }),
    }
}

// LValue: LValue = {
//     <l:Sp<Var>> => LValue::Var(l),
//     "*" <e: Sp<Exp>> => LValue::Mutate(e),
//     "_" => LValue::Pop,
// }

fn parse_lvalue_<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<LValue_, ParseError<usize, anyhow::Error>> {
    match tokens.peek() {
        Tok::NameValue => {
            let l = parse_var(tokens)?;
            Ok(LValue_::Var(l))
        }
        Tok::Star => {
            tokens.advance()?;
            let e = parse_exp(tokens)?;
            Ok(LValue_::Mutate(e))
        }
        Tok::Underscore => {
            tokens.advance()?;
            Ok(LValue_::Pop)
        }
        _ => Err(ParseError::InvalidToken {
            location: tokens.start_loc(),
        }),
    }
}

fn parse_lvalue<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<LValue, ParseError<usize, anyhow::Error>> {
    let start_loc = tokens.start_loc();
    let lv = parse_lvalue_(tokens)?;
    let end_loc = tokens.previous_end_loc();
    Ok(spanned(start_loc, end_loc, lv))
}

// FieldBindings: (Field_, Var_) = {
//     <f: Sp<Field>> ":" <v: Sp<Var>> => (f, v),
//     <f: Sp<Field>> => { ... }
// }

fn parse_field_bindings<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<(Field, Var), ParseError<usize, anyhow::Error>> {
    let f = parse_field(tokens)?;
    if tokens.peek() == Tok::Colon {
        tokens.advance()?; // consume the colon
        let v = parse_var(tokens)?;
        Ok((f, v))
    } else {
        Ok((
            f.clone(),
            Spanned {
                span: f.span,
                value: Var_::new(f.value.name().into()),
            },
        ))
    }
}

// pub Cmd : Cmd = {
//     <lvalues: Comma<Sp<LValue>>> "=" <e: Sp<Exp>> => Cmd::Assign(lvalues, e),
//     <name_and_type_actuals: NameAndTypeActuals> "{" <bindings: Comma<FieldBindings>> "}" "=" <e: Sp<Exp>> =>? { ... },
//     "abort" <err: Sp<Exp>?> => { ... },
//     "return" <v: Comma<Sp<Exp>>> => Cmd::Return(Box::new(Spanned::no_loc(Exp::ExprList(v)))),
//     "continue" => Cmd::Continue,
//     "break" => Cmd::Break,
//     <Sp<Call>> => Cmd::Exp(Box::new(<>)),
//     "(" <Comma<Sp<Exp>>> ")" => Cmd::Exp(Box::new(Spanned::no_loc(Exp::ExprList(<>)))),
// }

fn parse_assign_<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Cmd_, ParseError<usize, anyhow::Error>> {
    let lvalues = parse_comma_list(tokens, &[Tok::Equal], parse_lvalue, false)?;
    if lvalues.is_empty() {
        return Err(ParseError::InvalidToken {
            location: tokens.start_loc(),
        });
    }
    consume_token(tokens, Tok::Equal)?;
    let e = parse_exp(tokens)?;
    Ok(Cmd_::Assign(lvalues, e))
}

fn parse_unpack_<'input>(
    tokens: &mut Lexer<'input>,
    name: &str,
    type_actuals: Vec<Type>,
) -> Result<Cmd_, ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::LBrace)?;
    let bindings = parse_comma_list(tokens, &[Tok::RBrace], parse_field_bindings, true)?;
    consume_token(tokens, Tok::RBrace)?;
    consume_token(tokens, Tok::Equal)?;
    let e = parse_exp(tokens)?;
    Ok(Cmd_::Unpack(
        StructName::parse(name)?,
        type_actuals,
        bindings.into_iter().collect(),
        Box::new(e),
    ))
}

fn parse_cmd_<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Cmd_, ParseError<usize, anyhow::Error>> {
    match tokens.peek() {
        Tok::NameValue => {
            // This could be either an LValue for an assignment or
            // NameAndTypeActuals (with no type_actuals) for an unpack.
            if tokens.lookahead()? == Tok::LBrace {
                let name = parse_name(tokens)?;
                parse_unpack_(tokens, &name, vec![])
            } else {
                parse_assign_(tokens)
            }
        }
        Tok::Star | Tok::Underscore => parse_assign_(tokens),
        Tok::NameBeginTyValue => {
            let (name, tys) = parse_name_and_type_actuals(tokens)?;
            parse_unpack_(tokens, &name, tys)
        }
        Tok::Abort => {
            tokens.advance()?;
            let val = if tokens.peek() == Tok::Semicolon {
                None
            } else {
                Some(Box::new(parse_exp(tokens)?))
            };
            Ok(Cmd_::Abort(val))
        }
        Tok::Return => {
            tokens.advance()?;
            let v = parse_comma_list(tokens, &[Tok::Semicolon], parse_exp, true)?;
            Ok(Cmd_::Return(Box::new(Spanned::no_loc(Exp_::ExprList(v)))))
        }
        Tok::Continue => {
            tokens.advance()?;
            Ok(Cmd_::Continue)
        }
        Tok::Break => {
            tokens.advance()?;
            Ok(Cmd_::Break)
        }
        Tok::Exists
        | Tok::BorrowGlobal
        | Tok::BorrowGlobalMut
        | Tok::GetTxnSender
        | Tok::MoveFrom
        | Tok::MoveToSender
        | Tok::Freeze
        | Tok::DotNameValue
        | Tok::ToU8
        | Tok::ToU64
        | Tok::ToU128 => Ok(Cmd_::Exp(Box::new(parse_call(tokens)?))),
        Tok::LParen => {
            tokens.advance()?;
            let v = parse_comma_list(tokens, &[Tok::RParen], parse_exp, true)?;
            consume_token(tokens, Tok::RParen)?;
            Ok(Cmd_::Exp(Box::new(Spanned::no_loc(Exp_::ExprList(v)))))
        }
        _ => Err(ParseError::InvalidToken {
            location: tokens.start_loc(),
        }),
    }
}

// Statement : Statement = {
//     <cmd: Cmd_> ";" => Statement::CommandStatement(cmd),
//     "assert(" <e: Sp<Exp>> "," <err: Sp<Exp>> ")" => { ... },
//     <IfStatement>,
//     <WhileStatement>,
//     <LoopStatement>,
//     ";" => Statement::EmptyStatement,
// }

fn parse_statement<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Statement, ParseError<usize, anyhow::Error>> {
    match tokens.peek() {
        Tok::Assert => {
            tokens.advance()?;
            let e = parse_exp(tokens)?;
            consume_token(tokens, Tok::Comma)?;
            let err = parse_exp(tokens)?;
            consume_token(tokens, Tok::RParen)?;
            let cond = {
                let span = e.span;
                Spanned {
                    span,
                    value: Exp_::UnaryExp(UnaryOp::Not, Box::new(e)),
                }
            };
            let span = err.span;
            let stmt = {
                Statement::CommandStatement(Spanned {
                    span,
                    value: Cmd_::Abort(Some(Box::new(err))),
                })
            };
            Ok(Statement::IfElseStatement(IfElse::if_block(
                cond,
                Spanned {
                    span,
                    value: Block_::new(vec![stmt]),
                },
            )))
        }
        Tok::If => parse_if_statement(tokens),
        Tok::While => parse_while_statement(tokens),
        Tok::Loop => parse_loop_statement(tokens),
        Tok::Semicolon => {
            tokens.advance()?;
            Ok(Statement::EmptyStatement)
        }
        _ => {
            // Anything else should be parsed as a Cmd...
            let start_loc = tokens.start_loc();
            let c = parse_cmd_(tokens)?;
            let end_loc = tokens.previous_end_loc();
            let cmd = spanned(start_loc, end_loc, c);
            consume_token(tokens, Tok::Semicolon)?;
            Ok(Statement::CommandStatement(cmd))
        }
    }
}

// IfStatement : Statement = {
//     "if" "(" <cond: Sp<Exp>> ")" <block: Sp<Block>> => { ... }
//     "if" "(" <cond: Sp<Exp>> ")" <if_block: Sp<Block>> "else" <else_block: Sp<Block>> => { ... }
// }

fn parse_if_statement<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Statement, ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::If)?;
    consume_token(tokens, Tok::LParen)?;
    let cond = parse_exp(tokens)?;
    consume_token(tokens, Tok::RParen)?;
    let if_block = parse_block(tokens)?;
    if tokens.peek() == Tok::Else {
        tokens.advance()?;
        let else_block = parse_block(tokens)?;
        Ok(Statement::IfElseStatement(IfElse::if_else(
            cond, if_block, else_block,
        )))
    } else {
        Ok(Statement::IfElseStatement(IfElse::if_block(cond, if_block)))
    }
}

// WhileStatement : Statement = {
//     "while" "(" <cond: Sp<Exp>> ")" <block: Sp<Block>> => { ... }
// }

fn parse_while_statement<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Statement, ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::While)?;
    consume_token(tokens, Tok::LParen)?;
    let cond = parse_exp(tokens)?;
    consume_token(tokens, Tok::RParen)?;
    let block = parse_block(tokens)?;
    Ok(Statement::WhileStatement(While { cond, block }))
}

// LoopStatement : Statement = {
//     "loop" <block: Sp<Block>> => { ... }
// }

fn parse_loop_statement<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Statement, ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::Loop)?;
    let block = parse_block(tokens)?;
    Ok(Statement::LoopStatement(Loop { block }))
}

// Statements : Vec<Statement> = {
//     <Statement*>
// }

fn parse_statements<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Vec<Statement>, ParseError<usize, anyhow::Error>> {
    let mut stmts: Vec<Statement> = vec![];
    // The Statements non-terminal in the grammar is always followed by a
    // closing brace, so continue parsing until we find one of those.
    while tokens.peek() != Tok::RBrace {
        stmts.push(parse_statement(tokens)?);
    }
    Ok(stmts)
}

// Block : Block = {
//     "{" <stmts: Statements> "}" => Block::new(stmts)
// }

fn parse_block<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Block, ParseError<usize, anyhow::Error>> {
    let start_loc = tokens.start_loc();
    consume_token(tokens, Tok::LBrace)?;
    let stmts = parse_statements(tokens)?;
    consume_token(tokens, Tok::RBrace)?;
    let end_loc = tokens.previous_end_loc();
    Ok(spanned(start_loc, end_loc, Block_::new(stmts)))
}

// Declaration: (Var_, Type) = {
//   "let" <v: Sp<Var>> ":" <t: Type> ";" => (v, t),
// }

fn parse_declaration<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<(Var, Type), ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::Let)?;
    let v = parse_var(tokens)?;
    consume_token(tokens, Tok::Colon)?;
    let t = parse_type(tokens)?;
    consume_token(tokens, Tok::Semicolon)?;
    Ok((v, t))
}

// Declarations: Vec<(Var_, Type)> = {
//     <Declaration*>
// }

fn parse_declarations<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Vec<(Var, Type)>, ParseError<usize, anyhow::Error>> {
    let mut decls: Vec<(Var, Type)> = vec![];
    // Declarations always begin with the "let" token so continue parsing
    // them until we hit something else.
    while tokens.peek() == Tok::Let {
        decls.push(parse_declaration(tokens)?);
    }
    Ok(decls)
}

// FunctionBlock: (Vec<(Var_, Type)>, Block) = {
//     "{" <locals: Declarations> <stmts: Statements> "}" => (locals, Block::new(stmts))
// }

fn parse_function_block_<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<(Vec<(Var, Type)>, Block_), ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::LBrace)?;
    let locals = parse_declarations(tokens)?;
    let stmts = parse_statements(tokens)?;
    consume_token(tokens, Tok::RBrace)?;
    Ok((locals, Block_::new(stmts)))
}

// Kind: Kind = {
//     "resource" => Kind::Resource,
//     "unrestricted" => Kind::Unrestricted,
// }

fn parse_kind<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Kind, ParseError<usize, anyhow::Error>> {
    let k = match tokens.peek() {
        Tok::Resource => Kind::Resource,
        Tok::Unrestricted => Kind::Unrestricted,
        _ => {
            return Err(ParseError::InvalidToken {
                location: tokens.start_loc(),
            })
        }
    };
    tokens.advance()?;
    Ok(k)
}

// Type: Type = {
//     "address" => Type::Address,
//     "u64" => Type::U64,
//     "bool" => Type::Bool,
//     "bytearray" => Type::ByteArray,
//     <s: QualifiedStructIdent> <tys: TypeActuals> => Type::Struct(s, tys),
//     "&" <t: Type> => Type::Reference(false, Box::new(t)),
//     "&mut " <t: Type> => Type::Reference(true, Box::new(t)),
//     <n: Name> =>? Ok(Type::TypeParameter(TypeVar::parse(n)?)),
// }

fn parse_type<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Type, ParseError<usize, anyhow::Error>> {
    let t = match tokens.peek() {
        Tok::Address => {
            tokens.advance()?;
            Type::Address
        }
        Tok::U8 => {
            tokens.advance()?;
            Type::U8
        }
        Tok::U64 => {
            tokens.advance()?;
            Type::U64
        }
        Tok::U128 => {
            tokens.advance()?;
            Type::U128
        }
        Tok::Bool => {
            tokens.advance()?;
            Type::Bool
        }
        Tok::Bytearray => {
            tokens.advance()?;
            Type::ByteArray
        }
        Tok::DotNameValue => {
            let s = parse_qualified_struct_ident(tokens)?;
            let tys = parse_type_actuals(tokens)?;
            Type::Struct(s, tys)
        }
        Tok::Amp => {
            tokens.advance()?;
            Type::Reference(false, Box::new(parse_type(tokens)?))
        }
        Tok::AmpMut => {
            tokens.advance()?;
            Type::Reference(true, Box::new(parse_type(tokens)?))
        }
        Tok::NameValue => Type::TypeParameter(TypeVar_::parse(parse_name(tokens)?)?),
        _ => {
            return Err(ParseError::InvalidToken {
                location: tokens.start_loc(),
            })
        }
    };
    Ok(t)
}

// TypeVar: TypeVar = {
//     <n: Name> =>? TypeVar::parse(n),
// }
// TypeVar_ = Sp<TypeVar>;

fn parse_type_var<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<TypeVar, ParseError<usize, anyhow::Error>> {
    let start_loc = tokens.start_loc();
    let type_var = TypeVar_::parse(parse_name(tokens)?)?;
    let end_loc = tokens.previous_end_loc();
    Ok(spanned(start_loc, end_loc, type_var))
}

// TypeFormal: (TypeVar_, Kind) = {
//     <type_var: Sp<TypeVar>> <k: (":" <Kind>)?> =>? {
// }

fn parse_type_formal<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<(TypeVar, Kind), ParseError<usize, anyhow::Error>> {
    let type_var = parse_type_var(tokens)?;
    if tokens.peek() == Tok::Colon {
        tokens.advance()?; // consume the ":"
        let k = parse_kind(tokens)?;
        Ok((type_var, k))
    } else {
        Ok((type_var, Kind::All))
    }
}

// TypeActuals: Vec<Type> = {
//     <tys: ("<" <Comma<Type>> ">")?> => { ... }
// }

fn parse_type_actuals<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Vec<Type>, ParseError<usize, anyhow::Error>> {
    let tys = if tokens.peek() == Tok::Less {
        tokens.advance()?; // consume the "<"
        let list = parse_comma_list(tokens, &[Tok::Greater], parse_type, true)?;
        consume_token(tokens, Tok::Greater)?;
        list
    } else {
        vec![]
    };
    Ok(tys)
}

// NameAndTypeFormals: (String, Vec<(TypeVar_, Kind)>) = {
//     <n: NameBeginTy> <k: Comma<TypeFormal>> ">" => (n, k),
//     <n: Name> => (n, vec![]),
// }

fn parse_name_and_type_formals<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<(String, Vec<(TypeVar, Kind)>), ParseError<usize, anyhow::Error>> {
    let mut has_types = false;
    let n = if tokens.peek() == Tok::NameBeginTyValue {
        has_types = true;
        parse_name_begin_ty(tokens)?
    } else {
        parse_name(tokens)?
    };
    let k = if has_types {
        let list = parse_comma_list(tokens, &[Tok::Greater], parse_type_formal, true)?;
        consume_token(tokens, Tok::Greater)?;
        list
    } else {
        vec![]
    };
    Ok((n, k))
}

// NameAndTypeActuals: (String, Vec<Type>) = {
//     <n: NameBeginTy> <tys: Comma<Type>> ">" => (n, tys),
//     <n: Name> => (n, vec![]),
// }

fn parse_name_and_type_actuals<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<(String, Vec<Type>), ParseError<usize, anyhow::Error>> {
    let mut has_types = false;
    let n = if tokens.peek() == Tok::NameBeginTyValue {
        has_types = true;
        parse_name_begin_ty(tokens)?
    } else {
        parse_name(tokens)?
    };
    let tys = if has_types {
        let list = parse_comma_list(tokens, &[Tok::Greater], parse_type, true)?;
        consume_token(tokens, Tok::Greater)?;
        list
    } else {
        vec![]
    };
    Ok((n, tys))
}

// ArgDecl : (Var_, Type) = {
//     <v: Sp<Var>> ":" <t: Type> => (v, t)
// }

fn parse_arg_decl<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<(Var, Type), ParseError<usize, anyhow::Error>> {
    let v = parse_var(tokens)?;
    consume_token(tokens, Tok::Colon)?;
    let t = parse_type(tokens)?;
    Ok((v, t))
}

// ReturnType: Vec<Type> = {
//     ":" <t: Type> <v: ("*" <Type>)*> => { ... }
// }

fn parse_return_type<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Vec<Type>, ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::Colon)?;
    let t = parse_type(tokens)?;
    let mut v = vec![t];
    while tokens.peek() == Tok::Star {
        tokens.advance()?;
        v.push(parse_type(tokens)?);
    }
    Ok(v)
}

// AcquireList: Vec<StructName> = {
//     "acquires" <s: StructName> <al: ("," <StructName>)*> => { ... }
// }

fn parse_acquire_list<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Vec<StructName>, ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::Acquires)?;
    let s = parse_struct_name(tokens)?;
    let mut al = vec![s];
    while tokens.peek() == Tok::Comma {
        tokens.advance()?;
        al.push(parse_struct_name(tokens)?);
    }
    Ok(al)
}

//// Spec language parsing ////

// parses Name '.' Name and returns pair of strings.
fn spec_parse_dot_name<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<(String, String), ParseError<usize, anyhow::Error>> {
    let name1 = parse_name(tokens)?;
    consume_token(tokens, Tok::Period)?;
    let name2 = parse_name(tokens)?;
    Ok((name1, name2))
}

fn spec_parse_qualified_struct_ident<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<QualifiedStructIdent, ParseError<usize, anyhow::Error>> {
    let (m_string, n_string) = spec_parse_dot_name(tokens)?;
    let m: ModuleName = ModuleName::parse(m_string)?;
    let n: StructName = StructName::parse(n_string)?;
    Ok(QualifiedStructIdent::new(m, n))
}

fn parse_storage_location<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<StorageLocation, ParseError<usize, anyhow::Error>> {
    let base = match tokens.peek() {
        Tok::SpecReturn => {
            // RET(i)
            tokens.advance()?;
            let i = {
                if tokens.peek() == Tok::LParen {
                    consume_token(tokens, Tok::LParen)?;
                    let i = u8::from_str(tokens.content()).unwrap();
                    consume_token(tokens, Tok::U64Value)?;
                    consume_token(tokens, Tok::RParen)?;
                    i
                } else {
                    // RET without brackets; use RET(0)
                    0
                }
            };

            StorageLocation::Ret(i)
        }
        Tok::TxnSender => {
            tokens.advance()?;
            StorageLocation::TxnSenderAddress
        }
        Tok::AccountAddressValue => StorageLocation::Address(parse_account_address(tokens)?),
        Tok::Global => {
            consume_token(tokens, Tok::Global)?;
            consume_token(tokens, Tok::Less)?;
            let type_ = spec_parse_qualified_struct_ident(tokens)?;
            let type_actuals = parse_type_actuals(tokens)?;
            consume_token(tokens, Tok::Greater)?;
            consume_token(tokens, Tok::LParen)?;
            let address = Box::new(parse_storage_location(tokens)?);
            consume_token(tokens, Tok::RParen)?;
            StorageLocation::GlobalResource {
                type_,
                type_actuals,
                address,
            }
        }
        _ => StorageLocation::Formal(parse_name(tokens)?),
    };

    // parsed the storage location base. now parse its fields (if any)
    let mut fields = vec![];
    while tokens.peek() == Tok::Period {
        tokens.advance()?;
        fields.push(parse_field(tokens)?.value);
    }
    if fields.is_empty() {
        Ok(base)
    } else {
        Ok(StorageLocation::AccessPath {
            base: Box::new(base),
            fields,
        })
    }
}

fn parse_unary_spec_exp<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<SpecExp, ParseError<usize, anyhow::Error>> {
    Ok(match tokens.peek() {
        Tok::AccountAddressValue
        | Tok::True
        | Tok::False
        | Tok::U8Value
        | Tok::U64Value
        | Tok::U128Value
        | Tok::ByteArrayValue => SpecExp::Constant(parse_copyable_val(tokens)?.value),
        Tok::GlobalExists => {
            consume_token(tokens, Tok::GlobalExists)?;
            consume_token(tokens, Tok::Less)?;
            let type_ = spec_parse_qualified_struct_ident(tokens)?;
            let type_actuals = parse_type_actuals(tokens)?;
            consume_token(tokens, Tok::Greater)?;
            consume_token(tokens, Tok::LParen)?;
            let address = parse_storage_location(tokens)?;
            consume_token(tokens, Tok::RParen)?;
            SpecExp::GlobalExists {
                type_,
                type_actuals,
                address,
            }
        }
        Tok::Star => {
            tokens.advance()?;
            let stloc = parse_storage_location(tokens)?;
            SpecExp::Dereference(stloc)
        }
        Tok::Amp => {
            tokens.advance()?;
            let stloc = parse_storage_location(tokens)?;
            SpecExp::Reference(stloc)
        }
        Tok::Exclaim => {
            tokens.advance()?;
            let exp = parse_unary_spec_exp(tokens)?;
            SpecExp::Not(Box::new(exp))
        }
        Tok::Old => {
            tokens.advance()?;
            consume_token(tokens, Tok::LParen)?;
            let exp = parse_spec_exp(tokens)?;
            consume_token(tokens, Tok::RParen)?;
            SpecExp::Old(Box::new(exp))
        }
        Tok::NameValue => {
            let next = tokens.lookahead();
            if next.is_err() || next.unwrap() != Tok::LParen {
                SpecExp::StorageLocation(parse_storage_location(tokens)?)
            } else {
                let name = parse_name(tokens)?;
                let mut args = vec![];
                consume_token(tokens, Tok::LParen)?;
                while tokens.peek() != Tok::RParen {
                    let exp = parse_spec_exp(tokens)?;
                    args.push(exp);
                    if tokens.peek() != Tok::Comma {
                        break;
                    }
                    consume_token(tokens, Tok::Comma)?;
                }
                consume_token(tokens, Tok::RParen)?;
                SpecExp::Call(name, args)
            }
        }
        _ => SpecExp::StorageLocation(parse_storage_location(tokens)?),
    })
}

fn parse_rhs_of_spec_exp<'input>(
    tokens: &mut Lexer<'input>,
    lhs: SpecExp,
    min_prec: u32,
) -> Result<SpecExp, ParseError<usize, anyhow::Error>> {
    let mut result = lhs;
    let mut next_tok_prec = get_precedence(&tokens.peek());

    // Continue parsing binary expressions as long as they have they
    // specified minimum precedence.
    while next_tok_prec >= min_prec {
        let op_token = tokens.peek();
        tokens.advance()?;

        let mut rhs = parse_unary_spec_exp(tokens)?;

        // If the next token is another binary operator with a higher
        // precedence, then recursively parse that expression as the RHS.
        let this_prec = next_tok_prec;
        next_tok_prec = get_precedence(&tokens.peek());
        if this_prec < next_tok_prec {
            rhs = parse_rhs_of_spec_exp(tokens, rhs, this_prec + 1)?;
            next_tok_prec = get_precedence(&tokens.peek());
        }
        if op_token == Tok::EqualEqualGreater {
            // Syntactic sugar: p ==> c ~~~> !p || c
            result = SpecExp::Binop(
                Box::new(SpecExp::Not(Box::new(result))),
                BinOp::Or,
                Box::new(rhs),
            );
        } else {
            let op = match op_token {
                Tok::EqualEqual => BinOp::Eq,
                Tok::ExclaimEqual => BinOp::Neq,
                Tok::Less => BinOp::Lt,
                Tok::Greater => BinOp::Gt,
                Tok::LessEqual => BinOp::Le,
                Tok::GreaterEqual => BinOp::Ge,
                Tok::PipePipe => BinOp::Or,
                Tok::AmpAmp => BinOp::And,
                Tok::Caret => BinOp::Xor,
                Tok::Pipe => BinOp::BitOr,
                Tok::Amp => BinOp::BitAnd,
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => panic!("Unexpected token that is not a binary operator"),
            };
            result = SpecExp::Binop(Box::new(result), op, Box::new(rhs))
        }
    }
    Ok(result)
}

fn parse_spec_exp<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<SpecExp, ParseError<usize, anyhow::Error>> {
    let lhs = parse_unary_spec_exp(tokens)?;
    parse_rhs_of_spec_exp(tokens, lhs, /* min_prec */ 1)
}

// Parse a top-level requires, ensures, aborts_if, or succeeds_if spec
// in a function decl.  This has to set the lexer into "spec_mode" to
// return names without eating trailing punctuation such as '<' or '.'.
// That is needed to parse paths with dots separating field names.
fn parse_spec_condition<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Condition_, ParseError<usize, anyhow::Error>> {
    // Set lexer to read names without trailing punctuation
    tokens.spec_mode = true;
    let retval = Ok(match tokens.peek() {
        Tok::AbortsIf => {
            tokens.advance()?;
            Condition_::AbortsIf(parse_spec_exp(tokens)?)
        }
        Tok::Ensures => {
            tokens.advance()?;
            Condition_::Ensures(parse_spec_exp(tokens)?)
        }
        Tok::Requires => {
            tokens.advance()?;
            Condition_::Requires(parse_spec_exp(tokens)?)
        }
        Tok::SucceedsIf => {
            tokens.advance()?;
            Condition_::SucceedsIf(parse_spec_exp(tokens)?)
        }
        _ => {
            tokens.spec_mode = false;
            return Err(ParseError::InvalidToken {
                location: tokens.start_loc(),
            });
        }
    });
    tokens.spec_mode = false;
    retval
}

fn parse_invariant<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Invariant, ParseError<usize, anyhow::Error>> {
    // Set lexer to read names without trailing punctuation
    tokens.spec_mode = true;
    let start = tokens.start_loc();
    let result = parse_invariant_(tokens);
    tokens.spec_mode = false;
    Ok(spanned(start, tokens.previous_end_loc(), result?))
}

fn parse_invariant_<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Invariant_, ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::Invariant)?;
    let modifier = if tokens.peek() == Tok::LBrace {
        tokens.advance()?;
        let s = parse_name(tokens)?;
        consume_token(tokens, Tok::RBrace)?;
        s
    } else {
        String::new()
    };
    let condition = parse_spec_exp(tokens)?;
    Ok(Invariant_ {
        modifier,
        condition,
    })
}

fn parse_synthetic<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<SyntheticDefinition, ParseError<usize, anyhow::Error>> {
    // Set lexer to read names without trailing punctuation
    tokens.spec_mode = true;
    let start = tokens.start_loc();
    let result = parse_synthetic_(tokens);
    tokens.spec_mode = false;
    Ok(spanned(start, tokens.previous_end_loc(), result?))
}

fn parse_synthetic_<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<SyntheticDefinition_, ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::Synthetic)?;
    let name = Identifier::from(parse_field(tokens)?.value.name());
    consume_token(tokens, Tok::Colon)?;
    let type_ = parse_type(tokens)?;
    consume_token(tokens, Tok::Semicolon)?;
    Ok(SyntheticDefinition_ { name, type_ })
}

// FunctionDecl : (FunctionName, Function_) = {
//   <f: Sp<MoveFunctionDecl>> => (f.value.0, Spanned { span: f.span, value: f.value.1 }),
//   <f: Sp<NativeFunctionDecl>> => (f.value.0, Spanned { span: f.span, value: f.value.1 }),
// }

// MoveFunctionDecl : (FunctionName, Function) = {
//     <p: Public?> <name_and_type_formals: NameAndTypeFormals> "(" <args:
//     (ArgDecl)*> ")" <ret: ReturnType?>
//     <acquires: AcquireList?>
//     <locals_body: FunctionBlock> =>? { ... }
// }

// NativeFunctionDecl: (FunctionName, Function) = {
//     <nat: NativeTag> <p: Public?> <name_and_type_formals: NameAndTypeFormals>
//     "(" <args: Comma<ArgDecl>> ")" <ret: ReturnType?>
//         <acquires: AcquireList?>
//         ";" =>? { ... }
// }

fn parse_function_decl<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<(FunctionName, Function), ParseError<usize, anyhow::Error>> {
    let start_loc = tokens.start_loc();

    let is_native = if tokens.peek() == Tok::Native {
        tokens.advance()?;
        true
    } else {
        false
    };

    let is_public = if tokens.peek() == Tok::Public {
        tokens.advance()?;
        true
    } else {
        false
    };

    let (name, type_formals) = parse_name_and_type_formals(tokens)?;
    consume_token(tokens, Tok::LParen)?;
    let args = parse_comma_list(tokens, &[Tok::RParen], parse_arg_decl, true)?;
    consume_token(tokens, Tok::RParen)?;

    let ret = if tokens.peek() == Tok::Colon {
        Some(parse_return_type(tokens)?)
    } else {
        None
    };

    let acquires = if tokens.peek() == Tok::Acquires {
        Some(parse_acquire_list(tokens)?)
    } else {
        None
    };

    // parse each specification directive--there may be zero or more
    let mut specifications = Vec::new();
    while tokens.peek().is_spec_directive() {
        let start_loc = tokens.start_loc();
        let cond = parse_spec_condition(tokens)?;
        let end_loc = tokens.previous_end_loc();
        specifications.push(spanned(start_loc, end_loc, cond));
    }

    let func_name = FunctionName::parse(name)?;
    let func = Function_::new(
        if is_public {
            FunctionVisibility::Public
        } else {
            FunctionVisibility::Internal
        },
        args,
        ret.unwrap_or_else(|| vec![]),
        type_formals,
        acquires.unwrap_or_else(Vec::new),
        specifications,
        if is_native {
            consume_token(tokens, Tok::Semicolon)?;
            FunctionBody::Native
        } else {
            let (locals, body) = parse_function_block_(tokens)?;
            FunctionBody::Move { locals, code: body }
        },
    );

    let end_loc = tokens.previous_end_loc();
    Ok((func_name, spanned(start_loc, end_loc, func)))
}

// FieldDecl : (Field_, Type) = {
//     <f: Sp<Field>> ":" <t: Type> => (f, t)
// }

fn parse_field_decl<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<(Field, Type), ParseError<usize, anyhow::Error>> {
    let f = parse_field(tokens)?;
    consume_token(tokens, Tok::Colon)?;
    let t = parse_type(tokens)?;
    Ok((f, t))
}

// Modules: Vec<ModuleDefinition> = {
//     "modules:" <c: Module*> "script:" => c,
// }

fn parse_modules<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Vec<ModuleDefinition>, ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::Modules)?;
    let mut c: Vec<ModuleDefinition> = vec![];
    while tokens.peek() == Tok::Module {
        c.push(parse_module(tokens)?);
    }
    consume_token(tokens, Tok::Script)?;
    Ok(c)
}

// pub Program : Program = {
//     <m: Modules?> <s: Script> => { ... },
//     <m: Module> => { ... }
// }

fn parse_program<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Program, ParseError<usize, anyhow::Error>> {
    if tokens.peek() == Tok::Module {
        let m = parse_module(tokens)?;
        let ret = Spanned {
            span: Span::default(),
            value: Cmd_::Return(Box::new(Spanned::no_loc(Exp_::ExprList(vec![])))),
        };
        let return_stmt = Statement::CommandStatement(ret);
        let body = FunctionBody::Move {
            locals: vec![],
            code: Block_::new(vec![return_stmt]),
        };
        let main = Function_::new(
            FunctionVisibility::Public,
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            body,
        );
        Ok(Program::new(
            vec![m],
            Script::new(vec![], Spanned::no_loc(main)),
        ))
    } else {
        let modules = if tokens.peek() == Tok::Modules {
            parse_modules(tokens)?
        } else {
            vec![]
        };
        let s = parse_script(tokens)?;
        Ok(Program::new(modules, s))
    }
}

// pub Script : Script = {
//     <imports: (ImportDecl)*>
//     "main" "(" <args: Comma<ArgDecl>> ")" <locals_body: FunctionBlock> => { ... }
// }

fn parse_script<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<Script, ParseError<usize, anyhow::Error>> {
    let start_loc = tokens.start_loc();
    let mut imports: Vec<ImportDefinition> = vec![];
    while tokens.peek() == Tok::Import {
        imports.push(parse_import_decl(tokens)?);
    }
    consume_token(tokens, Tok::Main)?;
    consume_token(tokens, Tok::LParen)?;
    let args = parse_comma_list(tokens, &[Tok::RParen], parse_arg_decl, true)?;
    consume_token(tokens, Tok::RParen)?;
    let (locals, body) = parse_function_block_(tokens)?;
    let end_loc = tokens.previous_end_loc();
    let main = Function_::new(
        FunctionVisibility::Public,
        args,
        vec![],
        vec![],
        vec![],
        vec![],
        FunctionBody::Move { locals, code: body },
    );
    let main = spanned(start_loc, end_loc, main);
    Ok(Script::new(imports, main))
}

// StructKind: bool = {
//     "struct" => false,
//     "resource" => true
// }
// StructDecl: StructDefinition_ = {
//     <is_nominal_resource: StructKind> <name_and_type_formals:
//     NameAndTypeFormals> "{" <data: Comma<FieldDecl>> "}" =>? { ... }
//     <native: NativeTag> <is_nominal_resource: StructKind>
//     <name_and_type_formals: NameAndTypeFormals> ";" =>? { ... }
// }

fn parse_struct_decl<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<StructDefinition, ParseError<usize, anyhow::Error>> {
    let start_loc = tokens.start_loc();

    let is_native = if tokens.peek() == Tok::Native {
        tokens.advance()?;
        true
    } else {
        false
    };

    let is_nominal_resource = match tokens.peek() {
        Tok::Struct => false,
        Tok::Resource => true,
        _ => {
            return Err(ParseError::InvalidToken {
                location: tokens.start_loc(),
            })
        }
    };
    tokens.advance()?;

    let (name, type_formals) = parse_name_and_type_formals(tokens)?;

    if is_native {
        consume_token(tokens, Tok::Semicolon)?;
        let end_loc = tokens.previous_end_loc();
        return Ok(spanned(
            start_loc,
            end_loc,
            StructDefinition_::native(is_nominal_resource, name, type_formals)?,
        ));
    }

    consume_token(tokens, Tok::LBrace)?;
    let fields = parse_comma_list(
        tokens,
        &[Tok::RBrace, Tok::Invariant],
        parse_field_decl,
        true,
    )?;
    let invariants = if tokens.peek() == Tok::Invariant {
        parse_comma_list(tokens, &[Tok::RBrace], parse_invariant, true)?
    } else {
        vec![]
    };
    consume_token(tokens, Tok::RBrace)?;
    let end_loc = tokens.previous_end_loc();
    Ok(spanned(
        start_loc,
        end_loc,
        StructDefinition_::move_declared(
            is_nominal_resource,
            name,
            type_formals,
            fields,
            invariants,
        )?,
    ))
}

// QualifiedModuleIdent: QualifiedModuleIdent = {
//     <a: AccountAddress> "." <m: ModuleName> => QualifiedModuleIdent::new(m, a),
// }

fn parse_qualified_module_ident<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<QualifiedModuleIdent, ParseError<usize, anyhow::Error>> {
    let a = parse_account_address(tokens)?;
    consume_token(tokens, Tok::Period)?;
    let m = parse_module_name(tokens)?;
    Ok(QualifiedModuleIdent::new(m, a))
}

// ModuleIdent: ModuleIdent = {
//     <q: QualifiedModuleIdent> => ModuleIdent::Qualified(q),
//     <transaction_dot_module: DotName> =>? { ... }
// }

fn parse_module_ident<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<ModuleIdent, ParseError<usize, anyhow::Error>> {
    if tokens.peek() == Tok::AccountAddressValue {
        return Ok(ModuleIdent::Qualified(parse_qualified_module_ident(
            tokens,
        )?));
    }
    let transaction_dot_module = parse_dot_name(tokens)?;
    let v: Vec<&str> = transaction_dot_module.split('.').collect();
    assert!(v.len() == 2);
    let ident: String = v[0].to_string();
    if ident != "Transaction" {
        panic!("Ident = {} which is not Transaction", ident);
    }
    let m: ModuleName = ModuleName::parse(v[1])?;
    Ok(ModuleIdent::Transaction(m))
}

// ImportAlias: ModuleName = {
//     "as" <alias: ModuleName> => { ... }
// }

fn parse_import_alias<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<ModuleName, ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::As)?;
    let alias = parse_module_name(tokens)?;
    if alias.as_inner() == ModuleName::self_name() {
        panic!(
            "Invalid use of reserved module alias '{}'",
            ModuleName::self_name()
        );
    }
    Ok(alias)
}

// ImportDecl: ImportDefinition = {
//     "import" <ident: ModuleIdent> <alias: ImportAlias?> ";" => { ... }
// }

fn parse_import_decl<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<ImportDefinition, ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::Import)?;
    let ident = parse_module_ident(tokens)?;
    let alias = if tokens.peek() == Tok::As {
        Some(parse_import_alias(tokens)?)
    } else {
        None
    };
    consume_token(tokens, Tok::Semicolon)?;
    Ok(ImportDefinition::new(ident, alias))
}

// pub Module : ModuleDefinition = {
//     "module" <n: Name> "{"
//         <imports: (ImportDecl)*>
//         <structs: (StructDecl)*>
//         <functions: (FunctionDecl)*>
//     "}" =>? ModuleDefinition::new(n, imports, structs, functions),
// }

fn is_struct_decl<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<bool, ParseError<usize, anyhow::Error>> {
    let mut t = tokens.peek();
    if t == Tok::Native {
        t = tokens.lookahead()?;
    }
    Ok(t == Tok::Struct || t == Tok::Resource)
}

fn parse_module<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<ModuleDefinition, ParseError<usize, anyhow::Error>> {
    consume_token(tokens, Tok::Module)?;
    let name = parse_name(tokens)?;
    consume_token(tokens, Tok::LBrace)?;

    let mut imports: Vec<ImportDefinition> = vec![];
    while tokens.peek() == Tok::Import {
        imports.push(parse_import_decl(tokens)?);
    }

    let mut synthetics = vec![];
    while tokens.peek() == Tok::Synthetic {
        synthetics.push(parse_synthetic(tokens)?);
    }

    let mut structs: Vec<StructDefinition> = vec![];
    while is_struct_decl(tokens)? {
        structs.push(parse_struct_decl(tokens)?);
    }

    let mut functions: Vec<(FunctionName, Function)> = vec![];
    while tokens.peek() != Tok::RBrace {
        functions.push(parse_function_decl(tokens)?);
    }
    tokens.advance()?; // consume the RBrace

    Ok(ModuleDefinition::new(
        name, imports, structs, functions, synthetics,
    )?)
}

// pub ScriptOrModule: ScriptOrModule = {
//     <s: Script> => ScriptOrModule::Script(s),
//     <m: Module> => ScriptOrModule::Module(m),
// }

fn parse_script_or_module<'input>(
    tokens: &mut Lexer<'input>,
) -> Result<ScriptOrModule, ParseError<usize, anyhow::Error>> {
    if tokens.peek() == Tok::Module {
        Ok(ScriptOrModule::Module(parse_module(tokens)?))
    } else {
        Ok(ScriptOrModule::Script(parse_script(tokens)?))
    }
}

pub fn parse_cmd_string<'input>(
    input: &'input str,
) -> Result<Cmd_, ParseError<usize, anyhow::Error>> {
    let mut tokens = Lexer::new(input);
    tokens.advance()?;
    parse_cmd_(&mut tokens)
}

pub fn parse_module_string<'input>(
    input: &'input str,
) -> Result<ModuleDefinition, ParseError<usize, anyhow::Error>> {
    let mut tokens = Lexer::new(input);
    tokens.advance()?;
    parse_module(&mut tokens)
}

pub fn parse_program_string<'input>(
    input: &'input str,
) -> Result<Program, ParseError<usize, anyhow::Error>> {
    let mut tokens = Lexer::new(input);
    tokens.advance()?;
    parse_program(&mut tokens)
}

pub fn parse_script_string<'input>(
    input: &'input str,
) -> Result<Script, ParseError<usize, anyhow::Error>> {
    let mut tokens = Lexer::new(input);
    tokens.advance()?;
    parse_script(&mut tokens)
}

pub fn parse_script_or_module_string<'input>(
    input: &'input str,
) -> Result<ScriptOrModule, ParseError<usize, anyhow::Error>> {
    let mut tokens = Lexer::new(input);
    tokens.advance()?;
    parse_script_or_module(&mut tokens)
}
