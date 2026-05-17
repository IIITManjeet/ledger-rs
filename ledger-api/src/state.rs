use ledger_db::LedgerDb;

/// Shared state available to every route handler. Holds the LedgerDb
/// (which internally Arc-shares the connection pool). Cheap to Clone.
#[derive(Clone)]
pub struct AppState {
    pub db: LedgerDb,
}

impl AppState {
    pub fn new(db: LedgerDb) -> Self {
        Self { db }
    }
}
