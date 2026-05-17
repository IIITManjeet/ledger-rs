use ledger_api::{router, AppState};
use ledger_db::LedgerDb;
use std::net::SocketAddr;
use testcontainers::{runners::AsyncRunner, ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;

/// A running ledger-api instance backed by an ephemeral Postgres container.
/// Drop to stop both.
///
/// Each test binary (happy_path, idempotency, ...) compiles this module
/// independently; fields used by some binaries but not others would trip
/// the dead-code lint. The `#[allow(dead_code)]` covers that.
#[allow(dead_code)]
pub struct TestApp {
    pub addr: SocketAddr,
    pub db: LedgerDb,
    pub client: reqwest::Client,
    // Container is held to keep the docker container alive.
    pub _container: ContainerAsync<Postgres>,
}

impl TestApp {
    /// Spin up a fresh DB + a fresh ledger-api server on 127.0.0.1:<random>.
    /// Returns immediately once the server is listening.
    pub async fn spawn() -> Self {
        let container = Postgres::default()
            .with_db_name("ledger")
            .with_user("ledger")
            .with_password("ledger")
            .with_tag("16-alpine")
            .start()
            .await
            .expect("postgres container did not start");

        let host = container.get_host().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://ledger:ledger@{host}:{port}/ledger");

        let db = LedgerDb::connect(&url).await.expect("connect");
        db.migrate().await.expect("migrate");

        let state = AppState::new(db.clone());
        let app = router(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server stopped");
        });

        Self {
            addr,
            db,
            client: reqwest::Client::new(),
            _container: container,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}
