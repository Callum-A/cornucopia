use crate::{
    parser::{error::ValidationError, NullableColumn, Parsed, ParsedQuery},
    read_queries::Module,
    type_registrar::CornucopiaType,
    type_registrar::TypeRegistrar,
};
use error::Error;
use error::ErrorVariant;
use heck::ToUpperCamelCase;
use indexmap::{map::Entry, IndexMap};
use postgres::Client;
use postgres_types::Kind;

/// This data structure is used by Cornucopia to generate all constructs related to this particular query.
#[derive(Debug, Clone)]
pub(crate) struct PreparedQuery {
    pub(crate) name: String,
    pub(crate) params: Vec<PreparedField>,
    pub(crate) row: Option<(usize, Vec<usize>)>, // None if execute
    pub(crate) sql: String,
}

/// A row or params field
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedField {
    pub(crate) name: String,
    pub(crate) ty: CornucopiaType,
    pub(crate) is_nullable: bool,
    pub(crate) is_inner_nullable: bool, // Vec only
}

/// A params struct
#[derive(Debug, Clone)]
pub(crate) struct PreparedParams {
    pub(crate) name: String,
    pub(crate) fields: Vec<PreparedField>,
    pub(crate) queries: Vec<usize>,
}

/// A returned row
#[derive(Debug, Clone)]
pub(crate) struct PreparedRow {
    pub(crate) name: String,
    pub(crate) fields: Vec<PreparedField>,
    pub(crate) is_copy: bool,
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub(crate) enum PreparedType {
    Enum(Vec<String>),
    Domain(PreparedField),
    Composite(Vec<PreparedField>),
}

/// A struct containing the module name and the list of all
/// the queries it contains.
#[derive(Debug, Clone)]
pub(crate) struct PreparedModule {
    pub(crate) name: String,
    pub(crate) queries: IndexMap<String, PreparedQuery>,
    pub(crate) params: IndexMap<String, PreparedParams>,
    pub(crate) rows: IndexMap<String, PreparedRow>,
}

impl PreparedModule {
    fn add_row(
        &mut self,
        name: Parsed<String>,
        fields: Vec<PreparedField>,
    ) -> Result<(usize, Vec<usize>), ErrorVariant> {
        assert!(!fields.is_empty());
        match self.rows.entry(name.value.clone()) {
            Entry::Occupied(o) => {
                let prev = &o.get().fields;

                // If the row doesn't contain the same fields as a previously
                // registered row with the same name...
                if prev.len() != fields.len() || !prev.iter().all(|f| fields.contains(f)) {
                    return Err(ErrorVariant::Validation(
                        ValidationError::NamedRowInvalidFields {
                            expected: prev.clone(),
                            actual: fields,
                            name: name.value,
                            pos: name.pos,
                        },
                    ));
                }

                let indexes: Option<Vec<_>> = prev
                    .iter()
                    .map(|f| fields.iter().position(|it| it == f))
                    .collect();
                Ok((o.index(), indexes.unwrap()))
            }
            Entry::Vacant(v) => {
                let is_copy = fields.iter().all(|f| f.ty.is_copy());
                let mut tmp = fields.to_vec();
                tmp.sort_unstable_by(|a, b| a.name.cmp(&b.name));
                v.insert(PreparedRow {
                    name: name.value.clone(),
                    fields: tmp,
                    is_copy,
                });
                self.add_row(name, fields)
            }
        }
    }

    fn add_query(
        &mut self,
        name: Parsed<String>,
        params: Vec<PreparedField>,
        row_idx: Option<(usize, Vec<usize>)>,
        sql: String,
    ) -> Result<usize, ErrorVariant> {
        match self.queries.entry(name.value.clone()) {
            Entry::Occupied(_o) => Err(ErrorVariant::Validation(
                ValidationError::QueryNameAlreadyUsed {
                    name: name.value,
                    pos: name.pos,
                },
            )),
            Entry::Vacant(v) => {
                let index = v.index();
                v.insert(PreparedQuery {
                    name: name.value,
                    params,
                    row: row_idx,
                    sql,
                });
                Ok(index)
            }
        }
    }

    fn add_params(
        &mut self,
        name: Parsed<String>,
        query_idx: usize,
    ) -> Result<usize, ErrorVariant> {
        let params = &self.queries.get_index(query_idx).unwrap().1.params;
        assert!(!params.is_empty());

        match self.params.entry(name.value.clone()) {
            Entry::Occupied(mut o) => {
                let prev = o.get_mut();
                // If the param struct doesn't contain the same fields as a previously
                // registered param struct with the same name...
                if prev.fields.len() != params.len()
                    || !prev.fields.iter().all(|f| params.contains(f))
                {
                    return Err(ErrorVariant::Validation(
                        ValidationError::NamedParamStructInvalidFields {
                            name: name.value,
                            pos: name.pos,
                            expected: prev.fields.clone(),
                            actual: params.clone(),
                        },
                    ));
                }
                prev.queries.push(query_idx);
                Ok(o.index())
            }
            Entry::Vacant(v) => {
                let mut fields = params.to_vec();
                fields.sort_unstable_by(|a, b| a.name.cmp(&b.name));
                let index = v.index();
                v.insert(PreparedParams {
                    name: name.value,
                    fields,
                    queries: vec![query_idx],
                });
                Ok(index)
            }
        }
    }
}

fn has_duplicate<T, U>(
    iter: T,
    mapper: fn(<T as IntoIterator>::Item) -> U,
) -> Option<<T as IntoIterator>::Item>
where
    T: IntoIterator + Clone,
    U: Eq + std::hash::Hash + Clone,
{
    let mut uniq = std::collections::HashSet::new();
    iter.clone()
        .into_iter()
        .zip(iter.into_iter().map(mapper))
        .find(|(_, u)| !uniq.insert(u.clone()))
        .map(|(t, _)| t)
}

/// Prepares all modules
pub(crate) fn prepare(
    client: &mut Client,
    type_registrar: &mut TypeRegistrar,
    modules: Vec<Module>,
) -> Result<
    (
        Vec<PreparedModule>,
        IndexMap<(String, String), PreparedType>,
    ),
    Error,
> {
    let mut prepared_modules = Vec::new();
    for module in modules {
        prepared_modules.push(prepare_module(client, module, type_registrar)?);
    }
    let prepared_types = type_registrar
        .types
        .iter()
        .filter_map(|(key, ty)| {
            if let CornucopiaType::Custom { pg_ty, .. } = ty {
                Some((
                    key.clone(),
                    match pg_ty.kind() {
                        Kind::Enum(variants) => PreparedType::Enum(variants.to_vec()),
                        Kind::Domain(inner) => {
                            PreparedType::Domain(PreparedField {
                                name: "inner".to_string(),
                                ty: type_registrar.get(inner).unwrap().clone(),
                                is_nullable: false,
                                is_inner_nullable: false, // TODO used when support null everywhere
                            })
                        }
                        Kind::Composite(fields) => PreparedType::Composite(
                            fields
                                .iter()
                                .map(|field| {
                                    PreparedField {
                                        name: field.name().to_string(),
                                        ty: type_registrar.get(field.type_()).unwrap().clone(),
                                        is_nullable: false, // TODO used when support null everywhere
                                        is_inner_nullable: false, // TODO used when support null everywhere
                                    }
                                })
                                .collect(),
                        ),
                        _ => unreachable!(),
                    },
                ))
            } else {
                None
            }
        })
        .collect();
    Ok((prepared_modules, prepared_types))
}

/// Prepares all queries in this module
fn prepare_module(
    client: &mut Client,
    module: Module,
    type_registrar: &mut TypeRegistrar,
) -> Result<PreparedModule, Error> {
    let mut tmp = PreparedModule {
        name: module.name,
        queries: IndexMap::new(),
        params: IndexMap::new(),
        rows: IndexMap::new(),
    };
    for query in module.queries {
        prepare_query(client, &mut tmp, type_registrar, query, &module.path)?;
    }
    Ok(tmp)
}

/// Prepares a query
fn prepare_query(
    client: &mut Client,
    module: &mut PreparedModule,
    type_registrar: &mut TypeRegistrar,
    query: ParsedQuery,
    module_path: &str,
) -> Result<(), Error> {
    // Prepare the statement
    let stmt = client
        .prepare(&query.sql_str)
        .map_err(|e| Error::new(e, &query, module_path))?;

    // Get parameter parameters
    let mut params = Vec::new();
    for (name, ty) in query.params.iter().zip(stmt.params().iter()) {
        // Register type
        let ty = type_registrar
            .register(ty)
            .map_err(|e| Error::new(e, &query, module_path))?;
        let name = name.value.to_owned();
        params.push(PreparedField {
            name,
            ty: ty.to_owned(),
            is_nullable: false,       // TODO used when support null everywhere
            is_inner_nullable: false, // TODO used when support null everywhere
        });
    }

    // Get return columns
    let stmt_cols = stmt.columns();
    // Check for duplicate names
    if let Some(duplicate_col) = has_duplicate(stmt_cols.iter(), |col| col.name()) {
        return Err(Error::new(
            ErrorVariant::ColumnNameAlreadyTaken {
                name: duplicate_col.name().to_owned(),
            },
            &query,
            module_path,
        ));
    };

    // Nullable columns
    let mut nullable_cols = Vec::new();
    for nullable_col in query.nullable_columns {
        match &nullable_col.value {
            crate::parser::NullableColumn::Index(index) => {
                // Get name from column index
                let name = if let Some(col) = stmt_cols.get(*index as usize - 1) {
                    col.name()
                } else {
                    return Err(Error {
                        err: ErrorVariant::Validation(
                            ValidationError::InvalidNullableColumnIndex {
                                index: *index as usize,
                                max_col_index: stmt_cols.len(),
                                pos: nullable_col.pos,
                            },
                        ),
                        query_name: query.name.value,
                        query_start_line: Some(query.line),
                        path: module_path.to_owned(),
                    });
                };

                // Check if `nullable_cols` already contains this column. If not, add it.
                if let Some((p, n)) = nullable_cols
                    .iter()
                    .find(|(_, n): &&(Parsed<NullableColumn>, String)| n == name)
                {
                    return Err(Error {
                        err: ErrorVariant::Validation(ValidationError::ColumnAlreadyNullable {
                            name: n.to_owned(),
                            pos: p.pos.clone(),
                        }),
                        query_name: query.name.value,
                        query_start_line: Some(query.line),
                        path: module_path.to_owned(),
                    });
                } else {
                    nullable_cols.push((nullable_col, name.to_owned()))
                }
            }
            crate::parser::NullableColumn::Named(name) => {
                // Check that the nullable column's name corresponds to one of the returned columns'.
                if stmt_cols.iter().any(|y| y.name() == name) {
                    nullable_cols.push((nullable_col.clone(), name.to_owned()))
                } else {
                    return Err(Error {
                        err: ErrorVariant::Validation(ValidationError::InvalidNullableColumnName {
                            name: name.to_owned(),
                            pos: nullable_col.pos,
                        }),
                        query_name: query.name.value.clone(),
                        query_start_line: Some(query.line),
                        path: module_path.to_owned(),
                    });
                };
            }
        }
    }

    // Now that we know all the nullable columns by name, check if there are duplicates.
    if let Some((p, u)) = has_duplicate(nullable_cols.iter(), |(_, n)| n) {
        return Err(Error {
            query_name: query.name.value,
            query_start_line: Some(query.line),
            err: ErrorVariant::Validation(ValidationError::ColumnAlreadyNullable {
                name: u.to_owned(),
                pos: p.pos.clone(),
            }),
            path: module_path.to_owned(),
        });
    };

    // Get return columns
    let mut row_fields = Vec::new();
    for column in stmt_cols {
        let ty = type_registrar.register(column.type_()).map_err(|e| Error {
            query_start_line: Some(query.line),
            err: e.into(),
            path: String::from(module_path),
            query_name: query.name.value.clone(),
        })?;
        let name = column.name().to_owned();
        let is_nullable = nullable_cols.iter().any(|(_, n)| *n == name);
        row_fields.push(PreparedField {
            is_nullable,
            is_inner_nullable: false, // TODO used when support null everywhere
            name,
            ty: ty.clone(),
        });
    }

    let row_struct_name = query
        .named_return_struct
        .unwrap_or_else(|| query.name.map(|x| x.to_upper_camel_case()));
    let param_struct_name = query
        .named_param_struct
        .unwrap_or_else(|| query.name.map(|x| x.to_upper_camel_case() + "Params"));

    let row_idx = if !row_fields.is_empty() {
        Some(
            module
                .add_row(row_struct_name, row_fields)
                .map_err(|e| Error {
                    err: e,
                    query_name: query.name.value.clone(),
                    query_start_line: Some(query.line),
                    path: module_path.to_owned(),
                })?,
        )
    } else {
        None
    };

    let params_not_empty = !params.is_empty();

    let query_idx = module
        .add_query(query.name.clone(), params, row_idx, query.sql_str)
        .map_err(|e| Error {
            err: e,
            query_name: query.name.value.clone(),
            query_start_line: Some(query.line),
            path: module_path.to_owned(),
        })?;
    if params_not_empty {
        module
            .add_params(param_struct_name, query_idx)
            .map_err(|e| Error {
                err: e,
                query_name: query.name.value.clone(),
                query_start_line: Some(query.line),
                path: module_path.to_owned(),
            })?;
    }

    Ok(())
}

pub(crate) mod error {
    use std::fmt::Display;

    use crate::parser::{error::ValidationError, ParsedQuery};
    use crate::type_registrar::error::Error as PostgresTypeError;
    use thiserror::Error as ThisError;

    #[derive(Debug, ThisError)]
    #[error("{0}")]
    pub(crate) enum ErrorVariant {
        Db(#[from] postgres::Error),
        PostgresType(#[from] PostgresTypeError),
        Validation(#[from] ValidationError),
        #[error("Two or more columns have the same name: `{name}`. Consider disambiguing the column names with `AS` clauses.")]
        ColumnNameAlreadyTaken {
            name: String,
        },
    }

    #[derive(Debug)]
    pub struct Error {
        pub(crate) query_name: String,
        pub(crate) query_start_line: Option<usize>,
        pub(crate) err: ErrorVariant,
        pub(crate) path: String,
    }

    impl Error {
        pub(crate) fn new<E: Into<ErrorVariant>>(err: E, query: &ParsedQuery, path: &str) -> Self {
            Self {
                query_start_line: Some(query.line),
                err: err.into(),
                path: String::from(path),
                query_name: query.name.value.clone(),
            }
        }
    }

    impl Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match &self.err {
                ErrorVariant::Db(e) => write!(
                    f,
                    "Error while preparing query \"{}\" [file: \"{}\", line: {}] ({})",
                    self.query_name,
                    self.path,
                    self.query_start_line.unwrap_or_default(),
                    e.as_db_error().unwrap().message()
                ),
                _ => match self.query_start_line {
                    Some(line) => {
                        write!(
                            f,
                            "Error while preparing query \"{}\" [file: \"{}\", line: {}]:\n{}",
                            self.query_name, self.path, line, self.err
                        )
                    }
                    None => {
                        write!(
                            f,
                            "Error while preparing query \"{}\" [file: \"{}\"]: {}",
                            self.query_name, self.path, self.err
                        )
                    }
                },
            }
        }
    }

    impl std::error::Error for Error {}
}
