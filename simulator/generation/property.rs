use limbo_core::LimboError;
use serde::{Deserialize, Serialize};

use crate::{
    model::{
        query::{Create, Delete, Insert, Predicate, Query, Select},
        table::Value,
    },
    runner::env::SimulatorEnv,
};

use super::{
    frequency, pick, pick_index,
    plan::{Assertion, Interaction, InteractionStats, ResultSet},
    ArbitraryFrom,
};

/// Properties are representations of executable specifications
/// about the database behavior.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Property {
    /// Insert-Select is a property in which the inserted row
    /// must be in the resulting rows of a select query that has a
    /// where clause that matches the inserted row.
    /// The execution of the property is as follows
    ///     INSERT INTO <t> VALUES (...)
    ///     I_0
    ///     I_1
    ///     ...
    ///     I_n
    ///     SELECT * FROM <t> WHERE <predicate>
    /// The interactions in the middle has the following constraints;
    /// - There will be no errors in the middle interactions.
    /// - The inserted row will not be deleted.
    /// - The inserted row will not be updated.
    /// - The table `t` will not be renamed, dropped, or altered.
    InsertSelect {
        /// The insert query
        insert: Insert,
        /// Selected row index
        row_index: usize,
        /// Additional interactions in the middle of the property
        queries: Vec<Query>,
        /// The select query
        select: Select,
    },
    /// Double Create Failure is a property in which creating
    /// the same table twice leads to an error.
    /// The execution of the property is as follows
    ///     CREATE TABLE <t> (...)
    ///     I_0
    ///     I_1
    ///     ...
    ///     I_n
    ///     CREATE TABLE <t> (...) -> Error
    /// The interactions in the middle has the following constraints;
    /// - There will be no errors in the middle interactions.
    /// - Table `t` will not be renamed or dropped.
    DoubleCreateFailure {
        /// The create query
        create: Create,
        /// Additional interactions in the middle of the property
        queries: Vec<Query>,
    },
}

impl Property {
    pub(crate) fn name(&self) -> String {
        match self {
            Property::InsertSelect { .. } => "Insert-Select".to_string(),
            Property::DoubleCreateFailure { .. } => "Double-Create-Failure".to_string(),
        }
    }
    /// interactions construct a list of interactions, which is an executable representation of the property.
    /// the requirement of property -> vec<interaction> conversion emerges from the need to serialize the property,
    /// and `interaction` cannot be serialized directly.
    pub(crate) fn interactions(&self) -> Vec<Interaction> {
        match self {
            Property::InsertSelect {
                insert,
                row_index,
                queries,
                select,
            } => {
                // Check that the insert query has at least 1 value
                assert!(
                    !insert.values.is_empty(),
                    "insert query should have at least 1 value"
                );

                // Pick a random row within the insert values
                let row = insert.values[*row_index].clone();

                // Assume that the table exists
                let assumption = Interaction::Assumption(Assertion {
                    message: format!("table {} exists", insert.table),
                    func: Box::new({
                        let table_name = insert.table.clone();
                        move |_: &Vec<ResultSet>, env: &SimulatorEnv| {
                            Ok(env.tables.iter().any(|t| t.name == table_name))
                        }
                    }),
                });

                let assertion = Interaction::Assertion(Assertion {
                    message: format!(
                        "row [{:?}] not found in table {}",
                        row.iter().map(|v| v.to_string()).collect::<Vec<String>>(),
                        insert.table,
                    ),
                    func: Box::new(move |stack: &Vec<ResultSet>, _: &SimulatorEnv| {
                        let rows = stack.last().unwrap();
                        match rows {
                            Ok(rows) => Ok(rows.iter().any(|r| r == &row)),
                            Err(err) => Err(LimboError::InternalError(err.to_string())),
                        }
                    }),
                });

                let mut interactions = Vec::new();
                interactions.push(assumption);
                interactions.push(Interaction::Query(Query::Insert(insert.clone())));
                interactions.extend(queries.clone().into_iter().map(Interaction::Query));
                interactions.push(Interaction::Query(Query::Select(select.clone())));
                interactions.push(assertion);

                interactions
            }
            Property::DoubleCreateFailure { create, queries } => {
                let table_name = create.table.name.clone();

                let assumption = Interaction::Assumption(Assertion {
                    message: "Double-Create-Failure should not be called on an existing table"
                        .to_string(),
                    func: Box::new(move |_: &Vec<ResultSet>, env: &SimulatorEnv| {
                        Ok(!env.tables.iter().any(|t| t.name == table_name))
                    }),
                });

                let cq1 = Interaction::Query(Query::Create(create.clone()));
                let cq2 = Interaction::Query(Query::Create(create.clone()));

                let table_name = create.table.name.clone();

                let assertion = Interaction::Assertion(Assertion {
                    message:
                        "creating two tables with the name should result in a failure for the second query"
                            .to_string(),
                    func: Box::new(move |stack: &Vec<ResultSet>, _: &SimulatorEnv| {
                        let last = stack.last().unwrap();
                        match last {
                            Ok(_) => Ok(false),
                            Err(e) => Ok(e.to_string().contains(&format!("Table {table_name} already exists"))),
                        }
                    }),
                });

                let mut interactions = Vec::new();
                interactions.push(assumption);
                interactions.push(cq1);
                interactions.extend(queries.clone().into_iter().map(Interaction::Query));
                interactions.push(cq2);
                interactions.push(assertion);

                interactions
            }
        }
    }
}

pub(crate) struct Remaining {
    pub(crate) read: f64,
    pub(crate) write: f64,
    pub(crate) create: f64,
}

pub(crate) fn remaining(env: &SimulatorEnv, stats: &InteractionStats) -> Remaining {
    let remaining_read = ((env.opts.max_interactions as f64 * env.opts.read_percent / 100.0)
        - (stats.read_count as f64))
        .max(0.0);
    let remaining_write = ((env.opts.max_interactions as f64 * env.opts.write_percent / 100.0)
        - (stats.write_count as f64))
        .max(0.0);
    let remaining_create = ((env.opts.max_interactions as f64 * env.opts.create_percent / 100.0)
        - (stats.create_count as f64))
        .max(0.0);

    Remaining {
        read: remaining_read,
        write: remaining_write,
        create: remaining_create,
    }
}

fn property_insert_select<R: rand::Rng>(
    rng: &mut R,
    env: &SimulatorEnv,
    remaining: &Remaining,
) -> Property {
    // Get a random table
    let table = pick(&env.tables, rng);
    // Generate rows to insert
    let rows = (0..rng.gen_range(1..=5))
        .map(|_| Vec::<Value>::arbitrary_from(rng, table))
        .collect::<Vec<_>>();

    // Pick a random row to select
    let row_index = pick_index(rows.len(), rng);
    let row = rows[row_index].clone();

    // Insert the rows
    let insert_query = Insert {
        table: table.name.clone(),
        values: rows,
    };

    // Create random queries respecting the constraints
    let mut queries = Vec::new();
    // - [x] There will be no errors in the middle interactions. (this constraint is impossible to check, so this is just best effort)
    // - [x] The inserted row will not be deleted.
    // - [ ] The inserted row will not be updated. (todo: add this constraint once UPDATE is implemented)
    // - [ ] The table `t` will not be renamed, dropped, or altered. (todo: add this constraint once ALTER or DROP is implemented)
    for _ in 0..rng.gen_range(0..3) {
        let query = Query::arbitrary_from(rng, (table, remaining));
        match &query {
            Query::Delete(Delete {
                table: t,
                predicate,
            }) => {
                // The inserted row will not be deleted.
                if t == &table.name && predicate.test(&row, table) {
                    continue;
                }
            }
            Query::Create(Create { table: t }) => {
                // There will be no errors in the middle interactions.
                // - Creating the same table is an error
                if t.name == table.name {
                    continue;
                }
            }
            _ => (),
        }
        queries.push(query);
    }

    // Select the row
    let select_query = Select {
        table: table.name.clone(),
        predicate: Predicate::arbitrary_from(rng, (table, &row)),
    };

    Property::InsertSelect {
        insert: insert_query,
        row_index,
        queries,
        select: select_query,
    }
}

fn property_double_create_failure<R: rand::Rng>(
    rng: &mut R,
    env: &SimulatorEnv,
    remaining: &Remaining,
) -> Property {
    // Get a random table
    let table = pick(&env.tables, rng);
    // Create the table
    let create_query = Create {
        table: table.clone(),
    };

    // Create random queries respecting the constraints
    let mut queries = Vec::new();
    // The interactions in the middle has the following constraints;
    // - [x] There will be no errors in the middle interactions.(best effort)
    // - [ ] Table `t` will not be renamed or dropped.(todo: add this constraint once ALTER or DROP is implemented)
    for _ in 0..rng.gen_range(0..3) {
        let query = Query::arbitrary_from(rng, (table, remaining));
        match &query {
            Query::Create(Create { table: t }) => {
                // There will be no errors in the middle interactions.
                // - Creating the same table is an error
                if t.name == table.name {
                    continue;
                }
            }
            _ => (),
        }
        queries.push(query);
    }

    Property::DoubleCreateFailure {
        create: create_query,
        queries,
    }
}

impl ArbitraryFrom<(&SimulatorEnv, &InteractionStats)> for Property {
    fn arbitrary_from<R: rand::Rng>(
        rng: &mut R,
        (env, stats): (&SimulatorEnv, &InteractionStats),
    ) -> Self {
        let remaining_ = remaining(env, stats);
        frequency(
            vec![
                (
                    f64::min(remaining_.read, remaining_.write),
                    Box::new(|rng: &mut R| property_insert_select(rng, env, &remaining_)),
                ),
                (
                    remaining_.create / 2.0,
                    Box::new(|rng: &mut R| property_double_create_failure(rng, env, &remaining_)),
                ),
            ],
            rng,
        )
    }
}
