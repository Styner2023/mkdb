//! This module performs semantic or context dependant analysys on parsed SQL
//! statements.
//!
//! After a statement has been successfully analyzed by [`analyze`], it should
//! execute with almost no runtime errors, except for stuff like integer
//! overflow above [`i128::MAX`] (see [`Value`] for details), division by zero
//! or similar edge cases.

use std::{collections::HashSet, fmt::Display};

use super::statement::{Drop, UnaryOperator};
use crate::{
    db::{DatabaseContext, DbError, Schema, SqlError, TableMetadata, MKDB_META, ROW_ID_COL},
    sql::statement::{BinaryOperator, Constraint, Create, DataType, Expression, Statement, Value},
    storage::tuple,
    vm::{TypeError, VmDataType},
};

/// Errors caught at the analyzer layer before the statement is prepared and
/// executed.
#[derive(Debug, PartialEq)]
pub(crate) enum AnalyzerError {
    /// Insert statements where the number of columns doesn't match that of values.
    ColumnValueCountMismatch,
    /// Insert statements that don't specify all the columns in the table.
    MissingColumns,
    /// Column specified more than once.
    DuplicatedColumn(String),
    /// Multiple primary keys defined for the same table.
    MultiplePrimaryKeys,
    /// Table or index already exists.
    AlreadyExists(AlreadyExists),
    /// Number of characters exceeds `VARCHAR(max)`.
    ValueTooLong(String, usize),
    /// Integer data type can't store this value.
    IntegerOutOfRange(i128, DataType),
    /// Attempt to change the special Row ID column manually.
    RowIdAssignment,
    /// Attempt to modify the internal [`MKDB_META`] table.
    MkdbMetaModification,
}

#[derive(Debug, PartialEq)]
pub(crate) enum AlreadyExists {
    Table(String),
    Index(String),
}

impl Display for AlreadyExists {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Index(index) => write!(f, "index {index} already exists"),
            Self::Table(table) => write!(f, "table {table} already exists"),
        }
    }
}

impl Display for AnalyzerError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::ColumnValueCountMismatch => f.write_str("number of columns doesn't match values"),
            Self::MultiplePrimaryKeys => f.write_str("only one primary key per table is allowed"),
            Self::MissingColumns => {
                f.write_str("default values are not supported, all columns must be specified")
            }
            Self::DuplicatedColumn(col) => write!(f, "column '{col}' specified more than once"),
            Self::AlreadyExists(already_exists) => write!(f, "{already_exists}"),
            Self::ValueTooLong(string, max) => {
                write!(f, "string '{string}' too long for type VARCHAR({max})")
            }
            Self::IntegerOutOfRange(num, data_type) => {
                write!(f, "integer {num} out of range for data type {data_type}")
            }
            Self::RowIdAssignment => write!(
                f,
                "'{ROW_ID_COL}' is reserved for internal use, it cannot be manually changed or created"
            ),
            Self::MkdbMetaModification => write!(
                f,
                "table '{MKDB_META}' is reserved for internal use, it cannot be manually changed or created"
            ),
        }
    }
}

/// Analyzes the given statement and returns an error if any.
///
/// If there's no error this function does nothing else.
pub(crate) fn analyze(
    statement: &Statement,
    ctx: &mut impl DatabaseContext,
) -> Result<(), DbError> {
    match statement {
        Statement::Create(Create::Table { columns, name }) => {
            match ctx.table_metadata(name) {
                Err(DbError::Sql(SqlError::InvalidTable(_))) => {
                    // Table doesn't exist, we can create it.
                }

                Ok(_) => {
                    return Err(DbError::from(AnalyzerError::AlreadyExists(
                        AlreadyExists::Table(name.clone()),
                    )));
                }

                Err(e) => return Err(e),
            }

            let mut found_primary_key = false;
            let mut duplicates = HashSet::new();

            for col in columns {
                if !duplicates.insert(&col.name) {
                    return Err(AnalyzerError::DuplicatedColumn(col.name.to_owned()).into());
                }

                if col.name == ROW_ID_COL {
                    return Err(AnalyzerError::RowIdAssignment.into());
                }

                if col.constraints.contains(&Constraint::PrimaryKey) {
                    if found_primary_key {
                        return Err(AnalyzerError::MultiplePrimaryKeys.into());
                    }
                    found_primary_key = true;
                }
            }
        }

        Statement::Create(Create::Index {
            table,
            unique,
            name,
            ..
        }) => {
            if !unique {
                return Err(DbError::Sql(SqlError::Other(
                    "non-unique indexes are not supported".into(),
                )));
            }

            let metadata = ctx.table_metadata(table)?;

            // TODO: We're only checking if the table has an index with the same
            // name, but we should check all indexes. We don't have an index
            // cache yet so we'll do this at least.
            if metadata.indexes.iter().any(|index| &index.name == name) {
                return Err(
                    AnalyzerError::AlreadyExists(AlreadyExists::Index(name.clone())).into(),
                );
            }
        }

        Statement::Insert {
            into,
            columns,
            values,
        } => {
            let metadata = ctx.table_metadata(into)?;

            if into == MKDB_META {
                return Err(AnalyzerError::MkdbMetaModification.into());
            }

            let mut columns = columns.as_slice();

            // In case the user didn't specify any columns.
            let schema_column_names_copy: Vec<String>;

            if columns.is_empty() {
                schema_column_names_copy = metadata.schema.column_identifiers();
                columns = schema_column_names_copy.as_slice();
                // User can't set row ID manually so remove it.
                if columns[0] == ROW_ID_COL {
                    columns = &schema_column_names_copy[1..];
                }
            }

            if columns.len() != values.len() {
                return Err(AnalyzerError::ColumnValueCountMismatch.into());
            }

            let mut duplicates = HashSet::new();

            for col in columns {
                if metadata.schema.index_of(col).is_none() {
                    return Err(DbError::Sql(SqlError::InvalidColumn(col.clone())));
                }
                if !duplicates.insert(col) {
                    return Err(AnalyzerError::DuplicatedColumn(col.into()).into());
                }
                if col == ROW_ID_COL {
                    return Err(AnalyzerError::RowIdAssignment.into());
                }
            }

            // The user can't manually set the special "row_id" the column.
            let schema_len = if metadata.schema.columns[0].name == ROW_ID_COL {
                metadata.schema.len() - 1
            } else {
                metadata.schema.len()
            };

            if schema_len != columns.len() {
                return Err(AnalyzerError::MissingColumns.into());
            }

            for (expr, col) in values.iter().zip(columns) {
                analyze_assignment(metadata, col, expr, false)?;
            }
        }

        Statement::Select {
            from,
            columns,
            r#where,
            order_by,
        } => {
            let metadata = ctx.table_metadata(from)?;

            for expr in columns {
                if expr != &Expression::Wildcard {
                    analyze_expression(&metadata.schema, None, expr)?;
                }
            }

            analyze_where(&metadata.schema, r#where)?;

            for expr in order_by {
                analyze_expression(&metadata.schema, None, expr)?;
            }
        }

        Statement::Delete { from, r#where } => {
            let metadata = ctx.table_metadata(from)?;

            if from == MKDB_META {
                return Err(AnalyzerError::MkdbMetaModification.into());
            }

            analyze_where(&metadata.schema, r#where)?;
        }

        Statement::Update {
            table,
            columns,
            r#where,
        } => {
            let metadata = ctx.table_metadata(table)?;

            if table == MKDB_META {
                return Err(AnalyzerError::MkdbMetaModification.into());
            }

            for col in columns {
                analyze_assignment(metadata, &col.identifier, &col.value, true)?;
            }

            analyze_where(&metadata.schema, r#where)?;
        }

        Statement::Explain(inner) => {
            analyze(inner, ctx)?;
        }

        Statement::Drop(Drop::Table(table)) => {
            ctx.table_metadata(table)?;
        }

        _ => {
            // Rest of statements that we support don't require any analysis.
        }
    };

    Ok(())
}

/// Makes sure that the given expression is valid and evaluates to a boolean.
fn analyze_where(schema: &Schema, r#where: &Option<Expression>) -> Result<(), DbError> {
    let Some(expr) = r#where else {
        return Ok(());
    };

    if let VmDataType::Bool = analyze_expression(schema, None, expr)? {
        return Ok(());
    };

    Err(TypeError::ExpectedType {
        expected: VmDataType::Bool,
        found: expr.clone(),
    })?
}

/// Makes sure that the expression will evaluate to a data type that can be
/// assigned to the given column.
///
/// Performs some additional checks such as VARCHAR(max) character limits.
fn analyze_assignment(
    table: &TableMetadata,
    column: &str,
    value: &Expression,
    allow_identifiers: bool,
) -> Result<(), SqlError> {
    if column == ROW_ID_COL {
        return Err(AnalyzerError::RowIdAssignment.into());
    }

    let index = table
        .schema
        .index_of(column)
        .ok_or(SqlError::InvalidColumn(column.into()))?;

    let data_type = table.schema.columns[index].data_type;

    let expected_data_type = VmDataType::from(data_type);
    let pre_eval_data_type = if allow_identifiers {
        analyze_expression(&table.schema, Some(&data_type), value)?
    } else {
        analyze_expression(&Schema::empty(), Some(&data_type), value)?
    };

    if expected_data_type != pre_eval_data_type {
        return Err(SqlError::TypeError(TypeError::ExpectedType {
            expected: expected_data_type,
            found: value.clone(),
        }));
    }

    if let DataType::Varchar(max) = data_type {
        if let Expression::Value(Value::String(string)) = value {
            if string.chars().count() > max {
                return Err(AnalyzerError::ValueTooLong(string.clone(), max).into());
            }
        };
    }

    Ok(())
}

/// Predetermines the type that an expression will evaluate to.
///
/// The expression resolver can also do that because it actually evaluates the
/// expression, but in the cases of statements with `WHERE` clauses it won't be
/// called until we loaded tuples into memory, which requires IO, so it's better
/// to do this check now.
///
/// If there are type errors or unknown columns not present in the given
/// schema then an error is returned.
pub(crate) fn analyze_expression(
    schema: &Schema,
    col_data_type: Option<&DataType>,
    expr: &Expression,
) -> Result<VmDataType, SqlError> {
    Ok(match expr {
        Expression::Value(value) => match value {
            Value::Bool(_) => VmDataType::Bool,
            Value::String(_) => VmDataType::String,
            Value::Number(num) => {
                if let Some(data_type) = col_data_type {
                    analyze_integer_range(num, data_type)?;
                }

                VmDataType::Number
            }
        },

        Expression::Identifier(ident) => {
            let index = schema
                .index_of(ident)
                .ok_or(SqlError::InvalidColumn(ident.clone()))?;

            match schema.columns[index].data_type {
                DataType::Bool => VmDataType::Bool,
                DataType::Varchar(_) => VmDataType::String,
                _ => VmDataType::Number,
            }
        }

        Expression::UnaryOperation { operator, expr } => {
            // Precompute negative numbers since the optimizer hasn't run yet.
            if let (Some(data_type), UnaryOperator::Minus, Expression::Value(Value::Number(num))) =
                (col_data_type, *operator, &**expr)
            {
                analyze_integer_range(&-num, data_type)?;
                return Ok(VmDataType::Number);
            }

            match analyze_expression(schema, col_data_type, expr)? {
                VmDataType::Number => VmDataType::Number,

                _ => Err(TypeError::ExpectedType {
                    expected: VmDataType::Number,
                    found: *expr.clone(),
                })?,
            }
        }

        Expression::BinaryOperation {
            left,
            operator,
            right,
        } => {
            let left_data_type = analyze_expression(schema, col_data_type, left)?;
            let right_data_type = analyze_expression(schema, col_data_type, right)?;

            // TODO: We're lazily evaluating this because we have to clone.
            // Figure out if we can refactor this module to avoid cloning
            // for errors.
            let mismatched_types = || {
                SqlError::TypeError(TypeError::CannotApplyBinary {
                    left: *left.clone(),
                    operator: *operator,
                    right: *right.clone(),
                })
            };

            if left_data_type != right_data_type {
                return Err(mismatched_types());
            }

            match operator {
                BinaryOperator::Eq
                | BinaryOperator::Neq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq => VmDataType::Bool,

                BinaryOperator::And | BinaryOperator::Or if left_data_type == VmDataType::Bool => {
                    VmDataType::Bool
                }

                BinaryOperator::Plus
                | BinaryOperator::Minus
                | BinaryOperator::Div
                | BinaryOperator::Mul
                    if left_data_type == VmDataType::Number =>
                {
                    VmDataType::Number
                }

                _ => Err(mismatched_types())?,
            }
        }

        Expression::Nested(expr) => analyze_expression(schema, col_data_type, expr)?,

        Expression::Wildcard => {
            return Err(SqlError::Other("unexpected wildcard expression (*)".into()))
        }
    })
}

/// Returns an error if the integer is out of range for the given data type.
fn analyze_integer_range(integer: &i128, data_type: &DataType) -> Result<(), AnalyzerError> {
    if let DataType::BigInt | DataType::Int | DataType::UnsignedBigInt | DataType::UnsignedInt =
        data_type
    {
        if !tuple::integer_is_within_range(integer, data_type) {
            return Err(AnalyzerError::IntegerOutOfRange(*integer, *data_type));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AlreadyExists, AnalyzerError};
    use crate::{
        db::{Context, DbError, SqlError},
        sql::{
            analyzer::analyze,
            parser::Parser,
            statement::{BinaryOperator, DataType, Expression, Value},
        },
        vm::{TypeError, VmDataType},
    };

    struct Analyze<'s> {
        sql: &'s str,
        ctx: &'s [&'s str],
        expected: Result<(), DbError>,
    }

    fn assert_analyze(Analyze { sql, ctx, expected }: Analyze) -> Result<(), DbError> {
        let statement = Parser::new(sql).parse_statement()?;
        let mut ctx = Context::try_from(ctx)?;

        assert_eq!(analyze(&statement, &mut ctx), expected);

        Ok(())
    }

    #[test]
    fn select_from_invalid_table() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &[],
            sql: "SELECT * FROM users;",
            expected: Err(SqlError::InvalidTable("users".into()).into()),
        })
    }

    #[test]
    fn insert_into_invalid_table() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT PRIMARY KEY);"],
            sql: "INSERT INTO tasks (id, title) VALUES (1, 'Test');",
            expected: Err(SqlError::InvalidTable("tasks".into()).into()),
        })
    }

    #[test]
    fn multiple_primary_keys() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &[],
            sql: "CREATE TABLE users (id INT PRIMARY KEY, email VARCHAR(255) PRIMARY KEY);",
            expected: Err(AnalyzerError::MultiplePrimaryKeys.into()),
        })
    }

    #[test]
    fn insert_count_mismatch() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), email VARCHAR(255) UNIQUE);"],
            sql: "INSERT INTO users (id, name, email) VALUES (1, 'John Doe');",
            expected: Err(AnalyzerError::ColumnValueCountMismatch.into()),
        })
    }

    #[test]
    fn insert_missing_columns() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), email VARCHAR(255) UNIQUE);"],
            sql: "INSERT INTO users (id, name) VALUES (1, 'John Doe');",
            expected: Err(AnalyzerError::MissingColumns.into()),
        })
    }
    #[test]
    fn insert_duplicate_columns() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), email VARCHAR(255) UNIQUE);"],
            sql: "INSERT INTO users (id, name, name) VALUES (1, 'John Doe', 'John Doe');",
            expected: Err(AnalyzerError::DuplicatedColumn("name".into()).into()),
        })
    }

    #[test]
    fn insert_wrong_data_types() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), email VARCHAR(255) UNIQUE);"],
            sql: "INSERT INTO users (id, name, email) VALUES ('string', 5, 6);",
            expected: Err(DbError::from(TypeError::ExpectedType {
                expected: VmDataType::Number,
                found: Expression::Value(Value::String("string".into()))
            })),
        })
    }

    #[test]
    fn insert_wrong_data_types_without_specifying_columns() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), email VARCHAR(255) UNIQUE);"],
            sql: "INSERT INTO users VALUES ('string', 5, 6);",
            expected: Err(DbError::from(TypeError::ExpectedType {
                expected: VmDataType::Number,
                found: Expression::Value(Value::String("string".into()))
            })),
        })
    }

    #[test]
    fn select_where_invalid_expression() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), email VARCHAR(255) UNIQUE);"],
            sql: "SELECT * FROM users WHERE id = 'string';",
            expected: Err(DbError::from(TypeError::CannotApplyBinary {
                left: Expression::Identifier("id".into()),
                operator: BinaryOperator::Eq,
                right: Expression::Value(Value::String("string".into()))
            })),
        })
    }

    #[test]
    fn select_where_doesnt_eval_to_bool() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), email VARCHAR(255) UNIQUE);"],
            sql: "SELECT * FROM users WHERE 2 + 2;",
            expected: Err(DbError::from(TypeError::ExpectedType {
                expected: VmDataType::Bool,
                found: Expression::BinaryOperation {
                    left: Box::new(Expression::Value(Value::Number(2))),
                    operator: BinaryOperator::Plus,
                    right: Box::new(Expression::Value(Value::Number(2))),
                }
             })),
        })
    }

    #[test]
    fn update_wrong_data_types() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), email VARCHAR(255) UNIQUE);"],
            sql: "UPDATE users SET id = 1, name = 'string', email = 2;",
            expected: Err(DbError::from(TypeError::ExpectedType {
                expected: VmDataType::String,
                found: Expression::Value(Value::Number(2))
            })),
        })
    }

    #[test]
    fn table_already_exists() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), email VARCHAR(255) UNIQUE);"],
            sql: "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), email VARCHAR(255) UNIQUE);",
            expected: Err(DbError::from(AnalyzerError::AlreadyExists(AlreadyExists::Table("users".into())))),
        })
    }

    #[test]
    fn row_id_assignment_on_insert() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT, name VARCHAR(8));"],
            sql: "INSERT INTO users (row_id, id, name) VALUES (1, 1, 'test');",
            expected: Err(DbError::from(AnalyzerError::RowIdAssignment)),
        })
    }

    #[test]
    fn row_id_assignment_on_update() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT, name VARCHAR(8));"],
            sql: "UPDATE users SET row_id = 5 WHERE id = 10;",
            expected: Err(DbError::from(AnalyzerError::RowIdAssignment)),
        })
    }

    #[test]
    fn value_too_long() -> Result<(), DbError> {
        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT, name VARCHAR(8));"],
            sql: "INSERT INTO users (id, name) VALUES (1, '123456789');",
            expected: Err(DbError::from(AnalyzerError::ValueTooLong(
                "123456789".into(),
                8,
            ))),
        })
    }

    #[test]
    fn integer_out_of_range() -> Result<(), DbError> {
        let integer = i128::from(i32::MAX) + 1;

        assert_analyze(Analyze {
            ctx: &["CREATE TABLE users (id INT, name VARCHAR(8));"],
            sql: &format!("INSERT INTO users (id, name) VALUES ({integer}, '1');"),
            expected: Err(DbError::from(AnalyzerError::IntegerOutOfRange(
                integer,
                DataType::Int,
            ))),
        })
    }
}
