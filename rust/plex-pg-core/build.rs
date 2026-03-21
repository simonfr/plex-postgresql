fn main() {
    #[cfg(target_os = "macos")]
    {
        let libpq_dir = std::env::var("PLEX_PG_LIBPQ_DIR")
            .unwrap_or_else(|_| "/opt/homebrew/opt/postgresql@15/lib".to_string());
        println!("cargo:rustc-link-search=native={}", libpq_dir);
        println!("cargo:rustc-link-lib=c++");
        println!("cargo:rustc-link-lib=c++abi");
    }

    println!("cargo:rustc-link-lib=pq");
    println!("cargo:rustc-link-lib=sqlite3");
}
