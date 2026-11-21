//! The Postgres backend: gpuprov's state in its own schema (`gpuprov` by
//! default) of a database the caller may share with other schemas.
//!
//! - Every statement names its tables schema-qualified; gpuprov never sets
//!   `search_path` (that would not survive a transaction-mode pooler) and
//!   never touches anything outside its schema.
//! - Migrations are gpuprov's own, append-only (`MIGRATIONS`, recorded in
//!   `<schema>.migrations`), applied at connect and serialized across
//!   processes with a transaction-scoped advisory lock.
//! - Lease times are the server's `now()`.

use std::sync::Arc;

use async_trait::async_trait;
use deadpool_postgres::{GenericClient, Manager, ManagerConfig, Pool, RecyclingMethod};
use tokio_postgres::Row;

use super::migrate::{ImportCounts, Snapshot};
use super::{
    JournalEntry, LeaseOwner, MachineEvent, MachineRow, StateStore, StoreError, UpFields, VmRecord,
    VmState, LEASE_STALE_SECS,
};

/// Append-only: never edit a shipped entry; add the next version.
const MIGRATIONS: &[(i32, &str)] = &[(1, include_str!("migrations/0001_state.sql"))];

/// Connection options beyond the URL.
#[derive(Debug, Clone)]
pub struct PgOptions {
    /// The schema holding gpuprov's tables (created if absent). Lowercase
    /// ASCII letters, digits and `_`, not starting with a digit.
    pub schema: String,
    /// Pool size.
    pub max_connections: usize,
}

impl Default for PgOptions {
    fn default() -> Self {
        PgOptions {
            schema: "gpuprov".into(),
            max_connections: 8,
        }
    }
}

/// The Postgres store: a connection pool, cheap to share (`Arc`).
pub struct PgStore {
    pool: Pool,
    /// Quoted schema identifier, spliced into statements for `{s}`.
    quoted_schema: String,
    schema: String,
}

impl PgStore {
    /// Connect to `url` (`postgres://…`; `sslmode` as in libpq, default
    /// `prefer`; `sslrootcert=<pem file>` as in libpq, trusted besides the
    /// system's roots — managed Postgres such as DigitalOcean's is signed by
    /// the provider's own CA), using the `gpuprov` schema, and apply pending
    /// migrations.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        Self::connect_with(url, PgOptions::default()).await
    }

    /// As [`PgStore::connect`], with explicit options.
    pub async fn connect_with(url: &str, options: PgOptions) -> Result<Self, StoreError> {
        if !valid_schema(&options.schema) {
            return Err(StoreError::Invalid(format!(
                "invalid schema name '{}' (lowercase letters, digits and _)",
                options.schema
            )));
        }
        let (url, root) = split_root_cert(url);
        let root_pem = match root {
            Some(path) => Some(std::fs::read(&path).map_err(|e| {
                StoreError::Invalid(format!("database url sslrootcert '{path}': {e}"))
            })?),
            None => None,
        };
        let config: tokio_postgres::Config = url
            .parse()
            .map_err(|e| StoreError::Invalid(format!("invalid database url: {e}")))?;
        let manager = Manager::from_config(
            config,
            tls(root_pem.as_deref()).map_err(StoreError::Invalid)?,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let pool = Pool::builder(manager)
            .max_size(options.max_connections.max(1))
            .build()
            .map_err(|e| StoreError::Invalid(e.to_string()))?;
        let store = PgStore {
            pool,
            quoted_schema: format!("\"{}\"", options.schema),
            schema: options.schema,
        };
        store.migrate().await?;
        Ok(store)
    }

    /// The schema holding gpuprov's tables.
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// `template` with `{s}` replaced by the quoted schema.
    fn sql(&self, template: &str) -> String {
        template.replace("{s}", &self.quoted_schema)
    }

    async fn migrate(&self) -> Result<(), StoreError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        // Serializes concurrent connects for this schema (`CREATE SCHEMA IF
        // NOT EXISTS` itself races).
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext($1))",
            &[&format!("gpuprov.migrate:{}", self.schema)],
        )
        .await?;
        tx.batch_execute("SET LOCAL client_min_messages = warning")
            .await?;
        // Created only when missing: `CREATE SCHEMA IF NOT EXISTS` needs the
        // CREATE privilege even when the schema exists.
        let exists = tx
            .query_opt(
                "SELECT 1 FROM pg_namespace WHERE nspname = $1",
                &[&self.schema],
            )
            .await?
            .is_some();
        if !exists {
            tx.batch_execute(&self.sql("CREATE SCHEMA {s}")).await?;
        }
        tx.batch_execute(&self.sql(
            "CREATE TABLE IF NOT EXISTS {s}.migrations (
                 version int PRIMARY KEY, applied_at timestamptz NOT NULL DEFAULT now())",
        ))
        .await?;
        for (version, sql) in MIGRATIONS {
            let done = tx
                .query_opt(
                    &self.sql("SELECT 1 FROM {s}.migrations WHERE version = $1"),
                    &[version],
                )
                .await?
                .is_some();
            if !done {
                tx.batch_execute(&self.sql(sql)).await?;
                tx.execute(
                    &self.sql("INSERT INTO {s}.migrations (version) VALUES ($1)"),
                    &[version],
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Copy `snapshot` in, in one transaction, never overwriting: a VM or
    /// machine already here is kept as is, and a key's events are copied only
    /// when this store has none for it (so a rerun copies nothing twice).
    /// Leases are not copied.
    pub(crate) async fn import(&self, snapshot: &Snapshot) -> Result<ImportCounts, StoreError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let mut counts = ImportCounts::default();
        let insert_vm = tx
            .prepare(&self.sql(
                "INSERT INTO {s}.vms (name, state, region, zone, instance_id, public_ip,
                     private_ip, ssh_user, ssh_key_path, ssh_config_path, spec_json,
                     created_at, updated_at, instance_type, provider, ssh_port)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                         to_timestamp($12), to_timestamp($13), $14, $15, $16)
                 ON CONFLICT (name) DO NOTHING",
            ))
            .await?;
        for vm in &snapshot.vms {
            let port = vm.ssh_port.map(i32::from);
            let inserted = tx
                .execute(
                    &insert_vm,
                    &[
                        &vm.name,
                        &vm.state.as_str(),
                        &vm.region,
                        &vm.zone,
                        &vm.instance_id,
                        &vm.public_ip,
                        &vm.private_ip,
                        &vm.ssh_user,
                        &vm.ssh_key_path,
                        &vm.ssh_config_path,
                        &vm.spec_json,
                        &(vm.created_at as f64),
                        &(vm.updated_at as f64),
                        &vm.instance_type,
                        &vm.provider,
                        &port,
                    ],
                )
                .await?;
            if inserted == 1 {
                counts.vms_copied += 1;
            } else {
                counts.vms_kept += 1;
            }
        }
        let insert_machine = tx
            .prepare(&self.sql(
                "INSERT INTO {s}.machines (key, kind, state_json, label, terminal, updated_at)
                 VALUES ($1, $2, $3, $4, $5, to_timestamp($6))
                 ON CONFLICT (key) DO NOTHING",
            ))
            .await?;
        for m in &snapshot.machines {
            let inserted = tx
                .execute(
                    &insert_machine,
                    &[
                        &m.key,
                        &m.row.kind,
                        &m.row.state_json,
                        &m.row.label,
                        &m.row.terminal,
                        &(m.updated_at as f64),
                    ],
                )
                .await?;
            if inserted == 1 {
                counts.machines_copied += 1;
            } else {
                counts.machines_kept += 1;
            }
        }
        let keys: Vec<&str> = {
            let mut keys: Vec<&str> = snapshot.events.iter().map(|e| e.key.as_str()).collect();
            keys.sort_unstable();
            keys.dedup();
            keys
        };
        let has_events: std::collections::HashSet<String> = tx
            .query(
                &self.sql("SELECT DISTINCT key FROM {s}.machine_events WHERE key = ANY($1)"),
                &[&keys],
            )
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect();
        let insert_event = tx
            .prepare(&self.sql(
                "INSERT INTO {s}.machine_events (key, kind, ts, label, detail)
                 VALUES ($1, $2, to_timestamp($3), $4, $5)",
            ))
            .await?;
        for e in &snapshot.events {
            if has_events.contains(&e.key) {
                continue;
            }
            tx.execute(
                &insert_event,
                &[
                    &e.key,
                    &e.event.kind,
                    &(e.event.ts as f64),
                    &e.event.label,
                    &e.event.detail,
                ],
            )
            .await?;
            counts.events_copied += 1;
        }
        tx.commit().await?;
        Ok(counts)
    }

    /// Test support: age `key`'s heartbeat by `secs`.
    #[cfg(test)]
    pub(crate) async fn backdate_heartbeat(&self, key: &str, secs: i64) {
        let client = self.pool.get().await.unwrap();
        client
            .execute(
                &self.sql(
                    "UPDATE {s}.machines SET heartbeat = now() - make_interval(secs => $2)
                     WHERE key = $1",
                ),
                &[&key, &(secs as f64)],
            )
            .await
            .unwrap();
    }
}

fn valid_schema(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name.len() <= 63
}

/// `url` without libpq's `sslrootcert` parameter (tokio-postgres does not
/// read it), and its value.
fn split_root_cert(url: &str) -> (String, Option<String>) {
    let Some((base, query)) = url.split_once('?') else {
        return (url.to_string(), None);
    };
    let mut root = None;
    let rest: Vec<&str> = query
        .split('&')
        .filter(|kv| match kv.strip_prefix("sslrootcert=") {
            Some(v) => {
                root = Some(v.to_string());
                false
            }
            None => !kv.is_empty(),
        })
        .collect();
    let url = if rest.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", rest.join("&"))
    };
    (url, root)
}

/// TLS as libpq's `sslmode` asks (`prefer` by default: TLS when the server
/// offers it), verified against the system's roots and `root_pem`'s
/// certificates.
fn tls(root_pem: Option<&[u8]>) -> Result<tokio_postgres_rustls::MakeRustlsConnect, String> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if let Some(pem) = root_pem {
        let certs: Vec<_> = rustls::pki_types::pem::PemObject::pem_slice_iter(pem)
            .collect::<Result<_, _>>()
            .map_err(|e| format!("sslrootcert is not PEM: {e}"))?;
        if certs.is_empty() {
            return Err("sslrootcert holds no certificate".into());
        }
        for cert in certs {
            roots.add(cert).map_err(|e| format!("sslrootcert: {e}"))?;
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
}

const RECORD_COLUMNS: &str = "name, state, region, zone, instance_id, public_ip, private_ip,
    ssh_user, ssh_key_path, ssh_config_path, spec_json,
    floor(extract(epoch FROM created_at))::bigint,
    floor(extract(epoch FROM updated_at))::bigint,
    instance_type, provider, ssh_port";

fn record_from_row(row: &Row) -> Result<VmRecord, StoreError> {
    let state: String = row.get(1);
    let port: Option<i32> = row.get(15);
    Ok(VmRecord {
        name: row.get(0),
        state: VmState::parse(&state)
            .ok_or_else(|| StoreError::Invalid(format!("unknown vm state '{state}'")))?,
        region: row.get(2),
        zone: row.get(3),
        instance_id: row.get(4),
        public_ip: row.get(5),
        private_ip: row.get(6),
        ssh_user: row.get(7),
        ssh_key_path: row.get(8),
        ssh_config_path: row.get(9),
        spec_json: row.get(10),
        created_at: row.get(11),
        updated_at: row.get(12),
        instance_type: row.get(13),
        provider: row.get(14),
        ssh_port: port
            .map(u16::try_from)
            .transpose()
            .map_err(|_| StoreError::Invalid(format!("bad ssh_port {port:?}")))?,
    })
}

#[async_trait]
impl StateStore for PgStore {
    async fn upsert_provisioning(
        &self,
        name: &str,
        provider: &str,
        region: &str,
        zone: Option<&str>,
        instance_type: &str,
        spec_json: &str,
    ) -> Result<(), StoreError> {
        let client = self.pool.get().await?;
        let stmt = client
            .prepare_cached(&self.sql(
                "INSERT INTO {s}.vms (name, state, provider, region, zone, instance_type,
                                      spec_json, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, now(), now())
                 ON CONFLICT (name) DO UPDATE SET
                   state = EXCLUDED.state,
                   provider = EXCLUDED.provider,
                   region = EXCLUDED.region,
                   zone = EXCLUDED.zone,
                   instance_type = EXCLUDED.instance_type,
                   instance_id = NULL, public_ip = NULL, private_ip = NULL,
                   spec_json = EXCLUDED.spec_json,
                   updated_at = now()",
            ))
            .await?;
        client
            .execute(
                &stmt,
                &[
                    &name,
                    &VmState::Provisioning.as_str(),
                    &provider,
                    &region,
                    &zone,
                    &instance_type,
                    &spec_json,
                ],
            )
            .await?;
        Ok(())
    }

    async fn mark_leaked(
        &self,
        name: &str,
        provider: &str,
        region: &str,
        zone: Option<&str>,
    ) -> Result<(), StoreError> {
        let client = self.pool.get().await?;
        let stmt = client
            .prepare_cached(&self.sql(
                "INSERT INTO {s}.vms (name, state, provider, region, zone, spec_json,
                                      created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, '{}', now(), now())
                 ON CONFLICT (name) DO UPDATE SET
                   state = EXCLUDED.state, provider = EXCLUDED.provider,
                   region = EXCLUDED.region, zone = EXCLUDED.zone,
                   updated_at = now()",
            ))
            .await?;
        client
            .execute(
                &stmt,
                &[&name, &VmState::Leaked.as_str(), &provider, &region, &zone],
            )
            .await?;
        Ok(())
    }

    async fn mark_up(&self, name: &str, up: &UpFields) -> Result<(), StoreError> {
        let client = self.pool.get().await?;
        let stmt = client
            .prepare_cached(&self.sql(
                "UPDATE {s}.vms SET
                   state = $2, region = $3, zone = $4, instance_id = $5,
                   public_ip = $6, private_ip = $7,
                   ssh_user = $8, ssh_key_path = $9, ssh_config_path = $10,
                   ssh_port = $11, updated_at = now()
                 WHERE name = $1",
            ))
            .await?;
        client
            .execute(
                &stmt,
                &[
                    &name,
                    &VmState::Up.as_str(),
                    &up.region,
                    &up.zone,
                    &up.instance_id,
                    &up.public_ip,
                    &up.private_ip,
                    &up.ssh_user,
                    &up.ssh_key_path,
                    &up.ssh_config_path,
                    &i32::from(up.ssh_port),
                ],
            )
            .await?;
        Ok(())
    }

    async fn set_state(&self, name: &str, state: VmState) -> Result<(), StoreError> {
        let client = self.pool.get().await?;
        let stmt = client
            .prepare_cached(
                &self.sql("UPDATE {s}.vms SET state = $2, updated_at = now() WHERE name = $1"),
            )
            .await?;
        client.execute(&stmt, &[&name, &state.as_str()]).await?;
        Ok(())
    }

    async fn get(&self, name: &str) -> Result<Option<VmRecord>, StoreError> {
        let client = self.pool.get().await?;
        let stmt = client
            .prepare_cached(&self.sql(&format!(
                "SELECT {RECORD_COLUMNS} FROM {{s}}.vms WHERE name = $1"
            )))
            .await?;
        client
            .query_opt(&stmt, &[&name])
            .await?
            .as_ref()
            .map(record_from_row)
            .transpose()
    }

    async fn list(&self) -> Result<Vec<VmRecord>, StoreError> {
        let client = self.pool.get().await?;
        let stmt = client
            .prepare_cached(&self.sql(&format!(
                "SELECT {RECORD_COLUMNS} FROM {{s}}.vms ORDER BY created_at, name"
            )))
            .await?;
        client
            .query(&stmt, &[])
            .await?
            .iter()
            .map(record_from_row)
            .collect()
    }

    async fn acquire_lease(
        &self,
        key: &str,
        kind: &str,
        owner: &LeaseOwner,
    ) -> Result<(), StoreError> {
        let client = self.pool.get().await?;
        // One statement: on a conflicting row Postgres locks it and
        // re-checks the WHERE against its latest version, so two contenders
        // can never both be granted.
        let stmt = client
            .prepare_cached(&self.sql(
                "INSERT INTO {s}.machines AS m
                     (key, kind, state_json, label, terminal, owner, heartbeat, updated_at)
                 VALUES ($1, $2, 'null', 'leased', false, $3, now(), now())
                 ON CONFLICT (key) DO UPDATE SET owner = EXCLUDED.owner, heartbeat = now()
                 WHERE m.owner IS NULL
                    OR m.owner = EXCLUDED.owner
                    OR m.heartbeat IS NULL
                    OR m.heartbeat <= now() - make_interval(secs => $4)
                    OR m.terminal
                 RETURNING owner",
            ))
            .await?;
        let granted = client
            .query_opt(
                &stmt,
                &[&key, &kind, &owner.as_str(), &(LEASE_STALE_SECS as f64)],
            )
            .await?;
        if granted.is_some() {
            return Ok(());
        }
        let holder: Option<String> = client
            .query_opt(
                &self.sql("SELECT owner FROM {s}.machines WHERE key = $1"),
                &[&key],
            )
            .await?
            .and_then(|r| r.get(0));
        Err(StoreError::Busy {
            key: key.to_string(),
            owner: holder.unwrap_or_else(|| "unknown".into()),
        })
    }

    async fn release_lease(&self, key: &str, owner: &LeaseOwner) -> Result<(), StoreError> {
        let client = self.pool.get().await?;
        let stmt = client
            .prepare_cached(
                &self.sql("UPDATE {s}.machines SET owner = NULL WHERE key = $1 AND owner = $2"),
            )
            .await?;
        client.execute(&stmt, &[&key, &owner.as_str()]).await?;
        Ok(())
    }

    async fn heartbeat(&self, key: &str, owner: &LeaseOwner) -> Result<(), StoreError> {
        let client = self.pool.get().await?;
        let stmt = client
            .prepare_cached(
                &self
                    .sql("UPDATE {s}.machines SET heartbeat = now() WHERE key = $1 AND owner = $2"),
            )
            .await?;
        if client.execute(&stmt, &[&key, &owner.as_str()]).await? == 0 {
            return Err(StoreError::LeaseLost {
                key: key.to_string(),
            });
        }
        Ok(())
    }

    async fn save_machine(
        &self,
        key: &str,
        owner: &LeaseOwner,
        entry: JournalEntry<'_>,
    ) -> Result<(), StoreError> {
        let mut client = self.pool.get().await?;
        let update = client
            .prepare_cached(&self.sql(
                "UPDATE {s}.machines SET kind = $3, state_json = $4, label = $5, terminal = $6,
                                         heartbeat = now(), updated_at = now()
                 WHERE key = $1 AND owner = $2",
            ))
            .await?;
        let insert = client
            .prepare_cached(&self.sql(
                "INSERT INTO {s}.machine_events (key, kind, ts, label, detail)
                 VALUES ($1, $2, now(), $3, $4)",
            ))
            .await?;
        // The UPDATE locks the row until commit: a contender's acquire waits,
        // then sees a fresh heartbeat.
        let tx = client.transaction().await?;
        let updated = tx
            .execute(
                &update,
                &[
                    &key,
                    &owner.as_str(),
                    &entry.kind,
                    &entry.state_json,
                    &entry.label,
                    &entry.terminal,
                ],
            )
            .await?;
        if updated == 0 {
            tx.rollback().await?;
            return Err(StoreError::LeaseLost {
                key: key.to_string(),
            });
        }
        tx.execute(&insert, &[&key, &entry.kind, &entry.label, &entry.detail])
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn load_machine(&self, key: &str) -> Result<Option<MachineRow>, StoreError> {
        let client = self.pool.get().await?;
        let stmt = client
            .prepare_cached(&self.sql(
                "SELECT kind, state_json, label, terminal FROM {s}.machines
                 WHERE key = $1 AND state_json <> 'null'",
            ))
            .await?;
        Ok(client.query_opt(&stmt, &[&key]).await?.map(|r| MachineRow {
            kind: r.get(0),
            state_json: r.get(1),
            label: r.get(2),
            terminal: r.get(3),
        }))
    }

    async fn machine_events(
        &self,
        key: &str,
        limit: usize,
    ) -> Result<Vec<MachineEvent>, StoreError> {
        let client = self.pool.get().await?;
        let stmt = client
            .prepare_cached(&self.sql(
                "SELECT floor(extract(epoch FROM ts))::bigint, kind, label, detail FROM
                   (SELECT seq, ts, kind, label, detail FROM {s}.machine_events
                    WHERE key = $1 ORDER BY seq DESC LIMIT $2) recent
                 ORDER BY seq",
            ))
            .await?;
        Ok(client
            .query(&stmt, &[&key, &(limit as i64)])
            .await?
            .iter()
            .map(|r| MachineEvent {
                ts: r.get(0),
                kind: r.get(1),
                label: r.get(2),
                detail: r.get(3),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sslrootcert_is_taken_out_of_the_url() {
        assert_eq!(
            split_root_cert("postgres://u:p@h:25060/db?sslmode=require&sslrootcert=/etc/ca.pem"),
            (
                "postgres://u:p@h:25060/db?sslmode=require".to_string(),
                Some("/etc/ca.pem".to_string())
            )
        );
        assert_eq!(
            split_root_cert("postgres://h/db?sslrootcert=/ca.pem&application_name=w"),
            (
                "postgres://h/db?application_name=w".to_string(),
                Some("/ca.pem".to_string())
            )
        );
        assert_eq!(
            split_root_cert("postgres://h/db?sslrootcert=/ca.pem"),
            ("postgres://h/db".to_string(), Some("/ca.pem".to_string()))
        );
        assert_eq!(
            split_root_cert("postgres://h/db?sslmode=disable"),
            ("postgres://h/db?sslmode=disable".to_string(), None)
        );
        assert_eq!(
            split_root_cert("postgres://h/db"),
            ("postgres://h/db".to_string(), None)
        );
    }

    #[tokio::test]
    async fn a_missing_sslrootcert_is_a_clear_error() {
        let err = PgStore::connect("postgres://h/db?sslrootcert=/nonexistent/ca.pem")
            .await
            .err()
            .expect("refused");
        let msg = err.to_string();
        assert!(msg.contains("sslrootcert '/nonexistent/ca.pem'"), "{msg}");
    }

    #[test]
    fn a_non_certificate_sslrootcert_is_refused() {
        assert!(tls(Some(b"not a pem")).is_err());
        assert!(tls(None).is_ok());
    }

    #[test]
    fn schema_names_are_plain_identifiers() {
        assert!(valid_schema("gpuprov"));
        assert!(valid_schema("gpuprov_test_0af3"));
        assert!(!valid_schema("GpuProv"));
        assert!(!valid_schema("1abc"));
        assert!(!valid_schema("a\"; DROP"));
        assert!(!valid_schema(""));
    }
}
