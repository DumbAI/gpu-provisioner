//! The [`StateStore`] contract, run against every backend: SQLite always,
//! Postgres when `GPUPROV_TEST_DATABASE_URL` is set (a fresh random schema
//! per test, dropped afterwards); without it the Postgres cases print a note
//! and pass.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use super::migrate::{self, MigrateError};
use super::*;
use crate::machine::{Cancel, Executor, Machine, Step, StepCx};

/// A backend under test: the contract plus a way to age a heartbeat.
#[async_trait]
trait Backend: StateStore + Sized + 'static {
    async fn backdate(&self, key: &str, secs: i64);
}

#[async_trait]
impl Backend for SqliteStore {
    async fn backdate(&self, key: &str, secs: i64) {
        self.backdate_heartbeat(key, secs);
    }
}

#[async_trait]
impl Backend for PgStore {
    async fn backdate(&self, key: &str, secs: i64) {
        self.backdate_heartbeat(key, secs).await;
    }
}

const PG_URL_VAR: &str = "GPUPROV_TEST_DATABASE_URL";

/// A Postgres store in a schema of its own, dropped with the fixture.
struct PgFixture {
    store: PgStore,
    url: String,
}

impl PgFixture {
    async fn new() -> Option<Self> {
        let Ok(url) = std::env::var(PG_URL_VAR) else {
            eprintln!("note: {PG_URL_VAR} is not set; skipping the Postgres case");
            return None;
        };
        let store = PgStore::connect_with(&url, random_schema()).await.unwrap();
        Some(PgFixture { store, url })
    }
}

fn random_schema() -> PgOptions {
    use rand_core::RngCore;
    PgOptions {
        schema: format!("gpuprov_test_{:016x}", rand_core::OsRng.next_u64()),
        max_connections: 4,
    }
}

impl Drop for PgFixture {
    fn drop(&mut self) {
        // Drop runs inside the test's runtime: drop the schema from a thread
        // with a runtime of its own.
        let url = self.url.clone();
        let schema = self.store.schema().to_string();
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                    .await
                    .expect("connect to drop the test schema");
                tokio::spawn(conn);
                client
                    .batch_execute(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
                    .await
                    .expect("drop the test schema");
            });
        })
        .join();
    }
}

/// Instantiate each contract case once per backend.
macro_rules! contract {
    ($($case:ident),* $(,)?) => {
        mod sqlite_backend {
            $(
                #[tokio::test]
                async fn $case() {
                    super::$case(&super::SqliteStore::open_in_memory().unwrap()).await;
                }
            )*
        }
        mod postgres_backend {
            $(
                #[tokio::test]
                async fn $case() {
                    if let Some(fixture) = super::PgFixture::new().await {
                        super::$case(&fixture.store).await;
                    }
                }
            )*
        }
    };
}

contract!(
    projection_round_trip,
    journal_round_trip,
    lease_acquire_busy_stale_takeover_release,
    lease_is_atomic_under_contention,
    lost_lease_fences_save_and_heartbeat,
    executor_resumes_from_the_journal,
    executor_stops_when_its_lease_is_lost_at_a_write,
    executor_stops_when_its_lease_is_lost_at_a_heartbeat,
);

fn up_fields() -> UpFields {
    UpFields {
        region: "us-west-2".into(),
        zone: "us-west-2a".into(),
        instance_id: "i-0123".into(),
        public_ip: "1.2.3.4".into(),
        private_ip: Some("10.0.0.1".into()),
        ssh_user: "ubuntu".into(),
        ssh_port: 2222,
        ssh_key_path: "/keys/k".into(),
        ssh_config_path: "/ssh/vm".into(),
    }
}

fn entry<'a>(state_json: &'a str, label: &'a str, terminal: bool) -> JournalEntry<'a> {
    JournalEntry {
        kind: "vm",
        state_json,
        label,
        terminal,
        detail: None,
    }
}

// --- the projection -----------------------------------------------------------

async fn projection_round_trip<B: Backend>(db: &B) {
    assert!(db.get("vm1").await.unwrap().is_none());
    db.upsert_provisioning("vm1", "aws", "us-west-2", None, "g6e.8xlarge", "{\"a\":1}")
        .await
        .unwrap();
    let rec = db.get("vm1").await.unwrap().unwrap();
    assert_eq!(rec.state, VmState::Provisioning);
    assert_eq!(rec.provider.as_deref(), Some("aws"));
    assert_eq!(rec.region.as_deref(), Some("us-west-2"));
    assert_eq!(rec.instance_type.as_deref(), Some("g6e.8xlarge"));
    assert_eq!(rec.spec_json, "{\"a\":1}");
    assert!(rec.public_ip.is_none());
    assert!(rec.created_at > 1_700_000_000, "{}", rec.created_at);

    db.mark_up("vm1", &up_fields()).await.unwrap();
    let rec = db.get("vm1").await.unwrap().unwrap();
    assert_eq!(rec.state, VmState::Up);
    assert_eq!(rec.zone.as_deref(), Some("us-west-2a"));
    assert_eq!(rec.public_ip.as_deref(), Some("1.2.3.4"));
    assert_eq!(rec.private_ip.as_deref(), Some("10.0.0.1"));
    assert_eq!(rec.instance_id.as_deref(), Some("i-0123"));
    assert_eq!(rec.ssh_user.as_deref(), Some("ubuntu"));
    assert_eq!(rec.ssh_port, Some(2222));
    assert_eq!(rec.ssh_key_path.as_deref(), Some("/keys/k"));
    assert_eq!(rec.ssh_config_path.as_deref(), Some("/ssh/vm"));

    // Failover to a new region resets the instance fields.
    db.upsert_provisioning("vm1", "aws", "us-east-1", None, "g6e.8xlarge", "{}")
        .await
        .unwrap();
    let rec = db.get("vm1").await.unwrap().unwrap();
    assert_eq!(rec.state, VmState::Provisioning);
    assert_eq!(rec.region.as_deref(), Some("us-east-1"));
    assert!(rec.public_ip.is_none() && rec.instance_id.is_none());

    db.set_state("vm1", VmState::Terminated).await.unwrap();
    assert_eq!(
        db.get("vm1").await.unwrap().unwrap().state,
        VmState::Terminated
    );
    // Updates of a missing row are no-ops.
    db.set_state("ghost", VmState::Failed).await.unwrap();
    db.mark_up("ghost", &up_fields()).await.unwrap();
    assert!(db.get("ghost").await.unwrap().is_none());

    // LEAKED keeps the location (and creates a row when there is none).
    db.mark_leaked("vm2", "gcp", "us-central1", Some("us-central1-a"))
        .await
        .unwrap();
    let rec = db.get("vm2").await.unwrap().unwrap();
    assert_eq!(rec.state, VmState::Leaked);
    assert_eq!(rec.provider.as_deref(), Some("gcp"));
    assert_eq!(rec.zone.as_deref(), Some("us-central1-a"));

    let names: Vec<String> = db
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"vm1".to_string()) && names.contains(&"vm2".to_string()));
}

// --- the journal ----------------------------------------------------------------

async fn journal_round_trip<B: Backend>(db: &B) {
    let me = LeaseOwner::new();
    assert!(db.load_machine("vm1").await.unwrap().is_none());
    db.acquire_lease("vm1", "vm", &me).await.unwrap();
    // A lease alone is not a journaled state.
    assert!(db.load_machine("vm1").await.unwrap().is_none());
    db.save_machine("vm1", &me, entry(r#""A""#, "a", false))
        .await
        .unwrap();
    db.save_machine(
        "vm1",
        &me,
        JournalEntry {
            detail: Some("why"),
            ..entry(r#""B""#, "b", true)
        },
    )
    .await
    .unwrap();
    let row = db.load_machine("vm1").await.unwrap().unwrap();
    assert_eq!(
        row,
        MachineRow {
            kind: "vm".into(),
            state_json: r#""B""#.into(),
            label: "b".into(),
            terminal: true,
        }
    );
    let events = db.machine_events("vm1", 10).await.unwrap();
    let labels: Vec<&str> = events.iter().map(|e| e.label.as_str()).collect();
    assert_eq!(labels, ["a", "b"]);
    assert_eq!(events[1].detail.as_deref(), Some("why"));
    assert_eq!(events[1].kind, "vm");
    assert!(events[0].ts > 1_700_000_000);
    // `limit` keeps the newest, still oldest-first.
    assert_eq!(db.machine_events("vm1", 1).await.unwrap()[0].label, "b");
    assert!(db.machine_events("other", 10).await.unwrap().is_empty());
}

// --- the lease --------------------------------------------------------------------

async fn lease_acquire_busy_stale_takeover_release<B: Backend>(db: &B) {
    let (a, b, c) = (LeaseOwner::new(), LeaseOwner::new(), LeaseOwner::new());
    db.acquire_lease("vm1", "vm", &a).await.unwrap();
    db.acquire_lease("vm1", "vm", &a).await.unwrap(); // re-entrant for its owner
    match db.acquire_lease("vm1", "vm", &b).await {
        Err(StoreError::Busy { key, owner }) => {
            assert_eq!(key, "vm1");
            assert_eq!(owner, a.as_str());
        }
        other => panic!("expected Busy, got {other:?}"),
    }
    // A heartbeat just inside the window still holds it.
    db.backdate("vm1", LEASE_STALE_SECS - 5).await;
    assert!(matches!(
        db.acquire_lease("vm1", "vm", &b).await,
        Err(StoreError::Busy { .. })
    ));
    // A stale heartbeat means the owner is gone: taken over.
    db.backdate("vm1", LEASE_STALE_SECS + 1).await;
    db.acquire_lease("vm1", "vm", &b).await.unwrap();
    // The old owner's release does not free the new owner's lease.
    db.release_lease("vm1", &a).await.unwrap();
    assert!(matches!(
        db.acquire_lease("vm1", "vm", &c).await,
        Err(StoreError::Busy { .. })
    ));
    db.release_lease("vm1", &b).await.unwrap();
    db.acquire_lease("vm1", "vm", &c).await.unwrap();

    // A finished operation's lease never blocks the next one.
    db.save_machine("vm1", &c, entry(r#""Done""#, "done", true))
        .await
        .unwrap();
    db.acquire_lease("vm1", "vm", &a).await.unwrap();
}

async fn lease_is_atomic_under_contention<B: Backend>(db: &B) {
    for round in 0..5 {
        let key = format!("contended-{round}");
        let owners: Vec<LeaseOwner> = (0..4).map(|_| LeaseOwner::new()).collect();
        let (r0, r1, r2, r3) = tokio::join!(
            db.acquire_lease(&key, "vm", &owners[0]),
            db.acquire_lease(&key, "vm", &owners[1]),
            db.acquire_lease(&key, "vm", &owners[2]),
            db.acquire_lease(&key, "vm", &owners[3]),
        );
        let granted = [r0, r1, r2, r3]
            .into_iter()
            .filter(|r| match r {
                Ok(()) => true,
                Err(StoreError::Busy { .. }) => false,
                Err(e) => panic!("{e}"),
            })
            .count();
        assert_eq!(granted, 1, "round {round}");
    }
}

async fn lost_lease_fences_save_and_heartbeat<B: Backend>(db: &B) {
    let (stalled, successor) = (LeaseOwner::new(), LeaseOwner::new());
    db.acquire_lease("vm1", "vm", &stalled).await.unwrap();
    db.save_machine("vm1", &stalled, entry(r#""A""#, "a", false))
        .await
        .unwrap();
    db.heartbeat("vm1", &stalled).await.unwrap();
    db.backdate("vm1", LEASE_STALE_SECS + 1).await;
    db.acquire_lease("vm1", "vm", &successor).await.unwrap();

    // The stalled owner wakes up: every fenced write is refused, nothing lands.
    assert!(matches!(
        db.save_machine("vm1", &stalled, entry(r#""Stale""#, "stale", false))
            .await,
        Err(StoreError::LeaseLost { .. })
    ));
    assert!(matches!(
        db.heartbeat("vm1", &stalled).await,
        Err(StoreError::LeaseLost { .. })
    ));
    assert_eq!(
        db.load_machine("vm1").await.unwrap().unwrap().state_json,
        r#""A""#
    );
    let labels: Vec<String> = db
        .machine_events("vm1", 10)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.label)
        .collect();
    assert_eq!(labels, ["a"]);
    // The successor carries on.
    db.heartbeat("vm1", &successor).await.unwrap();
    db.save_machine("vm1", &successor, entry(r#""B""#, "b", false))
        .await
        .unwrap();
    // Without any lease row at all, writes are refused too.
    assert!(matches!(
        db.save_machine("nobody", &stalled, entry("1", "x", false))
            .await,
        Err(StoreError::LeaseLost { .. })
    ));
}

// --- the executor on each backend -------------------------------------------------

/// Counts 0 → 3. At `hijack_at` an intruder takes the lease over (as a
/// successor does once this run's heartbeat is stale) and the step then
/// either moves on (the executor's journal write must be refused) or polls
/// (its heartbeat must be refused).
struct Hijacked<'a, B: Backend> {
    store: &'a B,
    key: &'static str,
    intruder: LeaseOwner,
    hijack_at: Option<u32>,
    poll_after_hijack: bool,
    steps: AtomicU32,
}

impl<'a, B: Backend> Hijacked<'a, B> {
    fn new(store: &'a B, hijack_at: Option<u32>, poll_after_hijack: bool) -> Self {
        Hijacked {
            store,
            key: "vm1",
            intruder: LeaseOwner::new(),
            hijack_at,
            poll_after_hijack,
            steps: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl<B: Backend> Machine for Hijacked<'_, B> {
    type State = u32;
    const KIND: &'static str = "hijacked";

    fn is_terminal(&self, n: &u32) -> bool {
        *n >= 3
    }

    fn label(&self, n: &u32) -> String {
        format!("n={n}")
    }

    async fn step(&self, n: &u32, _cx: &StepCx) -> Step<u32> {
        self.steps.fetch_add(1, Ordering::SeqCst);
        if Some(*n) == self.hijack_at {
            self.store.backdate(self.key, LEASE_STALE_SECS + 1).await;
            self.store
                .acquire_lease(self.key, Self::KIND, &self.intruder)
                .await
                .expect("a stale lease is taken over");
            if self.poll_after_hijack {
                return Step::Poll {
                    after: Duration::from_millis(10),
                    note: "waiting".into(),
                };
            }
        }
        Step::Next(n + 1)
    }

    async fn on_retries_exhausted(&self, n: &u32, _last_error: String) -> u32 {
        *n
    }
}

async fn executor_resumes_from_the_journal<B: Backend>(db: &B) {
    seed_machine(db, "vm1", "hijacked", "2", "n=2", false).await;
    let machine = Hijacked::new(db, None, false);
    let end = Executor::new(&machine, db, "vm1", Cancel::new())
        .run(0)
        .await
        .unwrap();
    assert_eq!(end, 3);
    assert_eq!(machine.steps.load(Ordering::SeqCst), 1); // only n=2 was stepped
    let row = db.load_machine("vm1").await.unwrap().unwrap();
    assert!(row.terminal);
    // The lease was released: anyone may take the key now.
    db.acquire_lease("vm1", "vm", &LeaseOwner::new())
        .await
        .unwrap();
}

async fn executor_stops_when_its_lease_is_lost_at_a_write<B: Backend>(db: &B) {
    let machine = Hijacked::new(db, Some(1), false);
    let err = Executor::new(&machine, db, "vm1", Cancel::new())
        .run(0)
        .await
        .expect_err("the run is fenced off");
    assert!(err.is_lease_lost(), "{err}");
    assert!(err.to_string().contains("lost the lease"), "{err}");
    assert_fenced(db, &machine).await;
}

async fn executor_stops_when_its_lease_is_lost_at_a_heartbeat<B: Backend>(db: &B) {
    let machine = Hijacked::new(db, Some(1), true);
    let err = Executor::new(&machine, db, "vm1", Cancel::new())
        .run(0)
        .await
        .expect_err("the run is fenced off");
    assert!(err.is_lease_lost(), "{err}");
    assert_fenced(db, &machine).await;
}

/// The fenced run stepped n=0 and n=1, then stopped: the journal still says
/// n=1, and the intruder still holds the lease.
async fn assert_fenced<B: Backend>(db: &B, machine: &Hijacked<'_, B>) {
    assert_eq!(machine.steps.load(Ordering::SeqCst), 2);
    let row = db.load_machine("vm1").await.unwrap().unwrap();
    assert_eq!(row.state_json, "1");
    let labels: Vec<String> = db
        .machine_events("vm1", 10)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.label)
        .collect();
    assert_eq!(labels, ["n=0", "n=1"]);
    db.heartbeat("vm1", &machine.intruder).await.unwrap();
    assert!(matches!(
        db.acquire_lease("vm1", "vm", &LeaseOwner::new()).await,
        Err(StoreError::Busy { .. })
    ));
}

// --- `gpuprov state migrate` --------------------------------------------------------

#[tokio::test]
async fn migrate_copies_sqlite_into_postgres_once() {
    let Some(pg) = PgFixture::new().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteStore::open(&dir.path().join("state.db")).unwrap();

    // vm1: up, its launch finished. vm2: an interrupted launch (its process
    // crashed: a stale lease).
    sqlite
        .upsert_provisioning("vm1", "aws", "us-west-2", None, "g6e", "{}")
        .await
        .unwrap();
    sqlite.mark_up("vm1", &up_fields()).await.unwrap();
    sqlite
        .upsert_provisioning("vm2", "gcp", "us-central1", Some("a"), "g2", "{}")
        .await
        .unwrap();
    seed_machine(&sqlite, "vm1", "vm", r#""Ready""#, "UP", true).await;
    seed_machine(&sqlite, "vm2", "vm", r#""Booting""#, "booting", false).await;
    let crashed = LeaseOwner::new();
    sqlite.acquire_lease("vm2", "vm", &crashed).await.unwrap();
    sqlite.backdate_heartbeat("vm2", LEASE_STALE_SECS + 1);
    // A lease that never journaled a state is not copied.
    sqlite
        .acquire_lease("vm3", "vm", &LeaseOwner::new())
        .await
        .unwrap();
    sqlite.execute("UPDATE machine_events SET ts = 1700000000 WHERE key = 'vm1'");

    let report = migrate::sqlite_to_postgres(&sqlite, &pg.store)
        .await
        .unwrap();
    assert_eq!(report.counts.vms_copied, 2);
    assert_eq!(report.counts.machines_copied, 2);
    assert_eq!(report.counts.events_copied, 2);
    assert_eq!(report.in_flight, ["vm2"]);

    let db = &pg.store;
    assert_eq!(
        db.get("vm1").await.unwrap(),
        sqlite.get("vm1").await.unwrap()
    );
    assert_eq!(
        db.get("vm2").await.unwrap(),
        sqlite.get("vm2").await.unwrap()
    );
    assert_eq!(
        db.load_machine("vm2").await.unwrap(),
        sqlite.load_machine("vm2").await.unwrap()
    );
    assert!(db.load_machine("vm3").await.unwrap().is_none());
    assert_eq!(
        db.machine_events("vm1", 10).await.unwrap(),
        sqlite.machine_events("vm1", 10).await.unwrap()
    );
    assert_eq!(
        db.machine_events("vm1", 10).await.unwrap()[0].ts,
        1_700_000_000
    );
    // Leases are not copied: a worker resumes vm2 right away.
    db.acquire_lease("vm2", "vm", &LeaseOwner::new())
        .await
        .unwrap();

    // A rerun copies nothing twice and overwrites nothing.
    db.set_state("vm1", VmState::Terminated).await.unwrap();
    let again = migrate::sqlite_to_postgres(&sqlite, &pg.store)
        .await
        .unwrap();
    assert_eq!(
        again.counts,
        migrate::ImportCounts {
            vms_kept: 2,
            machines_kept: 2,
            ..Default::default()
        }
    );
    assert_eq!(
        db.get("vm1").await.unwrap().unwrap().state,
        VmState::Terminated
    );
    assert_eq!(db.machine_events("vm1", 10).await.unwrap().len(), 1);

    // An operation running right now (fresh lease) blocks the migration.
    seed_machine(&sqlite, "vm4", "vm", r#""Creating""#, "creating", false).await;
    sqlite
        .acquire_lease("vm4", "vm", &LeaseOwner::new())
        .await
        .unwrap();
    match migrate::sqlite_to_postgres(&sqlite, &pg.store).await {
        Err(MigrateError::Leased(keys)) => {
            assert_eq!(keys.len(), 1);
            assert!(keys[0].starts_with("'vm4'"), "{keys:?}");
        }
        other => panic!("expected Leased, got {other:?}"),
    }
    assert!(db.get("vm4").await.unwrap().is_none());
}

/// Workers starting together migrate once, into an existing or a new schema.
#[tokio::test]
async fn postgres_migrations_are_idempotent_and_serialized() {
    let Some(pg) = PgFixture::new().await else {
        return;
    };
    let existing = PgOptions {
        schema: pg.store.schema().to_string(),
        max_connections: 2,
    };
    let (a, b) = tokio::join!(
        PgStore::connect_with(&pg.url, existing.clone()),
        PgStore::connect_with(&pg.url, existing),
    );
    a.unwrap();
    b.unwrap();

    let fresh = random_schema();
    let (a, b) = tokio::join!(
        PgStore::connect_with(&pg.url, fresh.clone()),
        PgStore::connect_with(&pg.url, fresh),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    a.upsert_provisioning("vm1", "aws", "r", None, "t", "{}")
        .await
        .unwrap();
    assert!(b.get("vm1").await.unwrap().is_some());
    drop(b);
    // The fixture's Drop removes the extra schema.
    drop(PgFixture {
        store: a,
        url: pg.url.clone(),
    });
}
