use std::borrow::Cow;
use crate::Opts;

/// Provides metadata on the underlying connection
pub trait ConnectionInfo {
    /// Returns connection identifier.
    fn id(&self) -> u32;

    /// Returns the ID generated by a query (usually `INSERT`) on a table with a column having the
    /// `AUTO_INCREMENT` attribute. Returns `None` if there was no previous query on the connection
    /// or if the query did not update an AUTO_INCREMENT value.
    fn last_insert_id(&self) -> Option<u64>;

    /// Returns the number of rows affected by the last `INSERT`, `UPDATE`, `REPLACE` or `DELETE`
    /// query.
    fn affected_rows(&self) -> u64;

    /// Text information, as reported by the server in the last OK packet, or an empty string.
    fn info(&self) -> Cow<'_, str>;

    /// Number of warnings, as reported by the server in the last OK packet, or `0`.
    fn get_warnings(&self) -> u16;

    /// Returns server version.
    fn server_version(&self) -> (u16, u16, u16);

    /// Returns connection options.
    fn opts(&self) -> &Opts;
}