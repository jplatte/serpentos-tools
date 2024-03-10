// SPDX-FileCopyrightText: Copyright © 2020-2024 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

use chrono::{DateTime, Utc};
use diesel::prelude::*;
use diesel::{Connection as _, SqliteConnection};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

use super::{Connection, Error};
use crate::state::{self, Id, Selection};
use crate::State;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("src/db/state/migrations");

mod schema;

#[derive(Debug, Clone)]
pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn new(url: &str) -> Result<Self, Error> {
        let mut conn = SqliteConnection::establish(url)?;

        conn.run_pending_migrations(MIGRATIONS).map_err(Error::Migration)?;

        Ok(Database {
            conn: Connection::new(conn),
        })
    }

    pub fn list_ids(&self) -> Result<Vec<(Id, DateTime<Utc>)>, Error> {
        self.conn.exec(|conn| {
            model::state::table
                .select(model::Created::as_select())
                .load_iter(conn)?
                .map(|result| {
                    let row = result?;
                    Ok((row.id.into(), row.created.0))
                })
                .collect()
        })
    }

    pub fn get(&self, id: Id) -> Result<State, Error> {
        self.conn.exec(|conn| {
            let state = model::state::table
                .select(model::State::as_select())
                .find(i32::from(id))
                .first(conn)?;
            let selections = model::Selection::belonging_to(&state)
                .select(model::Selection::as_select())
                .load_iter(conn)?
                .map(|result| {
                    let row = result?;
                    Ok(state::Selection {
                        package: row.package_id,
                        explicit: row.explicit,
                        reason: row.reason,
                    })
                })
                .collect::<Result<_, Error>>()?;

            Ok(State {
                id: state.id.into(),
                summary: state.summary,
                description: state.description,
                selections,
                created: state.created.0,
                kind: state.kind,
            })
        })
    }

    pub fn add(
        &self,
        selections: &[Selection],
        summary: Option<&str>,
        description: Option<&str>,
    ) -> Result<State, Error> {
        self.conn
            .exec(|conn| {
                conn.transaction(|conn| {
                    let state = model::NewState {
                        summary,
                        description,
                        kind: state::Kind::Transaction.to_string(),
                    };

                    let id = diesel::insert_into(model::state::table)
                        .values(state)
                        .returning(model::state::id)
                        .get_result::<i32>(conn)?;

                    let selections = selections
                        .iter()
                        .map(|selection| model::NewSelection {
                            state_id: id,
                            package_id: selection.package.as_ref(),
                            explicit: selection.explicit,
                            reason: selection.reason.as_deref(),
                        })
                        .collect::<Vec<_>>();

                    diesel::insert_into(model::state_selections::table)
                        .values(selections)
                        .execute(conn)?;
                    Ok(id.into())
                })
            })
            .and_then(|id| self.get(id))
    }

    pub fn remove(&self, state: &state::Id) -> Result<(), Error> {
        self.batch_remove(Some(state))
    }

    pub fn batch_remove<'a>(&self, states: impl IntoIterator<Item = &'a state::Id>) -> Result<(), Error> {
        self.conn.exec(|conn| {
            let states = states.into_iter().map(|id| i32::from(*id)).collect::<Vec<_>>();

            conn.transaction(|conn| {
                // Cascading wipes other tables
                diesel::delete(model::state::table.filter(model::state::id.eq_any(&states))).execute(conn)?;
                Ok(())
            })
        })
    }
}

mod model {
    use diesel::{
        associations::{Associations, Identifiable},
        deserialize::Queryable,
        prelude::Insertable,
        sqlite::Sqlite,
        Selectable,
    };

    use crate::{db::Timestamp, package, state::Kind};

    pub use super::schema::{state, state_selections};

    #[derive(Queryable, Selectable, Identifiable)]
    #[diesel(table_name = state)]
    #[diesel(check_for_backend(Sqlite))]
    pub struct State {
        pub id: i32,
        #[diesel(deserialize_as = i64)]
        pub created: Timestamp,
        pub summary: Option<String>,
        pub description: Option<String>,
        #[diesel(column_name = "type_", deserialize_as = String)]
        pub kind: Kind,
    }

    #[derive(Queryable, Selectable, Identifiable, Associations)]
    #[diesel(table_name = state_selections)]
    #[diesel(primary_key(state_id, package_id))]
    #[diesel(belongs_to(State))]
    pub struct Selection {
        pub state_id: i32,
        #[diesel(deserialize_as = String)]
        pub package_id: package::Id,
        pub explicit: bool,
        pub reason: Option<String>,
    }

    #[derive(Queryable, Selectable, Identifiable)]
    #[diesel(table_name = state)]
    #[diesel(check_for_backend(Sqlite))]
    pub struct Created {
        pub id: i32,
        #[diesel(deserialize_as = i64)]
        pub created: Timestamp,
    }

    #[derive(Insertable)]
    #[diesel(table_name = state)]
    pub struct NewState<'a> {
        pub summary: Option<&'a str>,
        pub description: Option<&'a str>,
        #[diesel(column_name = "type_")]
        pub kind: String,
    }

    #[derive(Insertable)]
    #[diesel(table_name = state_selections)]
    pub struct NewSelection<'a> {
        pub state_id: i32,
        pub package_id: &'a str,
        pub explicit: bool,
        pub reason: Option<&'a str>,
    }
}

#[cfg(test)]
mod test {
    use chrono::Utc;

    use super::*;
    use crate::package;

    #[test]
    fn create_insert_select() {
        let database = Database::new(":memory:").unwrap();

        let selections = vec![
            Selection::explicit(package::Id::from("pkg a".to_string())),
            Selection::explicit(package::Id::from("pkg b".to_string())),
            Selection::explicit(package::Id::from("pkg c".to_string())),
        ];

        let state = database.add(&selections, Some("test"), Some("test")).unwrap();

        // First record
        assert_eq!(i32::from(state.id), 1);

        // Check created
        let elapsed = Utc::now().signed_duration_since(state.created);
        assert!(elapsed.num_seconds() == 0);
        assert!(!elapsed.is_zero());

        assert_eq!(state.summary.as_deref(), Some("test"));
        assert_eq!(state.description.as_deref(), Some("test"));

        assert_eq!(state.selections, selections);
    }
}
