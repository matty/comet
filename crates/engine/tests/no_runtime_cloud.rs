use std::{fs, path::Path};

use comet_engine::{Engine, EngineConfig};
use comet_rpc::{RpcService, methods};
use serde_json::json;

#[tokio::test]
async fn no_runtime_cloud_fresh_engine_starts_without_account_or_runtime_edge() {
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig::for_test(dir.path());
    let runtime = Engine::assemble_runtime(&config).await.unwrap();
    let local = runtime
        .core()
        .rpc_service()
        .handle(methods::LOCAL_DEVICE, json!({}))
        .await;
    assert!(local.is_ok());
    runtime.shutdown().await;
}

#[test]
fn no_runtime_cloud_sources_do_not_reintroduce_hosted_authority_or_migration() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let roots = [workspace.join("crates"), workspace.join("apps/comet")];
    let forbidden = [
        concat!("/au", "th/"),
        concat!("/work", "space/"),
        concat!("/ses", "sion/"),
        concat!("/dev", "ice/"),
        concat!("COMET_", "EDGE_"),
        concat!("COMET_", "WORKOS"),
        concat!("Work", "OS"),
        concat!("comet ", "migrate"),
        concat!("Legacy", "Profile"),
        concat!("migrated", "_from"),
        concat!("prepare_", "local_store"),
        concat!("RECOVERY_", "COMMAND"),
    ];

    let mut violations = Vec::new();
    for root in roots {
        collect_violations(&root, &forbidden, &mut violations);
    }
    assert!(
        violations.is_empty(),
        "hosted-authority/migration remnants:\n{}",
        violations.join("\n")
    );
}

fn collect_violations(root: &Path, forbidden: &[&str], violations: &mut Vec<String>) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_violations(&path, forbidden, violations);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && path
                .file_name()
                .is_none_or(|name| name != "no_runtime_cloud.rs")
        {
            let source = fs::read_to_string(&path).unwrap();
            for needle in forbidden {
                if source.contains(needle) {
                    violations.push(format!("{}: {needle}", path.display()));
                }
            }
        }
    }
}
