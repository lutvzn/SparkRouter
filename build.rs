#[path = "build_support/models_dev.rs"]
mod models_dev;

use std::{env, fs, io::Read, path::PathBuf, time::Duration};

const URL: &str = "https://models.dev/api.json?type=all";
const LIMIT: u64 = 16 * 1024 * 1024;

fn download() -> Result<Vec<models_dev::Entry>, ()> {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| ())?;
    let response = client.get(URL).send().map_err(|_| ())?;
    if !response.status().is_success() {
        return Err(());
    }
    let mut bytes = Vec::new();
    response
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > LIMIT {
        return Err(());
    }
    models_dev::project(&bytes).map_err(|_| ())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_support/models_dev.rs");
    println!("cargo:rerun-if-changed=data/models-dev-categories.json");
    println!("cargo:rerun-if-env-changed=SPARKROUTER_MODELS_DEV_OFFLINE");
    println!("cargo:rerun-if-env-changed=SPARKROUTER_MODELS_DEV_REFRESH");
    let bundled: Vec<models_dev::Entry> = serde_json::from_slice(
        &fs::read("data/models-dev-categories.json").expect("bundled models.dev snapshot"),
    )
    .expect("valid bundled models.dev snapshot");
    let (entries, source) = if env::var_os("SPARKROUTER_MODELS_DEV_OFFLINE").is_some() {
        (bundled, "bundled snapshot (2026-10-10)")
    } else {
        match download() {
            Ok(entries) => (entries, "live download"),
            Err(()) => {
                println!(
                    "cargo:warning=models.dev unavailable or invalid; using bundled category snapshot"
                );
                (bundled, "bundled snapshot (2026-10-10); download failed")
            }
        }
    };
    let imported_at = if source == "live download" {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("build clock after epoch")
            .as_secs()
    } else {
        // The checked-in snapshot date, not the offline build clock.
        1_791_590_400
    };
    println!("cargo:rustc-env=MODELS_DEV_SOURCE={source}");
    println!("cargo:rustc-env=MODELS_DEV_IMPORTED_AT={imported_at}");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    fs::write(
        out.join("models_dev_categories.rs"),
        models_dev::rust_source(&entries),
    )
    .expect("write embedded models.dev categories");
}
