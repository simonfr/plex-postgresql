use postgres::NoTls;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[test]
#[ignore]
fn stress_load_basic_roundtrip() {
    let host = env_or("PLEX_PG_HOST", "/tmp");
    let db = env_or("PLEX_PG_DATABASE", "plex");
    let user = env_or("PLEX_PG_USER", "plex");
    let schema = env_or("PLEX_PG_SCHEMA", "plex");
    let password = std::env::var("PLEX_PG_PASSWORD").ok();

    let mut cfg = postgres::Config::new();
    cfg.host(&host).dbname(&db).user(&user);
    if let Some(pw) = password {
        if !pw.is_empty() {
            cfg.password(pw);
        }
    }

    let mut client = cfg.connect(NoTls).expect("connect to postgres");
    let sp = format!("SET search_path TO {schema}");
    let _ = client.simple_query(&sp);

    for _ in 0..50 {
        let _ = client.simple_query("SELECT 1");
    }
}
