use std::fmt::Display;

use crate::token::{Attribute, Token};
use crate::{
    Distinctness, Ident, Path, Pattern, Recoverable, Statement, TraitConstraint, UnresolvedType,
    UnresolvedTypeData, Visibility,
};
use acvm::FieldElement;
use iter_extended::vecmap;
use noirc_errors::{Span, Spanned};

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ExpressionKind {
    Literal(Literal),
    Block(BlockExpression),
    Prefix(Box<PrefixExpression>),
    Index(Box<IndexExpression>),
    Call(Box<CallExpression>),
    MethodCall(Box<MethodCallExpression>),
    Constructor(Box<ConstructorExpression>),
    MemberAccess(Box<MemberAccessExpression>),
    Cast(Box<CastExpression>),
    Infix(Box<InfixExpression>),
    For(Box<ForExpression>),
    If(Box<IfExpression>),
    Variable(Path),
    Tuple(Vec<Expression>),
    Lambda(Box<Lambda>),
    Error,
}

/// A Vec of unresolved names for type variables.
/// For `fn foo<A, B>(...)` this corresponds to vec!["A", "B"].
pub type UnresolvedGenerics = Vec<Ident>;

impl ExpressionKind {
    pub fn into_path(self) -> Option<Path> {
        match self {
            ExpressionKind::Variable(path) => Some(path),
            _ => None,
        }
    }

    pub fn into_infix(self) -> Option<InfixExpression> {
        match self {
            ExpressionKind::Infix(infix) => Some(*infix),
            _ => None,
        }
    }

    pub fn prefix(operator: UnaryOp, rhs: Expression) -> ExpressionKind {
        ExpressionKind::Prefix(Box::new(PrefixExpression { operator, rhs }))
    }

    pub fn array(contents: Vec<Expression>) -> ExpressionKind {
        ExpressionKind::Literal(Literal::Array(ArrayLiteral::Standard(contents)))
    }

    pub fn repeated_array(repeated_element: Expression, length: Expression) -> ExpressionKind {
        ExpressionKind::Literal(Literal::Array(ArrayLiteral::Repeated {
            repeated_element: Box::new(repeated_element),
            length: Box::new(length),
        }))
    }

    pub fn integer(contents: FieldElement) -> ExpressionKind {
        ExpressionKind::Literal(Literal::Integer(contents))
    }

    pub fn boolean(contents: bool) -> ExpressionKind {
        ExpressionKind::Literal(Literal::Bool(contents))
    }

    pub fn string(contents: String) -> ExpressionKind {
        ExpressionKind::Literal(Literal::Str(contents))
    }

    pub fn format_string(contents: String) -> ExpressionKind {
        ExpressionKind::Literal(Literal::FmtStr(contents))
    }

    pub fn constructor((type_name, fields): (Path, Vec<(Ident, Expression)>)) -> ExpressionKind {
        ExpressionKind::Constructor(Box::new(ConstructorExpression { type_name, fields }))
    }

    /// Returns true if the expression is a literal integer
    pub fn is_integer(&self) -> bool {
        self.as_integer().is_some()
    }

    fn as_integer(&self) -> Option<FieldElement> {
        let literal = match self {
            ExpressionKind::Literal(literal) => literal,
            _ => return None,
        };

        match literal {
            Literal::Integer(integer) => Some(*integer),
            _ => None,
        }
    }
}

impl Recoverable for ExpressionKind {
    fn error(_: Span) -> Self {
        ExpressionKind::Error
    }
}

impl Recoverable for Expression {
    fn error(span: Span) -> Self {
        Expression::new(ExpressionKind::Error, span)
    }
}

impl Recoverable for Option<Expression> {
    fn error(span: Span) -> Self {
        Some(Expression::new(ExpressionKind::Error, span))
    }
}

#[derive(Debug, Eq, Clone)]
pub struct Expression {
    pub kind: ExpressionKind,
    pub span: Span,
}

// This is important for tests. Two expressions are the same, if their Kind is the same
// We are ignoring Span
impl PartialEq<Expression> for Expression {
    fn eq(&self, rhs: &Expression) -> bool {
        self.kind == rhs.kind
    }
}

impl Expression {
    pub fn new(kind: ExpressionKind, span: Span) -> Expression {
        Expression { kind, span }
    }

    pub fn member_access_or_method_call(
        lhs: Expression,
        (rhs, args): (Ident, Option<Vec<Expression>>),
        span: Span,
    ) -> Expression {
        let kind = match args {
            None => ExpressionKind::MemberAccess(Box::new(MemberAccessExpression { lhs, rhs })),
            Some(arguments) => ExpressionKind::MethodCall(Box::new(MethodCallExpression {
                object: lhs,
                method_name: rhs,
                arguments,
            })),
        };
        Expression::new(kind, span)
    }

    pub fn index(collection: Expression, index: Expression, span: Span) -> Expression {
        let kind = ExpressionKind::Index(Box::new(IndexExpression { collection, index }));
        Expression::new(kind, span)
    }

    pub fn cast(lhs: Expression, r#type: UnresolvedType, span: Span) -> Expression {
        let kind = ExpressionKind::Cast(Box::new(CastExpression { lhs, r#type }));
        Expression::new(kind, span)
    }

    pub fn call(lhs: Expression, arguments: Vec<Expression>, span: Span) -> Expression {
        // Need to check if lhs is an if expression since users can sequence if expressions
        // with tuples without calling them. E.g. `if c { t } else { e }(a, b)` is interpreted
        // as a sequence of { if, tuple } rather than a function call. This behavior matches rust.
        let kind = if matches!(&lhs.kind, ExpressionKind::If(..)) {
            ExpressionKind::Block(BlockExpression(vec![
                Statement::Expression(lhs),
                Statement::Expression(Expression::new(ExpressionKind::Tuple(arguments), span)),
            ]))
        } else {
            ExpressionKind::Call(Box::new(CallExpression { func: Box::new(lhs), arguments }))
        };
        Expression::new(kind, span)
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ForExpression {
    pub identifier: Ident,
    pub start_range: Expression,
    pub end_range: Expression,
    pub block: Expression,
}

pub type BinaryOp = Spanned<BinaryOpKind>;

#[derive(PartialEq, PartialOrd, Eq, Ord, Hash, Debug, Copy, Clone)]
pub enum BinaryOpKind {
    Add,
    Subtract,
    Multiply,
    Divide,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    And,
    Or,
    Xor,
    ShiftRight,
    ShiftLeft,
    Modulo,
}

impl BinaryOpKind {
    /// Comparator operators return a 0 or 1
    /// When seen in the middle of an infix operator,
    /// they transform the infix expression into a predicate expression
    pub fn is_comparator(self) -> bool {
        matches!(
            self,
            BinaryOpKind::Equal
                | BinaryOpKind::NotEqual
                | BinaryOpKind::LessEqual
                | BinaryOpKind::Less
                | BinaryOpKind::Greater
                | BinaryOpKind::GreaterEqual
        )
    }

    pub fn is_valid_for_field_type(self) -> bool {
        matches!(self, BinaryOpKind::Equal | BinaryOpKind::NotEqual)
    }

    pub fn as_string(self) -> &'static str {
        match self {
            BinaryOpKind::Add => "+",
            BinaryOpKind::Subtract => "-",
            BinaryOpKind::Multiply => "*",
            BinaryOpKind::Divide => "/",
            BinaryOpKind::Equal => "==",
            BinaryOpKind::NotEqual => "!=",
            BinaryOpKind::Less => "<",
            BinaryOpKind::LessEqual => "<=",
            BinaryOpKind::Greater => ">",
            BinaryOpKind::GreaterEqual => ">=",
            BinaryOpKind::And => "&",
            BinaryOpKind::Or => "|",
            BinaryOpKind::Xor => "^",
            BinaryOpKind::ShiftRight => ">>",
            BinaryOpKind::ShiftLeft => "<<",
            BinaryOpKind::Modulo => "%",
        }
    }

    pub fn as_token(self) -> Token {
        match self {
            BinaryOpKind::Add => Token::Plus,
            BinaryOpKind::Subtract => Token::Minus,
            BinaryOpKind::Multiply => Token::Star,
            BinaryOpKind::Divide => Token::Slash,
            BinaryOpKind::Equal => Token::Equal,
            BinaryOpKind::NotEqual => Token::NotEqual,
            BinaryOpKind::Less => Token::Less,
            BinaryOpKind::LessEqual => Token::LessEqual,
            BinaryOpKind::Greater => Token::Greater,
            BinaryOpKind::GreaterEqual => Token::GreaterEqual,
            BinaryOpKind::And => Token::Ampersand,
            BinaryOpKind::Or => Token::Pipe,
            BinaryOpKind::Xor => Token::Caret,
            BinaryOpKind::ShiftLeft => Token::ShiftLeft,
            BinaryOpKind::ShiftRight => Token::ShiftRight,
            BinaryOpKind::Modulo => Token::Percent,
        }
    }

    pub fn is_bit_shift(&self) -> bool {
        matches!(self, BinaryOpKind::ShiftRight | BinaryOpKind::ShiftLeft)
    }
}

#[derive(PartialEq, PartialOrd, Eq, Ord, Hash, Debug, Copy, Clone)]
pub enum UnaryOp {
    Minus,
    Not,
    MutableReference,

    /// If implicitly_added is true, this operation was implicitly added by the compiler for a
    /// field dereference. The compiler may undo some of these implicitly added dereferences if
    /// the reference later turns out to be needed (e.g. passing a field by reference to a function
    /// requiring an &mut parameter).
    Dereference {
        implicitly_added: bool,
    },
}

impl UnaryOp {
    /// Converts a token to a unary operator
    /// If you want the parser to recognize another Token as being a prefix operator, it is defined here
    pub fn from(token: &Token) -> Option<UnaryOp> {
        match token {
            Token::Minus => Some(UnaryOp::Minus),
            Token::Bang => Some(UnaryOp::Not),
            _ => None,
        }
    }
}
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Literal {
    Array(ArrayLiteral),
    Bool(bool),
    Integer(FieldElement),
    Str(String),
    FmtStr(String),
    Unit,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct PrefixExpression {
    pub operator: UnaryOp,
    pub rhs: Expression,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct InfixExpression {
    pub lhs: Expression,
    pub operator: BinaryOp,
    pub rhs: Expression,
}

// This is an infix expression with 'as' as the binary operator
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct CastExpression {
    pub lhs: Expression,
    pub r#type: UnresolvedType,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct IfExpression {
    pub condition: Expression,
    pub consequence: Expression,
    pub alternative: Option<Expression>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Lambda {
    pub parameters: Vec<(Pattern, UnresolvedType)>,
    pub return_type: UnresolvedType,
    pub body: Expression,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct FunctionDefinition {
    pub name: Ident,

    // XXX: Currently we only have one attribute defined. If more attributes are needed per function, we can make this a vector and make attribute definition more expressive
    pub attribute: Option<Attribute>,

    /// True if this function was defined with the 'open' keyword
    pub is_open: bool,

    pub is_internal: bool,

    /// True if this function was defined with the 'unconstrained' keyword
    pub is_unconstrained: bool,

    /// True if this function was defined with the 'pub' keyword
    pub is_public: bool,

    pub generics: UnresolvedGenerics,
    pub parameters: Vec<(Pattern, UnresolvedType, Visibility)>,
    pub body: BlockExpression,
    pub span: Span,
    pub where_clause: Vec<TraitConstraint>,
    pub return_type: FunctionReturnType,
    pub return_visibility: Visibility,
    pub return_distinctness: Distinctness,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum FunctionReturnType {
    /// Returns type is not specified.
    Default(Span),
    /// Everything else.
    Ty(UnresolvedType, Span),
}

/// Describes the types of smart contract functions that are allowed.
/// - All Noir programs in the non-contract context can be seen as `Secret`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ContractFunctionType {
    /// This function will be executed in a private
    /// context.
    Secret,
    /// This function will be executed in a public
    /// context.
    Open,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ArrayLiteral {
    Standard(Vec<Expression>),
    Repeated { repeated_element: Box<Expression>, length: Box<Expression> },
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct CallExpression {
    pub func: Box<Expression>,
    pub arguments: Vec<Expression>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct MethodCallExpression {
    pub object: Expression,
    pub method_name: Ident,
    pub arguments: Vec<Expression>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ConstructorExpression {
    pub type_name: Path,
    pub fields: Vec<(Ident, Expression)>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct MemberAccessExpression {
    pub lhs: Expression,
    pub rhs: Ident,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct IndexExpression {
    pub collection: Expression, // XXX: For now, this will be the name of the array, as we do not support other collections
    pub index: Expression, // XXX: We accept two types of indices, either a normal integer or a constant
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct BlockExpression(pub Vec<Statement>);

impl BlockExpression {
    pub fn pop(&mut self) -> Option<Statement> {
        self.0.pop()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Display for Expression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind.fmt(f)
    }
}

impl Display for ExpressionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use ExpressionKind::*;
        match self {
            Literal(literal) => literal.fmt(f),
            Block(block) => block.fmt(f),
            Prefix(prefix) => prefix.fmt(f),
            Index(index) => index.fmt(f),
            Call(call) => call.fmt(f),
            MethodCall(call) => call.fmt(f),
            Cast(cast) => cast.fmt(f),
            Infix(infix) => infix.fmt(f),
            For(for_loop) => for_loop.fmt(f),
            If(if_expr) => if_expr.fmt(f),
            Variable(path) => path.fmt(f),
            Constructor(constructor) => constructor.fmt(f),
            MemberAccess(access) => access.fmt(f),
            Tuple(elements) => {
                let elements = vecmap(elements, ToString::to_string);
                write!(f, "({})", elements.join(", "))
            }
            Lambda(lambda) => lambda.fmt(f),
            Error => write!(f, "Error"),
        }
    }
}

impl Display for Literal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Literal::Array(ArrayLiteral::Standard(elements)) => {
                let contents = vecmap(elements, ToString::to_string);
                write!(f, "[{}]", contents.join(", "))
            }
            Literal::Array(ArrayLiteral::Repeated { repeated_element, length }) => {
                write!(f, "[{repeated_element}; {length}]")
            }
            Literal::Bool(boolean) => write!(f, "{}", if *boolean { "true" } else { "false" }),
            Literal::Integer(integer) => write!(f, "{}", integer.to_u128()),
            Literal::Str(string) => write!(f, "\"{string}\""),
            Literal::FmtStr(string) => write!(f, "f\"{string}\""),
            Literal::Unit => write!(f, "()"),
        }
    }
}

impl Display for BlockExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{{")?;
        for statement in &self.0 {
            let statement = statement.to_string();
            for line in statement.lines() {
                writeln!(f, "    {line}")?;
            }
        }
        write!(f, "}}")
    }
}

impl Display for PrefixExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({} {})", self.operator, self.rhs)
    }
}

impl Display for UnaryOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnaryOp::Minus => write!(f, "-"),
            UnaryOp::Not => write!(f, "!"),
            UnaryOp::MutableReference => write!(f, "&mut"),
            UnaryOp::Dereference { .. } => write!(f, "*"),
        }
    }
}

impl Display for IndexExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}[{}]", self.collection, self.index)
    }
}

impl Display for CallExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let args = vecmap(&self.arguments, ToString::to_string);
        write!(f, "{}({})", self.func, args.join(", "))
    }
}

impl Display for MethodCallExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let args = vecmap(&self.arguments, ToString::to_string);
        write!(f, "{}.{}({})", self.object, self.method_name, args.join(", "))
    }
}

impl Display for CastExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({} as {})", self.lhs, self.r#type)
    }
}

impl Display for ConstructorExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fields =
            self.fields.iter().map(|(ident, expr)| format!("{ident}: {expr}")).collect::<Vec<_>>();

        write!(f, "({} {{ {} }})", self.type_name, fields.join(", "))
    }
}

impl Display for MemberAccessExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({}.{})", self.lhs, self.rhs)
    }
}

impl Display for InfixExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({} {} {})", self.lhs, self.operator.contents, self.rhs)
    }
}

impl Display for BinaryOpKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinaryOpKind::Add => write!(f, "+"),
            BinaryOpKind::Subtract => write!(f, "-"),
            BinaryOpKind::Multiply => write!(f, "*"),
            BinaryOpKind::Divide => write!(f, "/"),
            BinaryOpKind::Equal => write!(f, "=="),
            BinaryOpKind::NotEqual => write!(f, "!="),
            BinaryOpKind::Less => write!(f, "<"),
            BinaryOpKind::LessEqual => write!(f, "<="),
            BinaryOpKind::Greater => write!(f, ">"),
            BinaryOpKind::GreaterEqual => write!(f, ">="),
            BinaryOpKind::And => write!(f, "&"),
            BinaryOpKind::Or => write!(f, "|"),
            BinaryOpKind::Xor => write!(f, "^"),
            BinaryOpKind::ShiftLeft => write!(f, "<<"),
            BinaryOpKind::ShiftRight => write!(f, ">>"),
            BinaryOpKind::Modulo => write!(f, "%"),
        }
    }
}

impl Display for ForExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "for {} in {} .. {} {}",
            self.identifier, self.start_range, self.end_range, self.block
        )
    }
}

impl Display for IfExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "if {} {}", self.condition, self.consequence)?;
        if let Some(alternative) = &self.alternative {
            write!(f, " else {alternative}")?;
        }
        Ok(())
    }
}

impl Display for Lambda {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parameters = vecmap(&self.parameters, |(name, r#type)| format!("{name}: {type}"));

        write!(f, "|{}| -> {} {{ {} }}", parameters.join(", "), self.return_type, self.body)
    }
}

impl FunctionDefinition {
    pub fn normal(
        name: &Ident,
        generics: &UnresolvedGenerics,
        parameters: &[(Ident, UnresolvedType)],
        body: &BlockExpression,
        where_clause: &[TraitConstraint],
        return_type: &FunctionReturnType,
    ) -> FunctionDefinition {
        let p = parameters
            .iter()
            .map(|(ident, unresolved_type)| {
                (Pattern::Identifier(ident.clone()), unresolved_type.clone(), Visibility::Private)
            })
            .collect();
        FunctionDefinition {
            name: name.clone(),
            attribute: None,
            is_open: false,
            is_internal: false,
            is_unconstrained: false,
            generics: generics.clone(),
            parameters: p,
            body: body.clone(),
            span: name.span(),
            where_clause: where_clause.to_vec(),
            return_type: return_type.clone(),
            return_visibility: Visibility::Private,
            return_distinctness: Distinctness::DuplicationAllowed,
        }
    }
}

impl Display for FunctionDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(attribute) = &self.attribute {
            writeln!(f, "{attribute}")?;
        }

        let parameters = vecmap(&self.parameters, |(name, r#type, visibility)| {
            format!("{name}: {visibility} {type}")
        });

        let where_clause = vecmap(&self.where_clause, ToString::to_string);
        let where_clause_str = if !where_clause.is_empty() {
            format!("where {}", where_clause.join(", "))
        } else {
            "".to_string()
        };

        write!(
            f,
            "fn {}({}) -> {} {} {}",
            self.name,
            parameters.join(", "),
            self.return_type,
            where_clause_str,
            self.body
        )
    }
}

impl FunctionReturnType {
    pub fn get_type(&self) -> &UnresolvedTypeData {
        match self {
            FunctionReturnType::Default(_span) => &UnresolvedTypeData::Unit,
            FunctionReturnType::Ty(typ, _span) => &typ.typ,
        }
    }
}

impl Display for FunctionReturnType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FunctionReturnType::Default(_) => f.write_str(""),
            FunctionReturnType::Ty(ty, _) => write!(f, "{ty}"),
        }
    }
}
