use ledger_db::LedgerDb;
use testcontainers::{runners::AsyncRunner, ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;

/// A test harness: a freshly-spawned Postgres container with our migrations
/// applied, plus a LedgerDb pointing at it. The container is kept alive
/// for the lifetime of the harness; drop it to stop the container.
pub struct TestDb {
    // _container is held to keep the docker container alive. Dropping it
    // also drops the running Postgres.
    pub _container: ContainerAsync<Postgres>,
    pub db: LedgerDb,
}

impl TestDb {
    /// Start a fresh Postgres in a container, apply migrations, return a handle.
    /// Each test gets its own DB — no shared state, no cleanup, parallel-safe.
    pub async fn fresh() -> Self {
        let container = Postgres::default()
            .with_db_name("ledger")
            .with_user("ledger")
            .with_password("ledger")
            .with_tag("16-alpine")
            .start()
            .await
            .expect("failed to start postgres container");

        let host = container.get_host().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://ledger:ledger@{host}:{port}/ledger");

        let db = LedgerDb::connect(&url)
            .await
            .expect("failed to connect to test db");
        db.migrate().await.expect("failed to migrate test db");

        Self {
            _container: container,
            db,
        }
    }
}
