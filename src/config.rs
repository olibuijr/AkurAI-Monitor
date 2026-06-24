use std::sync::LazyLock;

pub struct Config {
    pub listen_addr: String,
    pub db_path: String,
    pub static_dir: String,
    pub alert_log: String,
    pub log_files: Vec<String>,
    pub interval_secs: u64,
    pub retention_days: i64,
    pub log_retention_days: i64,
    pub oidc_issuer: String,
    pub oidc_client_id: String,
    pub oidc_client_secret: String,
    pub oidc_redirect_uri: String,
    pub base_url: String,
}

static CONFIG: LazyLock<Config> = LazyLock::new(|| {
    let log_files_str =
        std::env::var("MONITOR_LOG_FILES").unwrap_or_else(|_| "/var/log/syslog,/var/log/auth.log".to_string());
    let log_files: Vec<String> = log_files_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let base_url = std::env::var("MONITOR_BASE_URL")
        .unwrap_or_else(|_| "https://monitor.olibuijr.com".to_string())
        .trim_end_matches('/')
        .to_string();

    let redirect_uri = std::env::var("MONITOR_OIDC_REDIRECT_URI")
        .unwrap_or_else(|_| format!("{base_url}/auth/callback"));

    Config {
        listen_addr: std::env::var("MONITOR_LISTEN").unwrap_or_else(|_| "127.0.0.1:8800".to_string()),
        db_path: std::env::var("MONITOR_DB_PATH").unwrap_or_else(|_| "./data/monitor.db".to_string()),
        static_dir: std::env::var("MONITOR_STATIC_DIR").unwrap_or_else(|_| "./ui".to_string()),
        alert_log: std::env::var("MONITOR_ALERT_LOG").unwrap_or_else(|_| "./data/alerts.log".to_string()),
        log_files,
        interval_secs: std::env::var("MONITOR_INTERVAL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60),
        retention_days: std::env::var("MONITOR_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30),
        log_retention_days: std::env::var("MONITOR_LOG_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(7),
        oidc_issuer: std::env::var("MONITOR_OIDC_ISSUER")
            .unwrap_or_else(|_| "https://auth.olibuijr.com".to_string()),
        oidc_client_id: std::env::var("MONITOR_OIDC_CLIENT_ID").unwrap_or_default(),
        oidc_client_secret: std::env::var("MONITOR_OIDC_CLIENT_SECRET").unwrap_or_default(),
        oidc_redirect_uri: redirect_uri,
        base_url,
    }
});

pub fn get() -> &'static Config {
    &CONFIG
}
