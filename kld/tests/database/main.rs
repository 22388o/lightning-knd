use std::panic;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::anyhow;
use anyhow::Result;
use futures::Future;
use futures::FutureExt;
use kld::database::DurableConnection;
use kld::logger::KldLogger;
use kld::settings::Settings;
use kld::Service;
use test_utils::cockroach_manager::create_database;
use test_utils::poll;
use test_utils::test_settings;
use test_utils::CockroachManager;
use test_utils::TempDir;
use tokio::runtime::Runtime;
use tokio::sync::OnceCell;

// mod ldk_database;
// mod wallet_database;


static COCKROACH_REF_COUNT: AtomicU16 = AtomicU16::new(0);

static CONNECTION_RUNTIME: OnceCell<Runtime> = OnceCell::const_new();

static TMP_DIR: OnceCell<TempDir> = OnceCell::const_new();

pub async fn with_cockroach<F, Fut>(test: F) -> Result<()>
where
    F: FnOnce(Arc<Settings>, Arc<DurableConnection>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let (settings, durable_connection, _cockroach) = cockroach().await?;
    let result = panic::AssertUnwindSafe(test(settings.clone(), durable_connection.clone()))
        .catch_unwind()
        .await;

    teardown().await;
    match result {
        Err(e) => panic::resume_unwind(e),
        Ok(v) => v,
    }
}

// Need to call teardown function at the end of the test if using this.
async fn cockroach() -> Result<&'static (
    Arc<Settings>,
    Arc<DurableConnection>,
    Mutex<CockroachManager<'static>>,
)> {
    COCKROACH_REF_COUNT.fetch_add(1, Ordering::AcqRel);
    static INSTANCE: OnceCell<(
        Arc<Settings>,
        Arc<DurableConnection>,
        Mutex<CockroachManager>,
    )> = OnceCell::const_new();
    INSTANCE
        .get_or_try_init(|| async move {
            KldLogger::init("test", log::LevelFilter::Debug);
            let tmp_dir = TMP_DIR
                .get_or_init(|| async { TempDir::new().expect("could not open temp dir for db") })
                .await;
            let mut settings = test_settings(tmp_dir, "integration");
            let cockroach = CockroachManager::new(tmp_dir, &mut settings).await?;
            create_database(&settings).await;
            let settings = Arc::new(settings);
            let settings_clone = settings.clone();
            let durable_connection = std::thread::spawn(|| async {
                CONNECTION_RUNTIME
                    .get_or_init(|| async { Runtime::new().unwrap() })
                    .await
                    .spawn(async { Arc::new(DurableConnection::new_migrate(settings_clone).await) })
                    .await
            })
            .join()
            .map_err(|_| anyhow!("connection failed"))?
            .await?;
            poll!(3, durable_connection.is_connected().await);
            Ok((settings, durable_connection, Mutex::new(cockroach)))
        })
        .await
}

pub async fn teardown() {
    if COCKROACH_REF_COUNT.fetch_sub(1, Ordering::AcqRel) == 1 {
        if let Ok((_, connection, cockroach)) = cockroach().await {
            connection.disconnect();
            let instance = cockroach.lock().unwrap();
            drop(instance);
        }
    }
}
