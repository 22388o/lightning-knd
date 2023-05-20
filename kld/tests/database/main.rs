use std::panic;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use futures::Future;
use futures::FutureExt;
use kld::database::DurableConnection;
use kld::logger::KldLogger;
use kld::Service;
use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;
use settings::Settings;
use test_utils::cockroach_manager::create_database;
use test_utils::poll;
use test_utils::test_settings;
use test_utils::{cockroach, CockroachManager};
use tokio::runtime::Handle;
use tokio::runtime::Runtime;

mod ldk_database;
mod wallet_database;

static COCKROACH_REF_COUNT: AtomicU16 = AtomicU16::new(0);

static CONNECTION_RUNTIME: Lazy<Runtime> = Lazy::new(|| Runtime::new().unwrap());

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
    Mutex<CockroachManager>,
)> {
    COCKROACH_REF_COUNT.fetch_add(1, Ordering::AcqRel);
    static INSTANCE: OnceCell<(
        Arc<Settings>,
        Arc<DurableConnection>,
        Mutex<CockroachManager>,
    )> = OnceCell::new();
    INSTANCE.get_or_try_init(|| {
        KldLogger::init("test", log::LevelFilter::Debug);
        tokio::task::block_in_place(move || {
            Handle::current().block_on(async move {
                let mut settings = test_settings!("integration");
                let cockroach = cockroach!(settings);
                create_database(&settings).await;
                let settings = Arc::new(settings);
                let settings_clone = settings.clone();
                std::thread::spawn(|| CONNECTION_RUNTIME.enter());
                let durable_connection = CONNECTION_RUNTIME
                    .spawn(async { Arc::new(DurableConnection::new_migrate(settings_clone).await) })
                    .await?;
                poll!(3, durable_connection.is_connected().await);
                Ok((settings, durable_connection, Mutex::new(cockroach)))
            })
        })
    })
}

pub async fn teardown() {
    let count = COCKROACH_REF_COUNT.fetch_sub(1, Ordering::AcqRel);
    println!("COUNT {count}");
    if count == 1 {
        if let Ok((_, connection, cockroach)) = cockroach().await {
            connection.disconnect();
            let mut lock = cockroach.lock().unwrap();
            lock.kill();
        }
    }
}