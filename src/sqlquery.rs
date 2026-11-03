//! Audited dynamic SQL for sqlx 0.9 [`SqlSafeStr`](sqlx::SqlSafeStr).
//!
//! Dynamic fragments here are table/column names from internal allowlists; user
//! input is always passed as bind parameters or via [`sqlx::QueryBuilder`].

use sqlx::AssertSqlSafe;

#[inline]
pub fn query<'q, DB>(sql: &'q str) -> sqlx::query::Query<'q, DB, <DB as sqlx::Database>::Arguments>
where
    DB: sqlx::Database,
{
    sqlx::query(AssertSqlSafe(sql))
}

#[inline]
pub fn query_string<DB>(
    sql: String,
) -> sqlx::query::Query<'static, DB, <DB as sqlx::Database>::Arguments>
where
    DB: sqlx::Database,
{
    sqlx::query(AssertSqlSafe(sql))
}
