//! SQL tokenizer that produces [`Token`] instances.

use std::{fmt::Display, iter::Peekable, str::Chars};

use super::token::{Keyword, Token, Whitespace};

/// Token location.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Location {
    /// Line number starting at 1.
    pub line: usize,
    /// Column number starting at 1.
    pub col: usize,
}

impl Default for Location {
    fn default() -> Self {
        Self { line: 1, col: 1 }
    }
}

/// Stores both the [`Token`] and its starting location in the input string.
#[derive(Debug, PartialEq)]
pub(super) struct TokenWithLocation {
    pub variant: Token,
    pub location: Location,
}

impl TokenWithLocation {
    /// Discards the location. Used mostly for mapping:
    /// `.map(TokenWithLocation::token_only)`.
    pub fn token_only(self) -> Token {
        self.variant
    }

    /// Reference to [`Token`].
    pub fn token(&self) -> &Token {
        &self.variant
    }
}

/// Token stream.
///
/// Wraps a [`Peekable<Chars>`] instance and allows reading the next character
/// in the stream without consuming it.
struct Stream<'i> {
    /// Original string input.
    input: &'i str,
    /// Current location in the stream.
    location: Location,
    /// Character input.
    chars: Peekable<Chars<'i>>,
}

impl<'i> Stream<'i> {
    /// Creates a new stream over `input`.
    fn new(input: &'i str) -> Self {
        Self {
            input,
            location: Location { line: 1, col: 1 },
            chars: input.chars().peekable(),
        }
    }

    /// Consumes the next value updating [`Self::location`] in the process.
    fn next(&mut self) -> Option<char> {
        self.chars.next().inspect(|chr| {
            if *chr == '\n' {
                self.location.line += 1;
                self.location.col = 1;
            } else {
                self.location.col += 1;
            }
        })
    }

    /// Returns a reference to the next character in the stream without
    /// consuming it.
    fn peek(&mut self) -> Option<&char> {
        self.chars.peek()
    }

    /// Consumes one character in the stream and returns a reference to the next
    /// one without consuming it.
    fn peek_next(&mut self) -> Option<&char> {
        self.next();
        self.peek()
    }

    /// Safe version of [`std::iter::TakeWhile`] that does not discard elements
    /// when `predicate` returns `false`.
    fn take_while<P: FnMut(&char) -> bool>(&mut self, predicate: P) -> TakeWhile<'_, 'i, P> {
        TakeWhile {
            stream: self,
            predicate,
        }
    }

    /// Current location in the stream. [`Location`] is [`Copy`], no need for
    /// references.
    fn location(&self) -> Location {
        self.location
    }
}

/// See [`Stream::take_while`] for more details.
struct TakeWhile<'s, 'i, P> {
    stream: &'s mut Stream<'i>,
    predicate: P,
}

impl<'s, 'c, P: FnMut(&char) -> bool> Iterator for TakeWhile<'s, 'c, P> {
    type Item = char;

    fn next(&mut self) -> Option<Self::Item> {
        if (self.predicate)(self.stream.peek()?) {
            self.stream.next()
        } else {
            None
        }
    }
}

/// Some of the possible syntax errors that the [`Tokenizer`] can find.
#[derive(Debug, PartialEq)]
pub(crate) enum ErrorKind {
    UnexpectedOrUnsupportedToken(char),

    UnexpectedWhileParsingOperator { unexpected: char, operator: Token },

    OperatorNotClosed(Token),

    StringNotClosed,

    Other(String),
}

impl Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ErrorKind::UnexpectedOrUnsupportedToken(token) => {
                write!(f, "unexpected or unsupported token '{token}'")
            }

            ErrorKind::UnexpectedWhileParsingOperator {
                unexpected,
                operator,
            } => write!(
                f,
                "unexpected token '{unexpected}' while parsing '{operator}' operator"
            ),

            ErrorKind::StringNotClosed => f.write_str("string not closed"),

            ErrorKind::OperatorNotClosed(operator) => write!(f, "'{operator}' operator not closed"),

            ErrorKind::Other(message) => f.write_str(message),
        }
    }
}

/// If the tokenizer finds an error it means to syntax is not correct.
///
/// Some examples are unclosed strings, unclosed operators, etc.
#[derive(Debug, PartialEq)]
pub(super) struct TokenizerError {
    pub kind: ErrorKind,
    pub location: Location,
    pub input: String,
}

/// Main parsing structure. See [`Tokenizer::next_token`].
pub(super) struct Tokenizer<'i> {
    /// Character stream.
    stream: Stream<'i>,
    /// True once we've returned [`Token::Eof`].
    reached_eof: bool,
}

type TokenResult = Result<Token, TokenizerError>;

impl<'i> Tokenizer<'i> {
    /// Creates a new tokenizer for the given `input`.
    ///
    /// The tokenizer won't parse anything until [`Tokenizer::next_token`] is
    /// called through helper functions or iterators. See [`Tokenizer::iter`]
    /// and [`Tokenizer::tokenize`].
    pub fn new(input: &'i str) -> Self {
        Self {
            stream: Stream::new(input),
            reached_eof: false,
        }
    }

    /// Creates an iterator over [`Self`].
    ///
    /// Used mainly to parse tokens as they are found instead of waiting for the
    /// tokenizer to consume the entire input string.
    pub fn iter<'t>(&'t mut self) -> Iter<'t, 'i> {
        self.into_iter()
    }

    /// Reads the characters in [`Self::stream`] one by one parsing the results
    /// into [`Token`] variants.
    ///
    /// If an error is encountered in the process, this function returns
    /// immediately.
    pub fn tokenize(&mut self) -> Result<Vec<Token>, TokenizerError> {
        self.iter()
            .map(|result| result.map(TokenWithLocation::token_only))
            .collect()
    }

    /// Returns [`None`] once [`Token::Eof`] has been returned.
    ///
    /// Useful for iterators.
    fn optional_next_token_with_location(
        &mut self,
    ) -> Option<Result<TokenWithLocation, TokenizerError>> {
        if !self.reached_eof {
            Some(self.next_token_with_location())
        } else {
            None
        }
    }

    /// Same as [`Self::next_token`] but returns the starting location
    /// of the token as well.
    fn next_token_with_location(&mut self) -> Result<TokenWithLocation, TokenizerError> {
        let location = self.stream.location();

        self.next_token().map(|token| TokenWithLocation {
            variant: token,
            location,
        })
    }

    /// Consumes and returns the next [`Token`] variant in [`Self::stream`].
    fn next_token(&mut self) -> TokenResult {
        // Done, no more chars.
        let Some(chr) = self.stream.peek() else {
            self.reached_eof = true;
            return Ok(Token::Eof);
        };

        match chr {
            ' ' => self.consume(Token::Whitespace(Whitespace::Space)),

            '\t' => self.consume(Token::Whitespace(Whitespace::Tab)),

            '\n' => self.consume(Token::Whitespace(Whitespace::Newline)),

            '\r' => match self.stream.peek_next() {
                Some('\n') => self.consume(Token::Whitespace(Whitespace::Newline)),
                _ => Ok(Token::Whitespace(Whitespace::Newline)),
            },

            '<' => match self.stream.peek_next() {
                Some('=') => self.consume(Token::LtEq),
                _ => Ok(Token::Lt),
            },

            '>' => match self.stream.peek_next() {
                Some('=') => self.consume(Token::GtEq),
                _ => Ok(Token::Gt),
            },

            '*' => self.consume(Token::Mul),

            '/' => self.consume(Token::Div),

            '+' => self.consume(Token::Plus),

            '-' => self.consume(Token::Minus),

            '=' => self.consume(Token::Eq),

            '!' => match self.stream.peek_next() {
                Some('=') => self.consume(Token::Neq),

                Some(unexpected) => {
                    let error_kind = ErrorKind::UnexpectedWhileParsingOperator {
                        unexpected: *unexpected,
                        operator: Token::Neq,
                    };
                    self.error(error_kind)
                }

                None => self.error(ErrorKind::OperatorNotClosed(Token::Neq)),
            },

            '(' => self.consume(Token::LeftParen),

            ')' => self.consume(Token::RightParen),

            ',' => self.consume(Token::Comma),

            ';' => self.consume(Token::SemiColon),

            '"' | '\'' => self.tokenize_string(),

            '0'..='9' => self.tokenize_number(),

            _ if Token::is_part_of_ident_or_keyword(chr) => self.tokenize_keyword_or_identifier(),

            _ => {
                let error_kind = ErrorKind::UnexpectedOrUnsupportedToken(*chr);
                self.error(error_kind)
            }
        }
    }

    /// Consumes one character in the stream and returns an [`Ok`] result
    /// containing the given [`Token`] variant.
    fn consume(&mut self, token: Token) -> TokenResult {
        self.stream.next();
        Ok(token)
    }

    /// Builds an instance of [`TokenizerError`] wrapped in [`Err`] giving it
    /// the current location of the stream.
    fn error(&self, kind: ErrorKind) -> TokenResult {
        Err(TokenizerError {
            kind,
            location: self.stream.location(),
            input: self.stream.input.to_owned(),
        })
    }

    /// Parses a single quoted or double quoted string like `"this one"` into
    /// [`Token::String`].
    fn tokenize_string(&mut self) -> TokenResult {
        let quote = self.stream.next().unwrap();

        let string = self.stream.take_while(|chr| *chr != quote).collect();

        if self.stream.next().is_some_and(|chr| chr == quote) {
            Ok(Token::String(string))
        } else {
            self.error(ErrorKind::StringNotClosed)
        }
    }

    /// Tokenizes numbers like `1234`. Floats are not supported.
    fn tokenize_number(&mut self) -> TokenResult {
        Ok(Token::Number(
            self.stream.take_while(char::is_ascii_digit).collect(),
        ))
    }

    /// Attempts to parse an instance of [`Token::Keyword`] or
    /// [`Token::Identifier`].
    fn tokenize_keyword_or_identifier(&mut self) -> TokenResult {
        let value: String = self
            .stream
            .take_while(Token::is_part_of_ident_or_keyword)
            .collect();

        // TODO: Use [phf](https://docs.rs/phf/) or something similar if this
        // keeps growing.
        let keyword = match value.to_uppercase().as_str() {
            "SELECT" => Keyword::Select,
            "CREATE" => Keyword::Create,
            "UPDATE" => Keyword::Update,
            "DELETE" => Keyword::Delete,
            "INSERT" => Keyword::Insert,
            "VALUES" => Keyword::Values,
            "INTO" => Keyword::Into,
            "SET" => Keyword::Set,
            "DROP" => Keyword::Drop,
            "FROM" => Keyword::From,
            "WHERE" => Keyword::Where,
            "AND" => Keyword::And,
            "OR" => Keyword::Or,
            "PRIMARY" => Keyword::Primary,
            "KEY" => Keyword::Key,
            "UNIQUE" => Keyword::Unique,
            "TABLE" => Keyword::Table,
            "DATABASE" => Keyword::Database,
            "INT" => Keyword::Int,
            "BIGINT" => Keyword::BigInt,
            "UNSIGNED" => Keyword::Unsigned,
            "VARCHAR" => Keyword::Varchar,
            "BOOL" => Keyword::Bool,
            "TRUE" => Keyword::True,
            "FALSE" => Keyword::False,
            "ORDER" => Keyword::Order,
            "BY" => Keyword::By,
            "INDEX" => Keyword::Index,
            "ON" => Keyword::On,
            "START" => Keyword::Start,
            "TRANSACTION" => Keyword::Transaction,
            "ROLLBACK" => Keyword::Rollback,
            "COMMIT" => Keyword::Commit,
            "EXPLAIN" => Keyword::Explain,
            _ => Keyword::None,
        };

        Ok(match keyword {
            Keyword::None => Token::Identifier(value),
            _ => Token::Keyword(keyword),
        })
    }
}

/// Struct returned by [`Tokenizer::iter`].
pub(super) struct Iter<'t, 'i> {
    tokenizer: &'t mut Tokenizer<'i>,
}

impl<'t, 'i> Iterator for Iter<'t, 'i> {
    type Item = Result<TokenWithLocation, TokenizerError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.tokenizer.optional_next_token_with_location()
    }
}

impl<'t, 'i> IntoIterator for &'t mut Tokenizer<'i> {
    type IntoIter = Iter<'t, 'i>;
    type Item = Result<TokenWithLocation, TokenizerError>;

    fn into_iter(self) -> Self::IntoIter {
        Iter { tokenizer: self }
    }
}

/// Used to implement [`IntoIterator`] for [`Tokenizer`].
pub(super) struct IntoIter<'i> {
    tokenizer: Tokenizer<'i>,
}

impl<'i> Iterator for IntoIter<'i> {
    type Item = Result<TokenWithLocation, TokenizerError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.tokenizer.optional_next_token_with_location()
    }
}

impl<'i> IntoIterator for Tokenizer<'i> {
    type IntoIter = IntoIter<'i>;
    type Item = Result<TokenWithLocation, TokenizerError>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter { tokenizer: self }
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorKind, Keyword, Token, Tokenizer, Whitespace};
    use crate::sql::tokenizer::{Location, TokenizerError};

    #[test]
    fn tokenize_simple_select() {
        let sql = "SELECT id, name FROM users;";

        assert_eq!(
            Tokenizer::new(sql).tokenize(),
            Ok(vec![
                Token::Keyword(Keyword::Select),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("id".into()),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("name".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::From),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("users".into()),
                Token::SemiColon,
                Token::Eof,
            ])
        );
    }

    #[test]
    fn tokenize_select_where() {
        let sql = "SELECT id, price, discount FROM products WHERE price >= 100;";

        assert_eq!(
            Tokenizer::new(sql).tokenize(),
            Ok(vec![
                Token::Keyword(Keyword::Select),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("id".into()),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("price".into()),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("discount".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::From),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("products".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Where),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("price".into()),
                Token::Whitespace(Whitespace::Space),
                Token::GtEq,
                Token::Whitespace(Whitespace::Space),
                Token::Number("100".into()),
                Token::SemiColon,
                Token::Eof,
            ])
        );
    }

    #[test]
    fn tokenize_select_order_by() {
        let sql = "SELECT name, email FROM users ORDER BY email;";

        assert_eq!(
            Tokenizer::new(sql).tokenize(),
            Ok(vec![
                Token::Keyword(Keyword::Select),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("name".into()),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("email".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::From),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("users".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Order),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::By),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("email".into()),
                Token::SemiColon,
                Token::Eof,
            ])
        );
    }

    #[test]
    fn tokenize_select_where_with_and_or() {
        let sql = "SELECT id, name FROM users WHERE age >= 20 AND age <= 30 OR is_admin = 1;";

        assert_eq!(
            Tokenizer::new(sql).tokenize(),
            Ok(vec![
                Token::Keyword(Keyword::Select),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("id".into()),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("name".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::From),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("users".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Where),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("age".into()),
                Token::Whitespace(Whitespace::Space),
                Token::GtEq,
                Token::Whitespace(Whitespace::Space),
                Token::Number("20".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::And),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("age".into()),
                Token::Whitespace(Whitespace::Space),
                Token::LtEq,
                Token::Whitespace(Whitespace::Space),
                Token::Number("30".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Or),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("is_admin".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Eq,
                Token::Whitespace(Whitespace::Space),
                Token::Number("1".into()),
                Token::SemiColon,
                Token::Eof,
            ])
        );
    }

    #[test]
    fn tokenize_create_table() {
        let sql = "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), is_admin BOOL);";

        assert_eq!(
            Tokenizer::new(sql).tokenize(),
            Ok(vec![
                Token::Keyword(Keyword::Create),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Table),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("users".into()),
                Token::Whitespace(Whitespace::Space),
                Token::LeftParen,
                Token::Identifier("id".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Int),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Primary),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Key),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("name".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Varchar),
                Token::LeftParen,
                Token::Number("255".into()),
                Token::RightParen,
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("is_admin".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Bool),
                Token::RightParen,
                Token::SemiColon,
                Token::Eof,
            ])
        );
    }

    #[test]
    fn tokenize_update_table() {
        let sql = r#"UPDATE products SET code = "promo", discount = 10 WHERE price < 100;"#;

        assert_eq!(
            Tokenizer::new(sql).tokenize(),
            Ok(vec![
                Token::Keyword(Keyword::Update),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("products".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Set),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("code".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Eq,
                Token::Whitespace(Whitespace::Space),
                Token::String("promo".into()),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("discount".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Eq,
                Token::Whitespace(Whitespace::Space),
                Token::Number("10".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Where),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("price".into()),
                Token::Whitespace(Whitespace::Space),
                Token::Lt,
                Token::Whitespace(Whitespace::Space),
                Token::Number("100".into()),
                Token::SemiColon,
                Token::Eof,
            ])
        );
    }

    #[test]
    fn tokenize_insert_into() {
        let sql = r#"INSERT INTO users (name, email, age, is_admin) VALUES ("Test", "test@test.com", 20, TRUE);"#;

        assert_eq!(
            Tokenizer::new(sql).tokenize(),
            Ok(vec![
                Token::Keyword(Keyword::Insert),
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Into),
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("users".into()),
                Token::Whitespace(Whitespace::Space),
                Token::LeftParen,
                Token::Identifier("name".into()),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("email".into()),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("age".into()),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::Identifier("is_admin".into()),
                Token::RightParen,
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::Values),
                Token::Whitespace(Whitespace::Space),
                Token::LeftParen,
                Token::String("Test".into()),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::String("test@test.com".into()),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::Number("20".into()),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::Keyword(Keyword::True),
                Token::RightParen,
                Token::SemiColon,
                Token::Eof,
            ])
        );
    }

    #[test]
    fn tokenize_single_quoted_string() {
        let string = "single quoted \"string\"";
        assert_eq!(
            Tokenizer::new(&format!("'{string}'")).tokenize(),
            Ok(vec![Token::String(string.into()), Token::Eof])
        );
    }

    #[test]
    fn tokenize_incorrect_neq_operator() {
        let sql = "SELECT * FROM table WHERE column ! other";
        assert_eq!(
            Tokenizer::new(sql).tokenize(),
            Err(TokenizerError {
                kind: ErrorKind::UnexpectedWhileParsingOperator {
                    unexpected: ' ',
                    operator: Token::Neq
                },
                location: Location { line: 1, col: 35 },
                input: sql.to_owned(),
            })
        );
    }

    #[test]
    fn tokenize_unclosed_neq_operator() {
        let sql = "SELECT * FROM table WHERE column !";
        assert_eq!(
            Tokenizer::new(sql).tokenize(),
            Err(TokenizerError {
                kind: ErrorKind::OperatorNotClosed(Token::Neq),
                location: Location { line: 1, col: 35 },
                input: sql.to_owned(),
            })
        );
    }

    #[test]
    fn tokenize_double_quoted_string_not_closed() {
        let sql = "SELECT * FROM table WHERE string = \"not closed";
        assert_eq!(
            Tokenizer::new(sql).tokenize(),
            Err(TokenizerError {
                kind: ErrorKind::StringNotClosed,
                location: Location { line: 1, col: 47 },
                input: sql.to_owned(),
            })
        );
    }

    #[test]
    fn tokenize_single_quoted_string_not_closed() {
        let sql = "SELECT * FROM table WHERE string = 'not closed";
        assert_eq!(
            Tokenizer::new(sql).tokenize(),
            Err(TokenizerError {
                kind: ErrorKind::StringNotClosed,
                location: Location { line: 1, col: 47 },
                input: sql.to_owned(),
            })
        );
    }

    #[test]
    fn tokenize_unsupported_token() {
        let sql = "SELECT * FROM ^ WHERE unsupported = 1;";
        assert_eq!(
            Tokenizer::new(sql).tokenize(),
            Err(TokenizerError {
                kind: ErrorKind::UnexpectedOrUnsupportedToken('^'),
                location: Location { line: 1, col: 15 },
                input: sql.to_owned(),
            })
        );
    }
}
