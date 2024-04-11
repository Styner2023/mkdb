//! Generates [`Plan`] trees.
//!
//! See the module level documentation of [`crate::vm::plan`] to understand
//! what exactly we're "generating" here.

use std::{
    collections::VecDeque,
    io::{Read, Seek, Write},
    rc::Rc,
};

use super::optimizer;
use crate::{
    db::{Database, DatabaseContext, DbError, Schema, SqlError},
    paging,
    sql::{
        analyzer,
        statement::{Column, DataType, Expression, Statement},
    },
    vm::{
        plan::{
            Collect, CollectConfig, Delete, Insert, Plan, Project, Sort, SortConfig, SortKeysGen,
            TuplesComparator, Update, Values, DEFAULT_SORT_INPUT_BUFFERS,
        },
        VmDataType,
    },
};

/// Generates a query plan that's ready to execute by the VM.
pub(crate) fn generate_plan<F: Seek + Read + Write + paging::io::FileOps>(
    statement: Statement,
    db: &mut Database<F>,
) -> Result<Plan<F>, DbError> {
    Ok(match statement {
        Statement::Insert {
            into,
            columns,
            values,
        } => {
            let source = Box::new(Plan::Values(Values {
                values: VecDeque::from([values]),
            }));

            let table = db.table_metadata(&into)?.clone();

            Plan::Insert(Insert {
                source,
                comparator: table.comparator()?,
                table: db.table_metadata(&into)?.clone(),
                pager: Rc::clone(&db.pager),
            })
        }

        Statement::Select {
            columns,
            from,
            r#where,
            order_by,
        } => {
            let mut source = optimizer::generate_scan_plan(&from, r#where, db)?;

            let page_size = db.pager.borrow().page_size;

            let work_dir = db.work_dir.clone();
            let table = db.table_metadata(&from)?;

            if !order_by.is_empty()
                && order_by != [Expression::Identifier(table.schema.columns[0].name.clone())]
            {
                let mut sort_schema = table.schema.clone();
                let mut sort_keys_indexes = Vec::with_capacity(order_by.len());

                // Precompute all the sort keys indexes so that the sorter
                // doesn't waste time figuring out where the columns are.
                for expr in &order_by {
                    let index = match expr {
                        Expression::Identifier(col) => table.schema.index_of(col).unwrap(),

                        _ => {
                            let index = sort_schema.len();
                            let data_type = resolve_unknown_type(&table.schema, expr)?;
                            let col = Column::new(&format!("{expr}"), data_type);
                            sort_schema.push(col);

                            index
                        }
                    };

                    sort_keys_indexes.push(index);
                }

                // If there are no expressions that need to be evaluated for
                // sorting then just skip the sort key generation completely,
                // we already have all the sort keys we need.
                let collect_source = if sort_schema.len() > table.schema.len() {
                    Plan::SortKeysGen(SortKeysGen {
                        source: Box::new(source),
                        schema: table.schema.clone(),
                        gen_exprs: order_by
                            .into_iter()
                            .filter(|expr| !matches!(expr, Expression::Identifier(_)))
                            .collect(),
                    })
                } else {
                    source
                };

                source = Plan::Sort(Sort::from(SortConfig {
                    page_size,
                    work_dir: work_dir.clone(),
                    collection: Collect::from(CollectConfig {
                        source: Box::new(collect_source),
                        work_dir,
                        schema: sort_schema.clone(),
                        mem_buf_size: page_size,
                    }),
                    comparator: TuplesComparator {
                        schema: table.schema.clone(),
                        sort_schema,
                        sort_keys_indexes,
                    },
                    input_buffers: DEFAULT_SORT_INPUT_BUFFERS,
                }));
            }

            let mut output_schema = Schema::empty();

            for expr in &columns {
                match expr {
                    Expression::Identifier(ident) => output_schema
                        .push(table.schema.columns[table.schema.index_of(ident).unwrap()].clone()),

                    _ => {
                        output_schema.push(Column {
                            name: expr.to_string(), // TODO: AS alias
                            data_type: resolve_unknown_type(&table.schema, expr)?,
                            constraints: vec![],
                        });
                    }
                }
            }

            // No need to project if the output schema is the exact same as the
            // table schema.
            if table.schema == output_schema {
                return Ok(source);
            }

            Plan::Project(Project {
                input_schema: table.schema.clone(),
                output_schema,
                projection: columns,
                source: Box::new(source),
            })
        }

        Statement::Update {
            table,
            columns,
            r#where,
        } => {
            let mut source = optimizer::generate_scan_plan(&table, r#where, db)?;
            let work_dir = db.work_dir.clone();
            let page_size = db.pager.borrow().page_size;
            let metadata = db.table_metadata(&table)?;

            // Index scans have their own internal buffering for sorting.
            // Sequential scans plans don't, which is useful for SELECT
            // statements that don't need any buffering. Updates and deletes do
            // need buffering because BTree operations can destroy the scan
            // cursor. Maybe we can keep track of every cursor when updating the
            // BTree but as of right now it seems pretty complicated because the
            // BTree is not a self contained unit that can be passed around like
            // the pager.
            if needs_collection(&source) {
                source = Plan::Collect(Collect::from(CollectConfig {
                    source: Box::new(source),
                    work_dir,
                    schema: metadata.schema.clone(),
                    mem_buf_size: page_size,
                }));
            }

            Plan::Update(Update {
                comparator: metadata.comparator()?,
                table: metadata.clone(),
                assignments: columns,
                pager: Rc::clone(&db.pager),
                source: Box::new(source),
            })
        }

        Statement::Delete { from, r#where } => {
            let mut source = optimizer::generate_scan_plan(&from, r#where, db)?;
            let work_dir = db.work_dir.clone();
            let page_size = db.pager.borrow().page_size;
            let metadata = db.table_metadata(&from)?;

            if needs_collection(&source) {
                source = Plan::Collect(Collect::from(CollectConfig {
                    source: Box::new(source),
                    work_dir,
                    mem_buf_size: page_size,
                    schema: metadata.schema.clone(),
                }));
            }

            Plan::Delete(Delete {
                comparator: metadata.comparator()?,
                table: metadata.clone(),
                pager: Rc::clone(&db.pager),
                source: Box::new(source),
            })
        }

        other => {
            return Err(DbError::Other(format!(
                "statement {other} not yet implemeted or supported"
            )))
        }
    })
}

/// Returns a concrete [`DataType`] for an expression that hasn't been executed
/// yet.
///
/// TODO: There are no expressions that can evaluate to strings as of right now
/// since we didn't implement `CONCAT()` or any other similar function, so
/// strings can only come from identifiers. The [`analyzer`] should never return
/// [`VmDataType::String`], so it doesn't matter what type we return in that
/// case.
///
/// The real problem is when expressions evaluate to numbers becase we don't
/// know the exact kind of number. An expression with a raw value like
/// 4294967296 should evaluate to [`DataType::UnsignedBigInt`] but -65536 should
/// probably evaluate to [`DataType::Int`]. Expressions that have identifiers in
/// them should probably evaluate to the type of the identifier, but what if
/// there are multiple identifiers of different integer types? Not gonna worry
/// about this for now, this is a toy database after all :)
fn resolve_unknown_type(schema: &Schema, expr: &Expression) -> Result<DataType, SqlError> {
    Ok(match expr {
        Expression::Identifier(col) => {
            let index = schema.index_of(col).unwrap();
            schema.columns[index].data_type
        }

        _ => match analyzer::analyze_expression(schema, None, expr)? {
            VmDataType::Bool => DataType::Bool,
            VmDataType::Number => DataType::BigInt,
            VmDataType::String => DataType::Varchar(65535),
        },
    })
}

/// Returns `true` if the given plan needs collection to avoid destroying its
/// cursor.
fn needs_collection<F>(plan: &Plan<F>) -> bool {
    match plan {
        Plan::Filter(filter) => needs_collection(&filter.source),
        // KeyScan has a sorter behind it which buffers all the tuples and
        // ExactMatch only returns one tuple.
        Plan::KeyScan(_) | Plan::ExactMatch(_) => false,
        // Top-level SeqScan, RangeScan and LogicalOrScan will need collection
        // to preserve their cursor state.
        Plan::SeqScan(_) | Plan::RangeScan(_) | Plan::LogicalOrScan(_) => true,
        _ => unreachable!("needs_collection() called with plan that is not a 'scan' plan"),
    }
}

// TODO: Tests here are kinda verbose and it's hard to spot the difference
// between left and right when assert_eq! fails. There's probably some pattern
// that can help reduce clutter.
#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::{HashMap, VecDeque},
        io,
        ops::Bound,
        path::PathBuf,
        rc::Rc,
    };

    use crate::{
        db::{Database, DatabaseContext, IndexMetadata, Relation, Schema, TableMetadata},
        paging::{io::MemBuf, pager::Pager},
        sql::{
            self,
            parser::Parser,
            statement::{Column, Create, DataType, Expression, Statement, Value},
        },
        storage::{
            tuple::{self, byte_length_of_integer_type},
            Cursor, FixedSizeMemCmp,
        },
        vm::plan::{
            Collect, CollectConfig, ExactMatch, Filter, KeyScan, LogicalOrScan, Plan, Project,
            RangeScan, RangeScanConfig, SeqScan, Sort, SortConfig, SortKeysGen, TuplesComparator,
            DEFAULT_SORT_INPUT_BUFFERS,
        },
        DbError,
    };

    /// Test database context.
    struct DbCtx {
        inner: Database<MemBuf>,
        tables: HashMap<String, TableMetadata>,
        indexes: HashMap<String, IndexMetadata>,
    }

    impl DbCtx {
        fn pager(&self) -> Rc<RefCell<Pager<MemBuf>>> {
            Rc::clone(&self.inner.pager)
        }

        fn work_dir(&self) -> PathBuf {
            self.inner.work_dir.clone()
        }

        fn page_size(&self) -> usize {
            self.inner.pager.borrow().page_size
        }
    }

    fn init_db(ctx: &[&str]) -> Result<DbCtx, DbError> {
        let mut pager = Pager::<MemBuf>::builder().wrap(io::Cursor::new(Vec::<u8>::new()));
        pager.init()?;

        let mut db = Database::new(Rc::new(RefCell::new(pager)), PathBuf::new());

        let mut tables = HashMap::new();
        let mut indexes = HashMap::new();

        let mut fetch_tables = Vec::new();

        for sql in ctx {
            if let Statement::Create(Create::Table { name, .. }) =
                Parser::new(sql).parse_statement()?
            {
                fetch_tables.push(name);
            }

            db.exec(sql)?;
        }

        for table_name in fetch_tables {
            let table = db.table_metadata(&table_name)?;

            for index in &table.indexes {
                indexes.insert(index.name.to_owned(), index.to_owned());
            }

            tables.insert(table_name, table.to_owned());
        }

        Ok(DbCtx {
            inner: db,
            tables,
            indexes,
        })
    }

    fn gen_plan(db: &mut DbCtx, query: &str) -> Result<Plan<MemBuf>, DbError> {
        let statement = sql::pipeline(query, &mut db.inner)?;
        super::generate_plan(statement, &mut db.inner)
    }

    fn parse_expr(expr: &str) -> Expression {
        let mut expr = Parser::new(expr).parse_expression().unwrap();
        sql::optimizer::simplify(&mut expr).unwrap();

        expr
    }

    #[test]
    fn generate_basic_sequential_scan() -> Result<(), DbError> {
        let mut db = init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255));"])?;

        assert_eq!(
            gen_plan(&mut db, "SELECT * FROM users;")?,
            Plan::SeqScan(SeqScan {
                pager: db.pager(),
                cursor: Cursor::new(db.tables["users"].root, 0),
                table: db.tables["users"].to_owned(),
            })
        );

        Ok(())
    }

    #[test]
    fn generate_sequential_scan_with_filter() -> Result<(), DbError> {
        let mut db =
            init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), age INT);"])?;

        assert_eq!(
            gen_plan(&mut db, "SELECT * FROM users WHERE age >= 20;")?,
            Plan::Filter(Filter {
                filter: parse_expr("age >= 20"),
                schema: db.tables["users"].schema.to_owned(),
                source: Box::new(Plan::SeqScan(SeqScan {
                    pager: db.pager(),
                    cursor: Cursor::new(db.tables["users"].root, 0),
                    table: db.tables["users"].to_owned(),
                }))
            })
        );

        Ok(())
    }

    // Tables with no primary key have a special "row_id" column.
    #[test]
    fn generate_sequential_scan_with_projection_when_using_row_id() -> Result<(), DbError> {
        let mut db = init_db(&["CREATE TABLE users (id INT, name VARCHAR(255));"])?;

        assert_eq!(
            gen_plan(&mut db, "SELECT * FROM users;")?,
            Plan::Project(Project {
                input_schema: db.tables["users"].schema.to_owned(),
                output_schema: Schema::new(vec![
                    Column::new("id", DataType::Int),
                    Column::new("name", DataType::Varchar(255))
                ]),
                projection: vec![
                    Expression::Identifier("id".into()),
                    Expression::Identifier("name".into())
                ],
                source: Box::new(Plan::SeqScan(SeqScan {
                    pager: db.pager(),
                    cursor: Cursor::new(db.tables["users"].root, 0),
                    table: db.tables["users"].to_owned(),
                }))
            })
        );

        Ok(())
    }

    // Tables with no primary key have a special "row_id" column.
    #[test]
    fn generate_sequential_scan_with_projection_when_selecting_columns() -> Result<(), DbError> {
        let mut db = init_db(&[
            "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), email VARCHAR(255));",
        ])?;

        assert_eq!(
            gen_plan(&mut db, "SELECT email, id FROM users;")?,
            Plan::Project(Project {
                input_schema: db.tables["users"].schema.to_owned(),
                output_schema: Schema::new(vec![
                    Column::new("email", DataType::Varchar(255)),
                    Column::primary_key("id", DataType::Int),
                ]),
                projection: vec![
                    Expression::Identifier("email".into()),
                    Expression::Identifier("id".into()),
                ],
                source: Box::new(Plan::SeqScan(SeqScan {
                    cursor: Cursor::new(db.tables["users"].root, 0),
                    table: db.tables["users"].to_owned(),
                    pager: db.pager()
                }))
            })
        );

        Ok(())
    }

    #[test]
    fn generate_basic_sequential_scan_with_filter_and_projection() -> Result<(), DbError> {
        let mut db =
            init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), age INT);"])?;

        assert_eq!(
            gen_plan(&mut db, "SELECT name FROM users WHERE age >= 20;")?,
            Plan::Project(Project {
                input_schema: db.tables["users"].schema.to_owned(),
                output_schema: Schema::new(vec![Column::new("name", DataType::Varchar(255))]),
                projection: vec![Expression::Identifier("name".into())],
                source: Box::new(Plan::Filter(Filter {
                    filter: parse_expr("age >= 20"),
                    schema: db.tables["users"].schema.to_owned(),
                    source: Box::new(Plan::SeqScan(SeqScan {
                        cursor: Cursor::new(db.tables["users"].root, 0),
                        table: db.tables["users"].to_owned(),
                        pager: db.pager()
                    }))
                }))
            })
        );

        Ok(())
    }

    #[test]
    fn generate_exact_match_on_auto_index() -> Result<(), DbError> {
        let mut db = init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255));"])?;

        assert_eq!(
            gen_plan(&mut db, "SELECT * FROM users WHERE id = 5;")?,
            Plan::ExactMatch(ExactMatch {
                emit_table_key_only: false,
                key: tuple::serialize_key(&DataType::Int, &Value::Number(5)),
                expr: parse_expr("id = 5"),
                pager: db.pager(),
                relation: Relation::Table(db.tables["users"].to_owned()),
                done: false,
            })
        );

        Ok(())
    }

    #[test]
    fn generate_exact_match_on_external_index() -> Result<(), DbError> {
        let mut db =
            init_db(&["CREATE TABLE users (id INT PRIMARY KEY, email VARCHAR(255) UNIQUE);"])?;

        assert_eq!(
            gen_plan(
                &mut db,
                "SELECT * FROM users WHERE email = 'bob@email.com';"
            )?,
            Plan::KeyScan(KeyScan {
                pager: db.pager(),
                comparator: FixedSizeMemCmp(byte_length_of_integer_type(&DataType::Int)),
                table: db.tables["users"].to_owned(),
                source: Box::new(Plan::ExactMatch(ExactMatch {
                    emit_table_key_only: true,
                    pager: db.pager(),
                    relation: Relation::Index(db.indexes["users_email_uq_index"].to_owned()),
                    expr: parse_expr("email = 'bob@email.com'"),
                    key: tuple::serialize_key(
                        &DataType::Varchar(255),
                        &Value::String("bob@email.com".into())
                    ),
                    done: false,
                }))
            })
        );

        Ok(())
    }

    #[test]
    fn generate_range_on_auto_index() -> Result<(), DbError> {
        let mut db = init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255));"])?;

        assert_eq!(
            gen_plan(&mut db, "SELECT * FROM users WHERE id > 5 AND id < 10;")?,
            Plan::RangeScan(RangeScan::from(RangeScanConfig {
                emit_table_key_only: false,
                expr: parse_expr("id > 5 AND id < 10"),
                pager: db.pager(),
                range: (
                    Bound::Excluded(tuple::serialize_key(&DataType::Int, &Value::Number(5))),
                    Bound::Excluded(tuple::serialize_key(&DataType::Int, &Value::Number(10))),
                ),
                relation: Relation::Table(db.tables["users"].to_owned())
            }))
        );

        Ok(())
    }

    #[test]
    fn generate_range_on_external_index() -> Result<(), DbError> {
        let mut db = init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), email VARCHAR(255) UNIQUE);"])?;

        let key_only_schema = db.tables["users"].key_only_schema();

        assert_eq!(
            gen_plan(
                &mut db,
                "SELECT * FROM users WHERE email <= 'test@test.com';"
            )?,
            Plan::KeyScan(KeyScan {
                comparator: FixedSizeMemCmp(byte_length_of_integer_type(&DataType::Int)),
                table: db.tables["users"].to_owned(),
                pager: db.pager(),
                source: Box::new(Plan::Sort(Sort::from(SortConfig {
                    input_buffers: DEFAULT_SORT_INPUT_BUFFERS,
                    page_size: db.page_size(),
                    work_dir: db.work_dir(),
                    comparator: TuplesComparator {
                        schema: key_only_schema.clone(),
                        sort_schema: key_only_schema.clone(),
                        sort_keys_indexes: vec![0],
                    },
                    collection: Collect::from(CollectConfig {
                        mem_buf_size: db.page_size(),
                        schema: key_only_schema,
                        work_dir: db.work_dir(),
                        source: Box::new(Plan::RangeScan(RangeScan::from(RangeScanConfig {
                            emit_table_key_only: true,
                            expr: parse_expr("email <= 'test@test.com'"),
                            pager: db.pager(),
                            range: (
                                Bound::Unbounded,
                                Bound::Included(tuple::serialize_key(
                                    &DataType::Varchar(255),
                                    &Value::String("test@test.com".into())
                                )),
                            ),
                            relation: Relation::Index(
                                db.indexes["users_email_uq_index"].to_owned()
                            )
                        })))
                    })
                })))
            })
        );

        Ok(())
    }

    #[test]
    fn skip_filter_on_simple_range_scan() -> Result<(), DbError> {
        let mut db = init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255));"])?;

        assert_eq!(
            gen_plan(&mut db, "SELECT * FROM users WHERE id < 5;")?,
            Plan::RangeScan(RangeScan::from(RangeScanConfig {
                emit_table_key_only: false,
                pager: db.pager(),
                relation: Relation::Table(db.tables["users"].to_owned()),
                expr: parse_expr("id < 5"),
                range: (
                    Bound::Unbounded,
                    Bound::Excluded(tuple::serialize_key(&DataType::Int, &Value::Number(5)))
                ),
            }))
        );

        Ok(())
    }

    #[test]
    fn apply_filter_if_cant_be_skipped() -> Result<(), DbError> {
        let mut db = init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255));"])?;

        assert_eq!(
            gen_plan(
                &mut db,
                "SELECT * FROM users WHERE id < 5 AND name = 'Bob';"
            )?,
            Plan::Filter(Filter {
                filter: parse_expr("name = 'Bob'"),
                schema: db.tables["users"].schema.to_owned(),
                source: Box::new(Plan::RangeScan(RangeScan::from(RangeScanConfig {
                    emit_table_key_only: false,
                    pager: db.pager(),
                    expr: parse_expr("id < 5"),
                    relation: Relation::Table(db.tables["users"].to_owned()),
                    range: (
                        Bound::Unbounded,
                        Bound::Excluded(tuple::serialize_key(&DataType::Int, &Value::Number(5)))
                    )
                })))
            })
        );

        Ok(())
    }

    #[test]
    fn decompose_filter_on_and_scans() -> Result<(), DbError> {
        let mut db = init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255));"])?;

        assert_eq!(
            gen_plan(
                &mut db,
                "SELECT * FROM users WHERE id < 10 AND name = 'test';"
            )?,
            Plan::Filter(Filter {
                filter: parse_expr("name = 'test'"),
                schema: db.tables["users"].schema.to_owned(),
                source: Box::new(Plan::RangeScan(RangeScan::from(RangeScanConfig {
                    emit_table_key_only: false,
                    pager: db.pager(),
                    relation: Relation::Table(db.tables["users"].to_owned()),
                    expr: parse_expr("id < 10"),
                    range: (
                        Bound::Unbounded,
                        Bound::Excluded(tuple::serialize_key(&DataType::Int, &Value::Number(10)))
                    ),
                }))),
            })
        );

        Ok(())
    }

    #[test]
    fn fallback_to_seq_scan_when_union_of_ranges_is_fully_unbounded() -> Result<(), DbError> {
        let mut db =
            init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), age INT);"])?;

        assert_eq!(
            gen_plan(
                &mut db,
                "SELECT * FROM users WHERE (id > 5 OR id < 10) OR id > 15;"
            )?,
            Plan::Filter(Filter {
                filter: parse_expr("(id > 5 OR id < 10) OR id > 15"),
                schema: db.tables["users"].schema.to_owned(),
                source: Box::new(Plan::SeqScan(SeqScan {
                    pager: db.pager(),
                    cursor: Cursor::new(db.tables["users"].root, 0),
                    table: db.tables["users"].to_owned(),
                }))
            })
        );

        Ok(())
    }

    #[test]
    fn fallback_to_seq_scan_when_intersection_of_ranges_cancels_out() -> Result<(), DbError> {
        let mut db =
            init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), age INT);"])?;

        assert_eq!(
            gen_plan(
                &mut db,
                "SELECT * FROM users WHERE (id < 5 OR id > 10) AND id = 7;"
            )?,
            Plan::Filter(Filter {
                filter: parse_expr("(id < 5 OR id > 10) AND id = 7"),
                schema: db.tables["users"].schema.to_owned(),
                source: Box::new(Plan::SeqScan(SeqScan {
                    pager: db.pager(),
                    cursor: Cursor::new(db.tables["users"].root, 0),
                    table: db.tables["users"].to_owned(),
                }))
            })
        );

        Ok(())
    }

    #[test]
    fn generate_simple_sort_plan() -> Result<(), DbError> {
        let mut db =
            init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), age INT);"])?;

        assert_eq!(
            gen_plan(&mut db, "SELECT * FROM users ORDER by name, age;")?,
            Plan::Sort(Sort::from(SortConfig {
                page_size: db.page_size(),
                work_dir: db.work_dir(),
                input_buffers: DEFAULT_SORT_INPUT_BUFFERS,
                comparator: TuplesComparator {
                    schema: db.tables["users"].schema.to_owned(),
                    sort_schema: db.tables["users"].schema.to_owned(),
                    sort_keys_indexes: vec![1, 2],
                },
                collection: Collect::from(CollectConfig {
                    mem_buf_size: db.page_size(),
                    schema: db.tables["users"].schema.clone(),
                    work_dir: db.work_dir(),
                    source: Box::new(Plan::SeqScan(SeqScan {
                        pager: db.pager(),
                        cursor: Cursor::new(db.tables["users"].root, 0),
                        table: db.tables["users"].to_owned(),
                    }))
                })
            }))
        );

        Ok(())
    }

    #[test]
    fn generate_sort_plan_with_expressions() -> Result<(), DbError> {
        let mut db = init_db(&[
            "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), age INT, followers INT);",
        ])?;

        let mut sort_schema = db.tables["users"].schema.to_owned();
        sort_schema.push(Column::new("age + 10", DataType::BigInt));
        sort_schema.push(Column::new("followers * 2", DataType::BigInt));

        assert_eq!(
            gen_plan(
                &mut db,
                "SELECT * FROM users ORDER by name, age + 10, followers * 2;"
            )?,
            Plan::Sort(Sort::from(SortConfig {
                page_size: db.page_size(),
                work_dir: db.work_dir(),
                input_buffers: DEFAULT_SORT_INPUT_BUFFERS,
                comparator: TuplesComparator {
                    schema: db.tables["users"].schema.to_owned(),
                    sort_schema: sort_schema.clone(),
                    sort_keys_indexes: vec![1, 4, 5],
                },
                collection: Collect::from(CollectConfig {
                    mem_buf_size: db.page_size(),
                    schema: sort_schema.clone(),
                    work_dir: db.work_dir(),
                    source: Box::new(Plan::SortKeysGen(SortKeysGen {
                        gen_exprs: vec![parse_expr("age + 10"), parse_expr("followers * 2")],
                        schema: db.tables["users"].schema.to_owned(),
                        source: Box::new(Plan::SeqScan(SeqScan {
                            pager: db.pager(),
                            cursor: Cursor::new(db.tables["users"].root, 0),
                            table: db.tables["users"].to_owned(),
                        }))
                    }))
                })
            }))
        );

        Ok(())
    }

    #[test]
    fn skip_sorting_when_order_by_key_only() -> Result<(), DbError> {
        let mut db = init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255));"])?;

        assert_eq!(
            gen_plan(&mut db, "SELECT * FROM users ORDER BY id;")?,
            Plan::SeqScan(SeqScan {
                pager: db.pager(),
                cursor: Cursor::new(db.tables["users"].root, 0),
                table: db.tables["users"].to_owned(),
            })
        );

        Ok(())
    }

    #[test]
    fn generate_logical_or_scan_plan() -> Result<(), DbError> {
        let mut db =
            init_db(&["CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255), email VARCHAR(255) UNIQUE);"])?;

        #[rustfmt::skip]
        let expr = "
            (id > 5 AND id <= 10)
            OR (id >= 50 AND id < 60)
            OR id = 100
            OR email < 'b@b.com'
            OR email > 't@t.com'
            OR email = 'f@f.com'
        ";

        let sql = format!("SELECT * FROM users WHERE {expr};");

        let key_only_schema = db.tables["users"].key_only_schema();

        let expected_scans = [
            Plan::RangeScan(RangeScan::from(RangeScanConfig {
                emit_table_key_only: true,
                expr: parse_expr("id > 5 AND id <= 10"),
                pager: db.pager(),
                relation: Relation::Table(db.tables["users"].to_owned()),
                range: (
                    Bound::Excluded(tuple::serialize_key(&DataType::Int, &Value::Number(5))),
                    Bound::Included(tuple::serialize_key(&DataType::Int, &Value::Number(10))),
                ),
            })),
            Plan::RangeScan(RangeScan::from(RangeScanConfig {
                emit_table_key_only: true,
                expr: parse_expr("id >= 50 AND id < 60"),
                pager: db.pager(),
                relation: Relation::Table(db.tables["users"].to_owned()),
                range: (
                    Bound::Included(tuple::serialize_key(&DataType::Int, &Value::Number(50))),
                    Bound::Excluded(tuple::serialize_key(&DataType::Int, &Value::Number(60))),
                ),
            })),
            Plan::ExactMatch(ExactMatch {
                emit_table_key_only: true,
                done: false,
                expr: parse_expr("id = 100"),
                key: tuple::serialize_key(&DataType::Int, &Value::Number(100)),
                pager: db.pager(),
                relation: Relation::Table(db.tables["users"].to_owned()),
            }),
            Plan::RangeScan(RangeScan::from(RangeScanConfig {
                emit_table_key_only: true,
                expr: parse_expr("email < 'b@b.com'"),
                pager: db.pager(),
                relation: Relation::Index(db.indexes["users_email_uq_index"].to_owned()),
                range: (
                    Bound::Unbounded,
                    Bound::Excluded(tuple::serialize_key(
                        &DataType::Varchar(255),
                        &Value::String("b@b.com".into()),
                    )),
                ),
            })),
            Plan::ExactMatch(ExactMatch {
                emit_table_key_only: true,
                done: false,
                expr: parse_expr("email = 'f@f.com'"),
                key: tuple::serialize_key(
                    &DataType::Varchar(255),
                    &Value::String("f@f.com".into()),
                ),
                pager: db.pager(),
                relation: Relation::Index(db.indexes["users_email_uq_index"].to_owned()),
            }),
            Plan::RangeScan(RangeScan::from(RangeScanConfig {
                emit_table_key_only: true,
                expr: parse_expr("email > 't@t.com'"),
                pager: db.pager(),
                relation: Relation::Index(db.indexes["users_email_uq_index"].to_owned()),
                range: (
                    Bound::Excluded(tuple::serialize_key(
                        &DataType::Varchar(255),
                        &Value::String("t@t.com".into()),
                    )),
                    Bound::Unbounded,
                ),
            })),
        ];

        assert_eq!(
            gen_plan(&mut db, &sql)?,
            Plan::Filter(Filter {
                filter: parse_expr(expr),
                schema: db.tables["users"].schema.to_owned(),
                source: Box::new(Plan::KeyScan(KeyScan {
                    comparator: FixedSizeMemCmp(byte_length_of_integer_type(&DataType::Int)),
                    table: db.tables["users"].to_owned(),
                    pager: db.pager(),
                    source: Box::new(Plan::Sort(Sort::from(SortConfig {
                        comparator: TuplesComparator {
                            schema: key_only_schema.clone(),
                            sort_schema: key_only_schema.clone(),
                            sort_keys_indexes: vec![0],
                        },
                        input_buffers: DEFAULT_SORT_INPUT_BUFFERS,
                        work_dir: db.work_dir(),
                        page_size: db.page_size(),
                        collection: Collect::from(CollectConfig {
                            mem_buf_size: db.page_size(),
                            work_dir: db.work_dir(),
                            schema: key_only_schema,
                            source: Box::new(Plan::LogicalOrScan(LogicalOrScan {
                                scans: VecDeque::from(expected_scans)
                            }))
                        })
                    })))
                }))
            })
        );

        Ok(())
    }
}
