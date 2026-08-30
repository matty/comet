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

    // Literals that legitimately contain a route-shaped needle and are not
    // hosted authority. Removed from the text before the search rather than
    // spelled `concat!("_x.ai/ses", "sion/…")` at each call site: this repo's
    // conventions lean on grepping a wire method name, and obfuscating the
    // constant made `grep -r "_x.ai/session/prompt_complete" crates/` find
    // nothing — in the very file whose doc calls itself that method's primary
    // citation. The guard's own needles are split because they must not match
    // this file, which is an unavoidable self-reference; nothing else here is.
    let exempt = [
        // Grok's ACP completion notification: a JSON-RPC method name over the
        // agent's stdio, not an HTTP route to any hosted service.
        "_x.ai/session/prompt_complete",
    ];

    let mut violations = Vec::new();
    for root in roots {
        collect_violations(&root, &forbidden, &exempt, &mut violations);
    }
    assert!(
        violations.is_empty(),
        "hosted-authority/migration remnants:\n{}",
        violations.join("\n")
    );
}

fn collect_violations(
    root: &Path,
    forbidden: &[&str],
    exempt: &[&str],
    violations: &mut Vec<String>,
) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_violations(&path, forbidden, exempt, violations);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && path
                .file_name()
                .is_none_or(|name| name != "no_runtime_cloud.rs")
        {
            let mut source = fs::read_to_string(&path).unwrap();
            for allowed in exempt {
                source = source.replace(allowed, "");
            }
            for needle in forbidden {
                if source.contains(needle) {
                    violations.push(format!("{}: {needle}", path.display()));
                }
            }
        }
    }
}

/// The reply the UI's identity notice hangs on (D96), asserted against the
/// literal JSON the engine sends rather than through a Rust type — the same
/// reason `decode_models_reply`'s test does, per AGENTS.md: a reshaped reply
/// that still round-trips through a struct would keep this green while the
/// picker (or here, the notice) broke at runtime.
#[tokio::test]
async fn local_device_reports_no_identity_rebuild_on_an_ordinary_install() {
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig::for_test(dir.path());
    let runtime = Engine::assemble_runtime(&config).await.unwrap();
    let reply = runtime
        .core()
        .rpc_service()
        .handle(methods::LOCAL_DEVICE, json!({}))
        .await
        .expect("LocalDevice answers");

    let body = match reply {
        comet_rpc::RpcReply::Value(value) => value,
        comet_rpc::RpcReply::Stream(_) => panic!("LocalDevice is a unary reply"),
    };
    assert!(
        body.get("deviceId").and_then(|v| v.as_str()).is_some(),
        "the existing field must survive an additive change: {body}"
    );
    assert!(
        body.get("identityRebuiltAt").is_some_and(|v| v.is_null()),
        "a fresh install reports the key as null, never as a stamp: {body}"
    );
    runtime.shutdown().await;
}
