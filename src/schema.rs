use rusqlite::Connection;

pub fn create_tables(conn: &Connection) {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS hosts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            address TEXT NOT NULL,
            kind TEXT NOT NULL DEFAULT 'linux',
            enabled INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS applications (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            host_id INTEGER NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            service_name TEXT,
            health_url TEXT,
            enabled INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            UNIQUE(host_id, name)
        );

        CREATE TABLE IF NOT EXISTS metrics (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            host_id INTEGER REFERENCES hosts(id) ON DELETE SET NULL,
            application_id INTEGER REFERENCES applications(id) ON DELETE SET NULL,
            name TEXT NOT NULL,
            value REAL NOT NULL,
            ts INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_metrics_name_ts ON metrics(name, ts);

        CREATE TABLE IF NOT EXISTS logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            host_id INTEGER REFERENCES hosts(id) ON DELETE SET NULL,
            application_id INTEGER REFERENCES applications(id) ON DELETE SET NULL,
            source TEXT NOT NULL,
            line TEXT NOT NULL,
            ts INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_logs_source_ts ON logs(source, ts);

        CREATE TABLE IF NOT EXISTS alert_rules (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            host_id INTEGER REFERENCES hosts(id) ON DELETE CASCADE,
            application_id INTEGER REFERENCES applications(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            metric_name TEXT NOT NULL,
            operator TEXT NOT NULL,
            threshold REAL NOT NULL,
            duration_secs INTEGER NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1
        );

        CREATE TABLE IF NOT EXISTS alert_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            rule_id INTEGER NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
            triggered_at INTEGER NOT NULL,
            resolved_at INTEGER,
            value REAL NOT NULL
        );

        CREATE TABLE IF NOT EXISTS channels (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            kind TEXT NOT NULL,
            endpoint TEXT NOT NULL,
            token_env TEXT,
            enabled INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS recipients (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            target TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            UNIQUE(channel_id, target)
        );
        ",
    )
    .expect("failed to create tables");

    seed_local_host(conn);
    migrate_legacy_tables(conn);
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_metrics_host_ts ON metrics(host_id, ts);
         CREATE INDEX IF NOT EXISTS idx_logs_host_ts ON logs(host_id, ts);",
    )
    .expect("failed to create host indexes");
    create_application_host_triggers(conn);
}

fn create_application_host_triggers(conn: &Connection) {
    for table in ["metrics", "logs", "alert_rules"] {
        conn.execute_batch(&format!(
            "
            CREATE TRIGGER IF NOT EXISTS {table}_application_host_insert
            BEFORE INSERT ON {table}
            WHEN NEW.application_id IS NOT NULL
              AND (
                NEW.host_id IS NULL
                OR NOT EXISTS (
                    SELECT 1 FROM applications
                    WHERE id = NEW.application_id AND host_id = NEW.host_id
                )
              )
            BEGIN
                SELECT RAISE(ABORT, 'application does not belong to host');
            END;

            CREATE TRIGGER IF NOT EXISTS {table}_application_host_update
            BEFORE UPDATE OF host_id, application_id ON {table}
            WHEN NEW.application_id IS NOT NULL
              AND (
                NEW.host_id IS NULL
                OR NOT EXISTS (
                    SELECT 1 FROM applications
                    WHERE id = NEW.application_id AND host_id = NEW.host_id
                )
              )
            BEGIN
                SELECT RAISE(ABORT, 'application does not belong to host');
            END;
            "
        ))
        .expect("failed to create application ownership triggers");
    }
    conn.execute_batch(
        "
        CREATE TRIGGER IF NOT EXISTS applications_host_move_guard
        BEFORE UPDATE OF host_id ON applications
        WHEN NEW.host_id != OLD.host_id
          AND (
            EXISTS (SELECT 1 FROM metrics WHERE application_id = OLD.id)
            OR EXISTS (SELECT 1 FROM logs WHERE application_id = OLD.id)
            OR EXISTS (SELECT 1 FROM alert_rules WHERE application_id = OLD.id)
          )
        BEGIN
            SELECT RAISE(ABORT, 'application host cannot change while attributed data exists');
        END;
        ",
    )
    .expect("failed to create application move guard");
}

fn migrate_legacy_tables(conn: &Connection) {
    add_column(
        conn,
        "metrics",
        "host_id",
        "INTEGER REFERENCES hosts(id) ON DELETE SET NULL",
    );
    add_column(
        conn,
        "metrics",
        "application_id",
        "INTEGER REFERENCES applications(id) ON DELETE SET NULL",
    );
    add_column(
        conn,
        "logs",
        "host_id",
        "INTEGER REFERENCES hosts(id) ON DELETE SET NULL",
    );
    add_column(
        conn,
        "logs",
        "application_id",
        "INTEGER REFERENCES applications(id) ON DELETE SET NULL",
    );
    add_column(
        conn,
        "alert_rules",
        "host_id",
        "INTEGER REFERENCES hosts(id) ON DELETE CASCADE",
    );
    add_column(
        conn,
        "alert_rules",
        "application_id",
        "INTEGER REFERENCES applications(id) ON DELETE CASCADE",
    );
    let local_host_id: i64 = conn
        .query_row("SELECT id FROM hosts WHERE name = 'local'", [], |row| {
            row.get(0)
        })
        .expect("local host missing during migration");
    for table in ["metrics", "logs", "alert_rules"] {
        conn.execute(
            &format!("UPDATE {table} SET host_id = ?1 WHERE host_id IS NULL"),
            [local_host_id],
        )
        .expect("failed to backfill local host attribution");
    }
    migrate_alert_events_fk(conn);

    conn.execute(
        "UPDATE alert_rules SET metric_name = 'disk.root.used_pct' WHERE metric_name = 'disk./.used_pct'",
        [],
    )
    .expect("failed to migrate disk alert rule");
}

fn migrate_alert_events_fk(conn: &Connection) {
    let mut statement = conn
        .prepare("PRAGMA foreign_key_list(alert_events)")
        .expect("failed to inspect alert event foreign keys");
    let has_cascade = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(6)?,
            ))
        })
        .expect("failed to inspect alert event foreign keys")
        .filter_map(Result::ok)
        .any(|(table, column, on_delete)| {
            table == "alert_rules" && column == "rule_id" && on_delete == "CASCADE"
        });
    drop(statement);
    if has_cascade {
        return;
    }

    if let Err(error) = conn.execute_batch(
        "
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        DROP TABLE IF EXISTS alert_events_migrated;
        CREATE TABLE alert_events_migrated (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            rule_id INTEGER NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
            triggered_at INTEGER NOT NULL,
            resolved_at INTEGER,
            value REAL NOT NULL
        );
        INSERT INTO alert_events_migrated (id, rule_id, triggered_at, resolved_at, value)
        SELECT id, rule_id, triggered_at, resolved_at, value FROM alert_events;
        DROP TABLE alert_events;
        ALTER TABLE alert_events_migrated RENAME TO alert_events;
        COMMIT;
        PRAGMA foreign_keys = ON;
        ",
    ) {
        conn.execute_batch("ROLLBACK; PRAGMA foreign_keys = ON;")
            .ok();
        panic!("failed to migrate alert event foreign key: {error}");
    }
}

fn add_column(conn: &Connection, table: &str, column: &str, definition: &str) {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("failed to inspect table");
    let exists = statement
        .query_map([], |row| row.get::<_, String>(1))
        .expect("failed to inspect columns")
        .filter_map(Result::ok)
        .any(|name| name == column);
    drop(statement);

    if !exists {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))
        .expect("failed to add schema column");
    }
}

fn seed_local_host(conn: &Connection) {
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT OR IGNORE INTO hosts (name, address, kind, enabled, created_at, updated_at)
         VALUES ('local', '127.0.0.1', 'linux', 1, ?1, ?1)",
        [now],
    )
    .expect("failed to seed local host");
}

pub fn seed_alert_rules(conn: &Connection) {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM alert_rules", [], |row| row.get(0))
        .unwrap_or(0);
    if count > 0 {
        return;
    }

    let host_id: i64 = conn
        .query_row("SELECT id FROM hosts WHERE name = 'local'", [], |row| {
            row.get(0)
        })
        .expect("local host missing");
    let rules = [
        ("High CPU", "cpu.usage", "gt", 90.0, 300),
        ("High Memory", "mem.used_pct", "gt", 90.0, 300),
        ("Disk Almost Full", "disk.root.used_pct", "gt", 85.0, 60),
    ];

    for (name, metric, op, threshold, duration) in &rules {
        conn.execute(
            "INSERT INTO alert_rules
             (host_id, name, metric_name, operator, threshold, duration_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![host_id, name, metric, op, threshold, duration],
        )
        .expect("failed to seed alert rule");
    }

    tracing::info!("seeded {} default alert rules", rules.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_legacy_tables_without_losing_alert_rules() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(
            "
            CREATE TABLE metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                value REAL NOT NULL,
                ts INTEGER NOT NULL
            );
            CREATE TABLE logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source TEXT NOT NULL,
                line TEXT NOT NULL,
                ts INTEGER NOT NULL
            );
            CREATE TABLE alert_rules (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                metric_name TEXT NOT NULL,
                operator TEXT NOT NULL,
                threshold REAL NOT NULL,
                duration_secs INTEGER NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE alert_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                rule_id INTEGER NOT NULL REFERENCES alert_rules(id),
                triggered_at INTEGER NOT NULL,
                resolved_at INTEGER,
                value REAL NOT NULL
            );
            INSERT INTO alert_rules
                (name, metric_name, operator, threshold, duration_secs)
            VALUES
                ('Disk Almost Full', 'disk./.used_pct', 'gt', 85, 60);
            INSERT INTO metrics (name, value, ts) VALUES ('cpu.usage', 50, 1);
            INSERT INTO logs (source, line, ts) VALUES ('syslog', 'ready', 1);
            INSERT INTO alert_events (rule_id, triggered_at, value) VALUES (1, 1, 90);
            ",
        )
        .unwrap();

        create_tables(&conn);

        let metric_columns = table_columns(&conn, "metrics");
        let log_columns = table_columns(&conn, "logs");
        let alert_columns = table_columns(&conn, "alert_rules");
        assert!(metric_columns.contains(&"host_id".to_string()));
        assert!(metric_columns.contains(&"application_id".to_string()));
        assert!(log_columns.contains(&"host_id".to_string()));
        assert!(log_columns.contains(&"application_id".to_string()));
        assert!(alert_columns.contains(&"host_id".to_string()));
        assert!(alert_columns.contains(&"application_id".to_string()));

        let disk_metric: String = conn
            .query_row(
                "SELECT metric_name FROM alert_rules WHERE name = 'Disk Almost Full'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(disk_metric, "disk.root.used_pct");

        let local_hosts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM hosts WHERE name = 'local'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(local_hosts, 1);

        for table in ["metrics", "logs", "alert_rules"] {
            let attributed: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table}
                         WHERE host_id = (SELECT id FROM hosts WHERE name = 'local')"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(attributed, 1, "{table} was not attributed to local host");
        }

        conn.execute("DELETE FROM alert_rules WHERE id = 1", [])
            .unwrap();
        let events: i64 = conn
            .query_row("SELECT COUNT(*) FROM alert_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(events, 0);
    }

    #[test]
    fn rejects_application_attribution_to_the_wrong_host() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        create_tables(&conn);
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO hosts (name, address, kind, enabled, created_at, updated_at)
             VALUES ('remote', '100.88.0.8', 'linux', 1, ?1, ?1)",
            [now],
        )
        .unwrap();
        let remote_host = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO applications
             (host_id, name, enabled, created_at, updated_at)
             VALUES (?1, 'remote-app', 1, ?2, ?2)",
            rusqlite::params![remote_host, now],
        )
        .unwrap();
        let application_id = conn.last_insert_rowid();
        let local_host: i64 = conn
            .query_row("SELECT id FROM hosts WHERE name = 'local'", [], |row| {
                row.get(0)
            })
            .unwrap();

        let mismatch = conn.execute(
            "INSERT INTO metrics (host_id, application_id, name, value, ts)
             VALUES (?1, ?2, 'health.ready', 1, ?3)",
            rusqlite::params![local_host, application_id, now],
        );
        assert!(mismatch.is_err());
        conn.execute(
            "INSERT INTO metrics (host_id, application_id, name, value, ts)
             VALUES (?1, ?2, 'health.ready', 1, ?3)",
            rusqlite::params![remote_host, application_id, now],
        )
        .unwrap();

        let move_application = conn.execute(
            "UPDATE applications SET host_id = ?1 WHERE id = ?2",
            rusqlite::params![local_host, application_id],
        );
        assert!(move_application.is_err());

        let ownership_triggers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'trigger' AND name LIKE '%_application_host_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ownership_triggers, 6);
    }

    fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut statement = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        statement
            .query_map([], |row| row.get(1))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
}
