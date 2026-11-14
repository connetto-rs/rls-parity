use std::fs;

/// Embed the git revisions resolved for this binary.
fn main() -> anyhow::Result<()> {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let lock: toml::Value = toml::from_str(&fs::read_to_string("Cargo.lock")?)?;
    let packages = lock["package"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing packages"))?;
    let mut revisions = Vec::new();
    for package in packages {
        if let (Some(name), Some(source)) = (
            package["name"].as_str(),
            package.get("source").and_then(toml::Value::as_str),
        ) && source.starts_with("git+")
        {
            let (_, sha) = source
                .rsplit_once('#')
                .ok_or_else(|| anyhow::anyhow!("Missing git revision"))?;
            revisions.push(format!("{name}={sha}"));
        }
    }
    println!(
        "cargo:rustc-env=DEPENDENCY_REVISIONS={}",
        revisions.join(" ")
    );
    Ok(())
}
