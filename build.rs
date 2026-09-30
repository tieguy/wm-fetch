// Embed the resolved reqwest version at compile time so the User-Agent can
// carry `<library>/<version>` per the WMF User-Agent policy. Read from
// Cargo.lock so it stays truthful without manual syncing.
fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let lock = std::fs::read_to_string("Cargo.lock").unwrap_or_default();
    let ver = lock
        .split("[[package]]")
        .find(|s| s.trim_start().starts_with("name = \"reqwest\""))
        .and_then(|s| s.lines().find(|l| l.trim_start().starts_with("version = ")))
        .and_then(|l| l.split('"').nth(1))
        .unwrap_or("unknown")
        .to_string();
    println!("cargo:rustc-env=REQWEST_VERSION={ver}");
}
