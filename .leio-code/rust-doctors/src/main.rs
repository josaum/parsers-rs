//! Policy owned by sovereign parsers-rs. LEIO only transports these results.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs, path::Path};
use yaml_rust2::{Yaml, YamlLoader};
const NAMES: &[&str] = &[
    "parsers-clippy-gate",
    "parsers-arrow-ipc",
    "parsers-doc-coverage",
    "parsers-binding-contract",
];
const PY: &[&str] = &["docx", "xlsx", "pptx", "pdf", "email", "json", "xsd"];
const NODE: &[&str] = &["docx", "xlsx", "pptx", "email"];
const PY_BINDINGS: &[&str] = &[
    "docx", "xlsx", "pptx", "pdf", "email", "json", "xsd", "ontology",
];
const NODE_BINDINGS: &[&str] = &["docx", "xlsx", "pptx", "email", "json"];
#[derive(Deserialize)]
struct Request {
    protocol: u32,
    request_id: String,
    root: std::path::PathBuf,
    names: Vec<String>,
}
fn text(root: &Path, path: &str) -> Result<String> {
    fs::read_to_string(root.join(path)).with_context(|| path.to_owned())
}
/// The first document of a YAML file, as a generic value. Indexing a key that
/// is absent yields a value every `as_*` accessor answers `None` for.
fn yaml_file(root: &Path, path: &str) -> Result<Yaml> {
    YamlLoader::load_from_str(&text(root, path)?)?
        .into_iter()
        .next()
        .with_context(|| format!("{path} holds no YAML document"))
}
fn toml_file(root: &Path, path: &str) -> Result<toml::Value> {
    Ok(toml::from_str(&text(root, path)?)?)
}
fn check(root: &Path, name: &str) -> Value {
    let mut warnings = Vec::new();
    let mut evidence = Vec::new();
    let mut require = |ok: bool, path: &str, message: &str| {
        if !ok {
            warnings.push(message.to_owned());
        }
        evidence.push(json!({"kind":name,"path":path,"line":null,"detail":message,"passed":ok}));
    };
    let result = (|| -> Result<()> {
        match name {
            "parsers-clippy-gate" => {
                let path = ".github/workflows/ci.yml";
                let ci = yaml_file(root, path)?;
                let commands: Vec<_> = ci["jobs"]["rust"]["steps"]
                    .as_vec()
                    .context("CI rust steps missing")?
                    .iter()
                    .filter_map(|s| s["run"].as_str())
                    .collect();
                let flags = ci["env"]["RUSTFLAGS"].as_str().unwrap_or("");
                for tool in ["clippy", "test"] {
                    for arrow in [false, true] {
                        require(
                            commands.iter().any(|c| {
                                c.contains(&format!("cargo {tool} --workspace"))
                                    && c.contains("--locked")
                                    && (tool != "clippy" || c.contains("--all-targets"))
                                    && (c.contains("--features") == arrow)
                                    && (!arrow || c.contains("env.ARROW_FEATURES"))
                            }),
                            path,
                            &format!(
                                "CI must run locked {tool} for {} features",
                                if arrow { "Arrow" } else { "default" }
                            ),
                        );
                    }
                }
                require(
                    flags
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .windows(2)
                        .any(|w| w == ["-D", "warnings"]),
                    path,
                    "CI must deny Rust warnings",
                );
                require(
                    commands
                        .iter()
                        .any(|c| c.contains("cargo fmt --all --check")),
                    path,
                    "CI must check formatting",
                );
                let features = ci["env"]["ARROW_FEATURES"]
                    .as_str()
                    .context("ARROW_FEATURES missing")?;
                let workspace = toml_file(root, "Cargo.toml")?;
                for member in workspace["workspace"]["members"]
                    .as_array()
                    .context("workspace members missing")?
                    .iter()
                    .filter_map(|v| v.as_str())
                {
                    let manifest = toml_file(root, &format!("{member}/Cargo.toml"))?;
                    if manifest
                        .get("features")
                        .and_then(|f| f.get("arrow"))
                        .is_some()
                    {
                        let package = manifest["package"]["name"]
                            .as_str()
                            .context("package name missing")?;
                        require(
                            features.split(',').any(|f| f == format!("{package}/arrow")),
                            path,
                            &format!("Arrow CI must cover {package}"),
                        );
                    }
                }
            }
            "parsers-arrow-ipc" => {
                let workspace = toml_file(root, "Cargo.toml")?;
                let deps = &workspace["workspace"]["dependencies"];
                let version = |v: &toml::Value| {
                    v.as_str()
                        .or_else(|| v.get("version").and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .to_owned()
                };
                let pin = version(&deps["arrow-array"]);
                require(
                    pin.starts_with('='),
                    "Cargo.toml",
                    "Arrow wire format must use an exact shared version",
                );
                for dep in ["arrow", "arrow-ipc", "arrow-schema"] {
                    require(
                        version(&deps[dep]) == pin,
                        "Cargo.toml",
                        &format!("{dep} must match Arrow array pin"),
                    );
                }
                for parser in PY {
                    let path = format!("{parser}-fast-core/Cargo.toml");
                    let manifest = toml_file(root, &path)?;
                    require(
                        manifest
                            .get("features")
                            .and_then(|f| f.get("arrow"))
                            .is_some(),
                        &path,
                        "parser core must expose its Arrow feature",
                    );
                    let components: &[&str] = if manifest
                        .get("dependencies")
                        .and_then(|d| d.get("arrow"))
                        .is_some()
                    {
                        &["arrow"]
                    } else {
                        &["arrow-array", "arrow-schema"]
                    };
                    for dep in components {
                        require(
                            manifest
                                .get("dependencies")
                                .and_then(|d| d.get(dep))
                                .and_then(|d| d.get("workspace"))
                                .and_then(|v| v.as_bool())
                                == Some(true),
                            &path,
                            &format!("{dep} must inherit the sovereign workspace pin"),
                        );
                    }
                }
                for parser in NODE {
                    let path = format!("{parser}-fast-node/Cargo.toml");
                    let manifest = toml_file(root, &path)?;
                    let dep = format!("{parser}-fast-core");
                    require(
                        manifest["dependencies"][&dep]["features"]
                            .as_array()
                            .is_some_and(|a| a.iter().any(|x| x.as_str() == Some("arrow"))),
                        &path,
                        "Node binding must enable core Arrow support",
                    );
                }
            }
            "parsers-doc-coverage" => {
                // Existing ratchet from the migrated doctor; json had no prior deny-doc pin.
                for parser in ["docx", "xlsx", "pptx", "pdf", "email", "xsd"] {
                    let path = format!("{parser}-fast-core/src/lib.rs");
                    let body = text(root, &path)?;
                    require(
                        body.contains("#![deny(missing_docs)]"),
                        &path,
                        "documented parser core must retain deny(missing_docs)",
                    );
                }
            }
            "parsers-binding-contract" => {
                for parser in PY_BINDINGS {
                    let core = toml_file(root, &format!("{parser}-fast-core/Cargo.toml"))?;
                    let path = format!("{parser}-fast-py/Cargo.toml");
                    let binding = toml_file(root, &path)?;
                    require(
                        binding["package"]["version"] == core["package"]["version"],
                        &path,
                        "published Python binding version must match its core",
                    );
                }
                for parser in NODE_BINDINGS {
                    let core = toml_file(root, &format!("{parser}-fast-core/Cargo.toml"))?;
                    let path = format!("{parser}-fast-node/Cargo.toml");
                    let binding = toml_file(root, &path)?;
                    require(
                        binding["package"]["version"] == core["package"]["version"],
                        &path,
                        "published Node binding version must match its core",
                    );
                    let path = format!("{parser}-fast-node/package.json");
                    let package: Value = serde_json::from_str(&text(root, &path)?)?;
                    require(
                        package["version"].as_str() == core["package"]["version"].as_str(),
                        &path,
                        "Node tarball version must match its core",
                    );
                    require(
                        package["repository"]["url"].as_str()
                            == Some("https://github.com/josaum/parsers-rs.git"),
                        &path,
                        "Node package provenance must identify sovereign parsers-rs",
                    );
                }
            }
            _ => anyhow::bail!("unknown parser doctor {name}"),
        };
        Ok(())
    })();
    if let Err(error) = result {
        warnings.push(error.to_string());
    }
    json!({"schema_version":"1.0","query_id":format!("{name}-{}",std::process::id()),"kind":"doctor","summary":format!("{name}: {} warnings",warnings.len()),"confidence":if warnings.is_empty(){0.98}else{0.6},"entities":[],"evidence":evidence.into_iter().map(|mut e|{e.as_object_mut().unwrap().remove("passed");e}).collect::<Vec<_>>(),"warnings":warnings,"meta":{"owner":"parsers-rs","source_observations_only":true},"timing_ms":0})
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(args.len() == 3, "use --request PATH or --check-root ROOT");
    if args[1] == "--check-root" {
        let results: Vec<_> = NAMES
            .iter()
            .map(|n| check(Path::new(&args[2]), n))
            .collect();
        let failed = results
            .iter()
            .any(|r| !r["warnings"].as_array().unwrap().is_empty());
        println!("{}", serde_json::to_string_pretty(&results)?);
        if failed {
            std::process::exit(1);
        }
        return Ok(());
    }
    ensure!(args[1] == "--request", "unsupported mode");
    let request: Request = serde_json::from_reader(fs::File::open(&args[2])?)?;
    ensure!(
        request.protocol == 1 && request.root.is_absolute(),
        "unsupported request"
    );
    let mut results = serde_json::Map::new();
    for name in request.names {
        ensure!(NAMES.contains(&name.as_str()), "unknown doctor");
        results.insert(name.clone(), check(&request.root, &name));
    }
    println!(
        "{}",
        json!({"protocol":1,"request_id":request.request_id,"results":results})
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absent_repository_never_passes() {
        let root = tempfile::tempdir().unwrap();
        for name in NAMES {
            assert!(
                !check(root.path(), name)["warnings"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
    }
    #[test]
    fn docs_ratchet_detects_removed_pin() {
        let root = tempfile::tempdir().unwrap();
        for parser in ["docx", "xlsx", "pptx", "pdf", "email", "xsd"] {
            let dir = root.path().join(format!("{parser}-fast-core/src"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("lib.rs"), "#![deny(missing_docs)]\n").unwrap();
        }
        assert!(
            check(root.path(), "parsers-doc-coverage")["warnings"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        fs::write(
            root.path().join("pdf-fast-core/src/lib.rs"),
            "#![warn(missing_docs)]",
        )
        .unwrap();
        assert_eq!(
            check(root.path(), "parsers-doc-coverage")["warnings"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}

#[cfg(test)]
mod regression {
    use super::*;
    #[test]
    fn repository_ci_covers_every_declared_arrow_feature() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let workflow = yaml_file(&root, ".github/workflows/ci.yml").unwrap();
        assert!(
            workflow["on"]
                .as_hash()
                .unwrap()
                .contains_key(&Yaml::String("workflow_call".to_owned()))
        );
        let result = check(&root, "parsers-clippy-gate");
        assert_eq!(result["warnings"], json!([]), "{result}");
        assert!(
            result["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| { entry["detail"] == "Arrow CI must cover web-fast-core" })
        );
    }
    #[test]
    fn ci_contract_detects_missing_feature_and_warning_gate() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".github/workflows")).unwrap();
        fs::create_dir_all(root.join("sample-core")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"sample-core\"]\n",
        )
        .unwrap();
        fs::write(
            root.join("sample-core/Cargo.toml"),
            "[package]\nname = \"sample-core\"\n[features]\narrow = []\n",
        )
        .unwrap();
        let yaml = "env:\n  RUSTFLAGS: -D warnings\n  ARROW_FEATURES: sample-core/arrow\njobs:\n  rust:\n    steps:\n      - run: cargo fmt --all --check\n      - run: cargo clippy --workspace --all-targets --locked\n      - run: cargo clippy --workspace --all-targets --locked --features ${{ env.ARROW_FEATURES }}\n      - run: cargo test --workspace --locked\n      - run: cargo test --workspace --locked --features ${{ env.ARROW_FEATURES }}\n";
        fs::write(root.join(".github/workflows/ci.yml"), yaml).unwrap();
        assert!(
            check(root, "parsers-clippy-gate")["warnings"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        fs::write(
            root.join(".github/workflows/ci.yml"),
            yaml.replace("sample-core/arrow", "missing/arrow")
                .replace("-D warnings", "-A warnings"),
        )
        .unwrap();
        assert_eq!(
            check(root, "parsers-clippy-gate")["warnings"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn binding_contract_does_not_invent_missing_node_products() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for parser in PY_BINDINGS {
            for suffix in ["core", "py"] {
                let dir = root.join(format!("{parser}-fast-{suffix}"));
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join("Cargo.toml"), "[package]\nversion = \"1.2.3\"\n").unwrap();
            }
        }
        for parser in NODE_BINDINGS {
            let dir = root.join(format!("{parser}-fast-node"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("Cargo.toml"), "[package]\nversion = \"1.2.3\"\n").unwrap();
            fs::write(dir.join("package.json"),json!({"version":"1.2.3","repository":{"url":"https://github.com/josaum/parsers-rs.git"}}).to_string()).unwrap();
        }
        assert!(
            check(root, "parsers-binding-contract")["warnings"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        fs::write(root.join("docx-fast-node/package.json"),json!({"version":"1.2.2","repository":{"url":"https://github.com/josaum/parsers-rs.git"}}).to_string()).unwrap();
        assert_eq!(
            check(root, "parsers-binding-contract")["warnings"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        fs::write(root.join("docx-fast-node/package.json"),json!({"version":"1.2.3","repository":{"url":"https://github.com/josaum/parsers-rs.git"}}).to_string()).unwrap();
        for path in ["ontology-fast-py/Cargo.toml", "json-fast-node/Cargo.toml"] {
            fs::write(root.join(path), "[package]\nversion = \"1.2.2\"\n").unwrap();
            let result = check(root, "parsers-binding-contract");
            assert_eq!(result["warnings"].as_array().unwrap().len(), 1, "{result}");
            fs::write(root.join(path), "[package]\nversion = \"1.2.3\"\n").unwrap();
        }
    }
    #[test]
    fn release_bindings_include_ontology_python_and_json_node() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let result = check(&root, "parsers-binding-contract");
        assert_eq!(result["warnings"], json!([]), "{result}");
        let evidence = result["evidence"].as_array().unwrap();
        for path in [
            "ontology-fast-py/Cargo.toml",
            "json-fast-node/Cargo.toml",
            "json-fast-node/package.json",
        ] {
            assert!(
                evidence.iter().any(|entry| entry["path"] == path),
                "missing binding evidence for {path}"
            );
        }
    }
}
