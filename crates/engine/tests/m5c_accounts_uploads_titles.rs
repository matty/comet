//! M5c integration: agent-account slot mechanics (claude-swap), uploads
//! chunk→commit→readback + path jail, chat auto-titling with the mock harness,
//! and the RPC dispatch for each new method over the memory transport.
//!
//! Account tests use explicit `AgentAccountsConfig` paths under a tempdir (never
//! the real `~/.claude` / `~/.codex`), so they are hermetic and parallel-safe.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;

use comet_engine::{
    AgentAccounts, AgentAccountsConfig, EngineCore, HarnessRegistry, Repos, Uploads,
    worktree_branch_from_title,
};
use comet_harness::mock::MockHarness;
use comet_proto::{AgentAccountsSnapshot, AgentEvent, DoneStatus, HarnessId, SandboxLevel};
use comet_rpc::methods;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// AgentAccounts wired to temp claude/codex homes.
fn test_accounts(root: &Path) -> (AgentAccounts, AgentAccountsConfig) {
    let config = AgentAccountsConfig {
        data_dir: root.join("data"),
        claude_config_dir: root.join("claude"),
        claude_config_file: root.join("claude.json"),
        codex_home: root.join("codex"),
    };
    (AgentAccounts::new(config.clone()), config)
}

fn write_claude_login(config: &AgentAccountsConfig, email: &str, uuid: &str, token: &str) {
    std::fs::create_dir_all(&config.claude_config_dir).expect("claude dir");
    std::fs::write(
        &config.claude_config_file,
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": uuid,
                "emailAddress": email,
                "displayName": "Test User",
                "organizationName": "Test Org",
                "organizationType": "claude_max",
                "organizationRateLimitTier": "default_claude_max_20x",
            },
            "userID": format!("user-{uuid}"),
            "projects": { "/keep/me": { "history": [] } },
        })
        .to_string(),
    )
    .expect("claude config");
    std::fs::write(
        config.claude_config_dir.join(".credentials.json"),
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": token,
                "refreshToken": format!("refresh-{token}"),
                // Far-future expiry: usage probes must never try to rotate it.
                "expiresAt": 4_102_444_800_000i64,
            }
        })
        .to_string(),
    )
    .expect("claude creds");
}

/// An unsigned JWT with the claims codex mines from `id_token`.
fn fake_id_token(email: &str, account_id: &str, plan: &str) -> String {
    let header = BASE64_URL.encode(br#"{"alg":"none"}"#);
    let payload = BASE64_URL.encode(
        serde_json::json!({
            "email": email,
            "name": "Codex User",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
                "chatgpt_plan_type": plan,
            },
        })
        .to_string(),
    );
    format!("{header}.{payload}.x")
}

fn write_codex_login(config: &AgentAccountsConfig, email: &str, account_id: &str) {
    std::fs::create_dir_all(&config.codex_home).expect("codex home");
    std::fs::write(
        config.codex_home.join("auth.json"),
        serde_json::json!({
            "tokens": {
                "id_token": fake_id_token(email, account_id, "plus"),
                "access_token": format!("at-{account_id}"),
                "account_id": account_id,
            }
        })
        .to_string(),
    )
    .expect("codex auth");
}

fn account_emails(snapshot: &AgentAccountsSnapshot, harness: HarnessId) -> Vec<(String, bool)> {
    snapshot
        .accounts
        .iter()
        .filter(|a| a.harness == harness)
        .map(|a| (a.email.clone().unwrap_or_default(), a.active))
        .collect()
}

fn assemble_with_mock(dir: &Path, script: Vec<AgentEvent>) -> EngineCore {
    std::fs::create_dir_all(dir).expect("data dir");
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness::with_script(script)));
    EngineCore::assemble(dir, Arc::new(registry), HarnessId::Mock, None).expect("engine assembles")
}

async fn git(cwd: &Path, args: &[&str]) {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .await
        .expect("git spawns");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("repo dir");
    git(dir, &["init", "-b", "main"]).await;
    std::fs::write(dir.join("a.txt"), "one\n").expect("write a.txt");
    git(dir, &["add", "."]).await;
    git(dir, &["commit", "-m", "initial"]).await;
}

/// Poll until `probe` yields Some, or panic at the deadline.
async fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Agent accounts — claude slot swap round trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claude_slot_swap_round_trip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());

    // Live login = Alice. Listing detects + auto-snapshots her into a slot.
    write_claude_login(&config, "alice@example.com", "uuid-alice", "token-alice");
    let snapshot = accounts.list(false).await.expect("list");
    assert_eq!(
        account_emails(&snapshot, HarnessId::ClaudeCode),
        vec![("alice@example.com".to_string(), true)]
    );
    let alice = &snapshot.accounts[0];
    assert_eq!(
        alice.plan_label.as_deref(),
        Some("Max 20×"),
        "plan label parse"
    );
    assert_eq!(alice.display_name.as_deref(), Some("Test User"));
    assert_eq!(alice.organization.as_deref(), Some("Test Org"));
    assert!(alice.switchable);
    assert!(snapshot.warnings.is_empty());
    let alice_id = alice.id.clone();
    assert_eq!(alice_id.len(), 16, "slot id is 16 hex chars");

    // Bob logs in via the CLI (live files replaced) — next list snapshots Bob
    // and shows Alice as a saved, inactive slot.
    write_claude_login(&config, "bob@example.com", "uuid-bob", "token-bob");
    let snapshot = accounts.list(false).await.expect("list bob");
    let mut emails = account_emails(&snapshot, HarnessId::ClaudeCode);
    emails.sort();
    assert_eq!(
        emails,
        vec![
            ("alice@example.com".to_string(), false),
            ("bob@example.com".to_string(), true)
        ]
    );

    // Activate Alice: her slot's tokens land in the live files, Bob's live
    // session is auto-snapshotted first, identity merged into claude.json.
    let snapshot = accounts
        .activate(HarnessId::ClaudeCode, &alice_id)
        .await
        .expect("activate");
    let mut emails = account_emails(&snapshot, HarnessId::ClaudeCode);
    emails.sort();
    assert_eq!(
        emails,
        vec![
            ("alice@example.com".to_string(), true),
            ("bob@example.com".to_string(), false)
        ]
    );
    let creds: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(config.claude_config_dir.join(".credentials.json"))
            .expect("creds readable"),
    )
    .expect("creds json");
    assert_eq!(creds["claudeAiOauth"]["accessToken"], "token-alice");
    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config.claude_config_file).expect("cfg"))
            .expect("cfg json");
    assert_eq!(cfg["oauthAccount"]["emailAddress"], "alice@example.com");
    assert_eq!(cfg["userID"], "user-uuid-alice");
    // The rest of the config survived the merge (only identity fields swapped).
    assert!(
        cfg["projects"]["/keep/me"].is_object(),
        "unrelated config keys preserved"
    );

    // Slot files: exactly two, under data/agent-accounts/claude-code.
    let slots_dir = config.data_dir.join("agent-accounts").join("claude-code");
    let slot_count = std::fs::read_dir(&slots_dir)
        .expect("slots dir")
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .count();
    assert_eq!(slot_count, 2);

    // Corrupt claude.json → activate must refuse rather than wipe it.
    std::fs::write(&config.claude_config_file, "{ definitely not json").expect("corrupt");
    let bob_id = snapshot
        .accounts
        .iter()
        .find(|a| a.email.as_deref() == Some("bob@example.com"))
        .expect("bob listed")
        .id
        .clone();
    let refused = accounts.activate(HarnessId::ClaudeCode, &bob_id).await;
    assert!(refused.is_err(), "parse-failed config must block the swap");
    assert_eq!(
        std::fs::read_to_string(&config.claude_config_file).expect("still there"),
        "{ definitely not json",
        "the unparsable config was left untouched"
    );
}

#[tokio::test]
async fn claude_account_switch_keeps_live_mcp_oauth() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());
    let creds_file = config.claude_config_dir.join(".credentials.json");

    write_claude_login(&config, "alice@example.com", "uuid-alice", "token-alice");
    // Alice's first snapshot includes a MCP token that will go stale.
    std::fs::write(
        &creds_file,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "token-alice",
                "refreshToken": "refresh-token-alice",
                "expiresAt": 4_102_444_800_000i64,
            },
            "mcpOAuth": { "github": { "accessToken": "stale-github" } },
            "pluginSecrets": { "old": true },
            "trustedDeviceToken": "alice-device",
        })
        .to_string(),
    )
    .expect("alice mcp creds");
    let snapshot = accounts.list(false).await.expect("list alice");
    let alice_id = snapshot
        .accounts
        .iter()
        .find(|a| a.email.as_deref() == Some("alice@example.com"))
        .expect("alice listed")
        .id
        .clone();

    // Bob becomes live; MCP tokens rotate while he is the active login.
    write_claude_login(&config, "bob@example.com", "uuid-bob", "token-bob");
    std::fs::write(
        &creds_file,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "token-bob",
                "refreshToken": "refresh-token-bob",
                "expiresAt": 4_102_444_800_000i64,
            },
            "mcpOAuth": { "github": { "accessToken": "live-github" } },
            "pluginSecrets": { "live": true },
            "trustedDeviceToken": "bob-device",
        })
        .to_string(),
    )
    .expect("bob mcp creds");
    accounts.list(false).await.expect("list bob");

    accounts
        .activate(HarnessId::ClaudeCode, &alice_id)
        .await
        .expect("activate alice");

    let creds: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&creds_file).expect("creds readable"))
            .expect("creds json");
    assert_eq!(creds["claudeAiOauth"]["accessToken"], "token-alice");
    assert_eq!(
        creds["trustedDeviceToken"], "alice-device",
        "account-bound device token stays with the slot"
    );
    assert_eq!(
        creds["mcpOAuth"]["github"]["accessToken"], "live-github",
        "live MCP OAuth must survive the switch, not Alice's stale snapshot"
    );
    assert_eq!(creds["pluginSecrets"]["live"], true);
    assert!(
        creds["pluginSecrets"].get("old").is_none(),
        "slot plugin secrets must not clobber the live generation"
    );
}

#[tokio::test]
async fn claude_account_switch_keeps_mcp_when_target_slot_has_none() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());
    let creds_file = config.claude_config_dir.join(".credentials.json");

    // Alice saved via the oauth-only shape (new login / usage refresh).
    write_claude_login(&config, "alice@example.com", "uuid-alice", "token-alice");
    let snapshot = accounts.list(false).await.expect("list alice");
    let alice_id = snapshot.accounts[0].id.clone();

    write_claude_login(&config, "bob@example.com", "uuid-bob", "token-bob");
    std::fs::write(
        &creds_file,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "token-bob",
                "refreshToken": "refresh-token-bob",
                "expiresAt": 4_102_444_800_000i64,
            },
            "mcpOAuth": { "linear": { "accessToken": "live-linear" } },
        })
        .to_string(),
    )
    .expect("bob mcp creds");
    accounts.list(false).await.expect("list bob");

    accounts
        .activate(HarnessId::ClaudeCode, &alice_id)
        .await
        .expect("activate alice");

    let creds: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&creds_file).expect("creds readable"))
            .expect("creds json");
    assert_eq!(creds["claudeAiOauth"]["accessToken"], "token-alice");
    assert_eq!(creds["mcpOAuth"]["linear"]["accessToken"], "live-linear");
}

#[tokio::test]
async fn codex_slot_swap_and_api_key_detection() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());

    write_codex_login(&config, "carol@example.com", "acct-carol");
    let snapshot = accounts.list(false).await.expect("list");
    let carol = snapshot
        .accounts
        .iter()
        .find(|a| a.harness == HarnessId::Codex)
        .expect("codex account");
    assert_eq!(carol.email.as_deref(), Some("carol@example.com"));
    assert_eq!(carol.plan_label.as_deref(), Some("ChatGPT Plus"));
    assert!(carol.active);
    let carol_id = carol.id.clone();

    // Second login (Dave) becomes live; swap back to Carol.
    write_codex_login(&config, "dave@example.com", "acct-dave");
    accounts.list(false).await.expect("list dave");
    let snapshot = accounts
        .activate(HarnessId::Codex, &carol_id)
        .await
        .expect("activate carol");
    let mut emails = account_emails(&snapshot, HarnessId::Codex);
    emails.sort();
    assert_eq!(
        emails,
        vec![
            ("carol@example.com".to_string(), true),
            ("dave@example.com".to_string(), false)
        ]
    );
    let auth: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(config.codex_home.join("auth.json")).expect("auth"),
    )
    .expect("auth json");
    assert_eq!(auth["tokens"]["account_id"], "acct-carol");

    // API-key mode: no tokens, just the key.
    std::fs::write(
        config.codex_home.join("auth.json"),
        serde_json::json!({ "OPENAI_API_KEY": "sk-test-12345678abcd" }).to_string(),
    )
    .expect("api key auth");
    let snapshot = accounts.list(false).await.expect("list api key");
    let key_account = snapshot
        .accounts
        .iter()
        .find(|a| a.harness == HarnessId::Codex && a.active)
        .expect("api key account");
    assert_eq!(key_account.plan_label.as_deref(), Some("API key"));
    assert_eq!(key_account.email.as_deref(), Some("API key ·…abcd"));
}

#[tokio::test]
async fn forget_guards_and_removes_slots() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());
    write_claude_login(&config, "alice@example.com", "uuid-alice", "token-alice");
    let snapshot = accounts.list(false).await.expect("list");
    let alice_id = snapshot.accounts[0].id.clone();

    // Path-shaped ids never reach the filesystem.
    assert!(
        accounts
            .forget(HarnessId::ClaudeCode, "../../evil")
            .await
            .is_err()
    );
    assert!(
        accounts
            .forget(HarnessId::ClaudeCode, "ABCDEF0123456789")
            .await
            .is_err()
    );
    // The live login can't be forgotten (it would just be re-detected).
    assert!(
        accounts
            .forget(HarnessId::ClaudeCode, &alice_id)
            .await
            .is_err()
    );

    // A non-active slot forgets cleanly.
    write_claude_login(&config, "bob@example.com", "uuid-bob", "token-bob");
    accounts.list(false).await.expect("list bob");
    let snapshot = accounts
        .forget(HarnessId::ClaudeCode, &alice_id)
        .await
        .expect("forget alice");
    assert_eq!(
        account_emails(&snapshot, HarnessId::ClaudeCode),
        vec![("bob@example.com".to_string(), true)]
    );
}

#[test]
fn snapshot_wire_shape() {
    let snapshot = AgentAccountsSnapshot::default();
    let value = serde_json::to_value(&snapshot).expect("serializes");
    assert_eq!(value, serde_json::json!({ "accounts": [], "warnings": [] }));
}

#[tokio::test]
async fn claude_login_flow_is_pkce_paste_code() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, _) = test_accounts(tmp.path());
    let start = accounts
        .start_login(HarnessId::ClaudeCode)
        .await
        .expect("start");
    assert!(
        start
            .url
            .starts_with("https://claude.ai/oauth/authorize?code=true")
    );
    assert!(start.url.contains("code_challenge_method=S256"));
    assert!(
        start
            .url
            .contains("redirect_uri=https%3A%2F%2Fconsole.anthropic.com")
    );
    let mode = serde_json::to_value(start.mode).expect("mode");
    assert_eq!(mode, serde_json::json!("paste-code"));

    // Claude flows poll as pending (paste-code completes them); cancel drops the
    // flow so the next poll reports it expired.
    let poll = accounts.poll_login(&start.login_id).await.expect("poll");
    assert_eq!(
        serde_json::to_value(poll.status).expect("status"),
        serde_json::json!("pending")
    );
    accounts.cancel_login(&start.login_id);
    assert!(
        accounts.poll_login(&start.login_id).await.is_err(),
        "cancelled flow is gone"
    );
    assert!(
        accounts
            .complete_login(&start.login_id, "code#state")
            .await
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// Uploads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn uploads_chunk_commit_readback_and_jail() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let uploads = Uploads::new(tmp.path());

    // 100KB of pseudo-random bytes, staged as three positional base64 chunks
    // (out of order, with one retried) — chunk boundaries are multiples of 3
    // bytes so independent base64 strings concatenate losslessly.
    let payload: Vec<u8> = (0..100_002u32)
        .map(|i| (i.wrapping_mul(31) % 251) as u8)
        .collect();
    let chunks: Vec<String> = payload.chunks(45_000).map(|c| BASE64.encode(c)).collect();
    assert_eq!(chunks.len(), 3);
    uploads
        .append("up-1", &chunks[2], Some(2))
        .expect("chunk 2");
    uploads
        .append("up-1", &chunks[0], Some(0))
        .expect("chunk 0");
    uploads
        .append("up-1", &chunks[0], Some(0))
        .expect("chunk 0 retry is idempotent");
    uploads
        .append("up-1", &chunks[1], Some(1))
        .expect("chunk 1");
    let path = uploads.commit("up-1", "photo.png").expect("commit");
    assert!(path.ends_with("up-1-photo.png"), "path: {path}");
    assert_eq!(std::fs::read(&path).expect("committed file"), payload);

    // Readback: chunked reassembly round-trips.
    let mut assembled = Vec::new();
    let mut offset = 0u64;
    loop {
        let chunk = uploads.read_chunk(&path, offset, &[]).expect("read chunk");
        assert_eq!(chunk.mime_type, "image/png");
        assert_eq!(chunk.name, "up-1-photo.png");
        assembled.extend(BASE64.decode(&chunk.data).expect("chunk base64"));
        offset = chunk.next_offset;
        if chunk.done {
            break;
        }
    }
    assert_eq!(assembled, payload);

    // Missing chunk → commit fails.
    uploads
        .append("up-2", &chunks[0], Some(0))
        .expect("chunk 0");
    uploads
        .append("up-2", &chunks[2], Some(2))
        .expect("chunk 2 (hole at 1)");
    assert!(
        uploads.commit("up-2", "holey.png").is_err(),
        "hole detected"
    );

    // Path jail: files outside the uploads dir (and outside any allowed cwd
    // root) are rejected, including traversal attempts and the dir itself.
    let outside = tmp.path().join("outside.png");
    std::fs::write(&outside, b"nope").expect("outside file");
    assert!(
        uploads
            .read_chunk(&outside.to_string_lossy(), 0, &[])
            .is_err()
    );
    assert!(uploads.read_chunk("/etc/passwd", 0, &[]).is_err());
    let sneaky = format!("{}/../outside.png", uploads.dir().display());
    assert!(
        uploads.read_chunk(&sneaky, 0, &[]).is_err(),
        "traversal rejected"
    );
    // …but a workspace-known cwd root admits its files.
    let ok = uploads
        .read_chunk(&outside.to_string_lossy(), 0, &[tmp.path().to_path_buf()])
        .expect("cwd-rooted read");
    assert_eq!(BASE64.decode(&ok.data).expect("data"), b"nope");
    // Non-image extensions are refused even inside the jail (comet parity).
    let text = PathBuf::from(uploads.dir()).join("notes.txt");
    std::fs::create_dir_all(uploads.dir()).expect("uploads dir");
    std::fs::write(&text, b"text").expect("txt");
    assert!(uploads.read_chunk(&text.to_string_lossy(), 0, &[]).is_err());

    // Bogus upload ids never become paths.
    assert!(uploads.append("../evil", "aGk=", None).is_err());
    assert!(uploads.commit("unknown-upload", "x.png").is_err());
}

// ---------------------------------------------------------------------------
// Titling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn titling_e2e_names_chat_and_renames_worktree_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Worktree root must be inside the tempdir (EngineCore reads the env-less
    // default otherwise) — create the worktree with a dedicated Repos handle.
    let repo_dir = tmp.path().join("repo");
    init_repo(&repo_dir).await;
    let repos = Repos::with_worktrees_root(
        &tmp.path().join("data"),
        "device-test",
        tmp.path().join("worktrees"),
    );
    let worktree = repos
        .create_worktree(&repo_dir, "main")
        .await
        .expect("worktree");

    let core = assemble_with_mock(
        &tmp.path().join("data"),
        vec![
            AgentEvent::TextDelta {
                text: "Fix Login Flow".into(),
            },
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            },
        ],
    );
    let chat_id = "chat-title-1";
    core.workspace
        .create_space(
            "space-title",
            &core.device_id,
            &repo_dir.to_string_lossy(),
            None,
            true,
        )
        .expect("create space");
    core.workspace
        .create_chat(chat_id, "space-title", None, Some(worktree.path.clone()))
        .expect("create chat");
    core.workspace
        .set_chat_branch(chat_id, &worktree.branch)
        .expect("set branch");

    let request = comet_proto::RunRequest {
        prompt: "please fix the login flow".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: worktree.path.clone(),
        runtime_mode: comet_proto::RuntimeMode::default(),
        sandbox: SandboxLevel::WorkspaceWrite,
        attachments: Vec::new(),
        resume: None,
    };
    core.sessions
        .dispatch(chat_id, HarnessId::Mock, request, None)
        .await
        .expect("dispatch");

    // The mock's scripted reply doubles as the titling model's output.
    //
    // Gate on BOTH fields this test asserts on, because their visibility to a
    // reader is not ordered. The titler renames the worktree branch (step 5)
    // before it writes the title (step 6), yet a snapshot taken on a non-empty
    // title routinely still held the PRE-rename branch — so write order buys
    // nothing here, and gating on either field alone leaves the other racy in
    // whichever direction that reader happens to lose.
    //
    // Wait for both to have CHANGED, then assert their values: a wrong title or
    // a wrongly derived branch name still fails the assertions below rather than
    // being absorbed into the wait.
    let original_branch = worktree.branch.clone();
    let chat = wait_for("titled chat on its renamed branch", || {
        core.workspace
            .doc()
            .chat(chat_id)
            .ok()
            .flatten()
            .filter(|c| {
                c.title.as_deref().is_some_and(|t| !t.is_empty())
                    && c.branch.as_deref().is_some_and(|b| b != original_branch)
            })
    })
    .await;
    assert_eq!(chat.title.as_deref(), Some("Fix Login Flow"));
    // Branch renamed from the title, chat row updated to match.
    assert_eq!(chat.branch.as_deref(), Some("comet/fix-login-flow"));
    let head = tokio::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(&worktree.path)
        .output()
        .await
        .expect("git");
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        "comet/fix-login-flow"
    );

    // A titled chat is never re-titled: rename, run again, title sticks.
    core.workspace
        .rename_chat(chat_id, "My Custom Name")
        .expect("rename");
    let request = comet_proto::RunRequest {
        prompt: "another request".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: worktree.path.clone(),
        runtime_mode: comet_proto::RuntimeMode::default(),
        sandbox: SandboxLevel::WorkspaceWrite,
        attachments: Vec::new(),
        resume: None,
    };
    core.sessions
        .dispatch(chat_id, HarnessId::Mock, request, None)
        .await
        .expect("second dispatch");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let chat = core
        .workspace
        .doc()
        .chat(chat_id)
        .expect("chat")
        .expect("row");
    assert_eq!(chat.title.as_deref(), Some("My Custom Name"));
    core.shutdown().await;
}

/// A harness whose titling call (the request whose prompt matches the
/// titling prompt's own signature — see `titles::run_title_model`) blocks on
/// a test-controlled gate, while its ordinary chat turn resolves instantly.
/// `TitleGenerator::maybe_generate_upfront` fires at request start
/// (`sessions.rs`'s dispatch, well before the main turn's own `drive_run`
/// task is even spawned) and is fully detached from `dispatch`'s own await
/// chain — by the time `dispatch(...).await` returns, an ungated titling
/// call may already have completed. Racing it needs a handle independent of
/// `dispatch`'s own timing, which is what this gate buys.
struct GatedTitlingHarness {
    id: HarnessId,
    gate: Arc<tokio::sync::Notify>,
    title: String,
}

#[async_trait::async_trait]
impl comet_harness::Harness for GatedTitlingHarness {
    fn id(&self) -> HarnessId {
        self.id
    }
    fn display_name(&self) -> &str {
        "Gated"
    }
    fn capabilities(&self) -> comet_proto::HarnessCapabilities {
        comet_proto::HarnessCapabilities::default()
    }
    async fn models(&self) -> Result<comet_proto::ModelCatalog, comet_harness::HarnessError> {
        Ok(comet_proto::ModelCatalog::built_in(Vec::new()))
    }
    async fn run(
        &self,
        request: comet_proto::RunRequest,
        _controls: comet_harness::RunControls,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<comet_proto::AgentEvent, comet_harness::HarnessError>,
        >,
        comet_harness::HarnessError,
    > {
        if request.prompt.starts_with("Reply with ONLY a concise") {
            self.gate.notified().await;
        }
        let events = vec![
            Ok(AgentEvent::TextDelta {
                text: self.title.clone(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// D116: `generate` must write the title BEFORE renaming the worktree
/// branch, matching its sibling `apply_agent_title` — never the other way
/// around, or a title that fails to land leaves the branch renamed for
/// nothing the chat ever shows.
///
/// The titling call is held on [`GatedTitlingHarness`]'s gate so the test
/// controls exactly when `generate`'s own model call resolves — that part
/// is deterministic. What is NOT deterministic is catching the bug's own
/// window: under the pre-fix ordering, the branch-doc commit and the
/// title-doc commit sit back to back with no `.await` between them, so an
/// external observer (the racer below) can only ever see it by a genuine,
/// CPU-instruction-scale timing race. See the per-attempt loop's own
/// comment for how that is made reliable without a flaky test.
#[tokio::test]
async fn generate_never_renames_the_branch_for_a_title_that_lost_the_write_race() {
    // The bug's own window (see D116: the branch-doc commit and the
    // title-doc commit sit back to back with no `.await` between them in
    // the buggy ordering) is CPU-instruction-scale, so whether a busy-spin
    // racer thread observes it at all is a genuine timing race rather than
    // something the test can sequence: the check itself (a doc read through
    // a lock) is not guaranteed faster than the window it is trying to
    // observe. One attempt is therefore not, on its own, trustworthy
    // evidence. Repeating the whole race up to `ATTEMPTS` times is: the
    // fixed ordering can NEVER trip the racer's trigger condition at all
    // (title only ever becomes visible together with, or after, the branch
    // — never before it, by construction, since the write happens first and
    // nothing yields between the two commits), so it always falls through
    // to the plain success-path assertions, deterministically, every
    // attempt. The buggy ordering trips it with the same per-attempt
    // probability every time, and in practice that probability is high:
    // falsified against the pre-fix ordering on Windows, 22 of 22 runs
    // failed and 21 of them caught the window on attempt 0. The racer
    // thread hammering the same doc lock is why — the titler has to
    // re-acquire it to write the title, and a spinning thread usually wins
    // that handoff. `ATTEMPTS` is insurance for a slower or more contended
    // machine where the window IS missed, not a claim that one attempt is
    // unreliable here: each independent attempt multiplies the miss chance,
    // so ten of them make a silent pass against a reintroduced bug
    // vanishingly unlikely. The repo is created once, outside the loop —
    // only the worktree (which needs a fresh, unrenamed `comet/<name>`
    // branch every time) and the engine's own data dir are per-attempt, to
    // keep the whole test well under `.config/nextest.toml`'s 60s
    // slow-test bound (measured ~5.5s for all ten on the fixed ordering).
    const ATTEMPTS: usize = 10;

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_repo(&repo_dir).await;
    let repos = Repos::with_worktrees_root(
        &tmp.path().join("worktrees-data"),
        "device-test",
        tmp.path().join("worktrees"),
    );

    for attempt in 0..ATTEMPTS {
        let worktree = repos
            .create_worktree(&repo_dir, "main")
            .await
            .expect("worktree");

        let gate = Arc::new(tokio::sync::Notify::new());
        let data_dir = tmp.path().join(format!("data-{attempt}"));
        std::fs::create_dir_all(&data_dir).expect("data dir");
        let registry = HarnessRegistry::new();
        registry.register(Arc::new(GatedTitlingHarness {
            id: HarnessId::Mock,
            gate: gate.clone(),
            title: "Fix Login Flow".into(),
        }));
        let core = EngineCore::assemble(&data_dir, Arc::new(registry), HarnessId::Mock, None)
            .expect("engine assembles");

        let chat_id = "chat-title-race";
        core.workspace
            .create_space(
                "space-title-race",
                &core.device_id,
                &repo_dir.to_string_lossy(),
                None,
                true,
            )
            .expect("create space");
        core.workspace
            .create_chat(
                chat_id,
                "space-title-race",
                None,
                Some(worktree.path.clone()),
            )
            .expect("create chat");
        core.workspace
            .set_chat_branch(chat_id, &worktree.branch)
            .expect("set branch");

        let request = comet_proto::RunRequest {
            prompt: "please fix the login flow".into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: serde_json::Map::new(),
            cwd: worktree.path.clone(),
            runtime_mode: comet_proto::RuntimeMode::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
            attachments: Vec::new(),
            resume: None,
        };
        core.sessions
            .dispatch(chat_id, HarnessId::Mock, request, None)
            .await
            .expect("dispatch");

        // The upfront titling call fired inside `dispatch` (before the main
        // turn's own task was even spawned) is parked on the gate — the
        // chat is guaranteed untitled here, deterministically, not by luck.
        let gated = core
            .workspace
            .doc()
            .chat(chat_id)
            .expect("chat")
            .expect("row");
        assert!(
            gated.title.is_none(),
            "the titling call must still be gated at this point"
        );
        let original_branch = worktree.branch.clone();

        // A busy-spin racer on tokio's blocking pool (a real OS thread,
        // checked with no sleep at all — `tokio::time::sleep`-based
        // polling, even at 1ms, was too coarse and always observed both
        // fields already changed together): the instant it sees the branch
        // already renamed but the title still unset, that IS the bug's
        // window, and it fires a manual rename right then, from that
        // thread, so first-writer-wins makes the auto-titler's own
        // still-pending write lose.
        let racer_ws = core.workspace.clone();
        let racer_chat_id = chat_id.to_string();
        let racer_branch = original_branch.clone();
        let racer = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Ok(Some(chat)) = racer_ws.doc().chat(&racer_chat_id) {
                    if chat.title.is_none() && chat.branch.as_deref() != Some(racer_branch.as_str())
                    {
                        let _ = racer_ws.rename_chat(&racer_chat_id, "Manually Renamed Meanwhile");
                        // Caught the bug's window: the branch had already
                        // moved but the write had not landed yet.
                        return true;
                    }
                    if chat.title.is_some() {
                        // The write already landed before the branch could
                        // be observed as unrenamed-but-titled — nothing to
                        // race (always true under the fix; sometimes true
                        // under the bug, when this attempt's racer thread
                        // loses the underlying timing).
                        return false;
                    }
                }
                if std::time::Instant::now() > deadline {
                    return false;
                }
            }
        });

        gate.notify_one();
        let caught_the_race = racer.await.expect("racer task");

        if caught_the_race {
            // The bug: the branch had already been renamed, but the write
            // the racer caught mid-flight had NOT landed yet (the racer
            // fired the manual rename right in that window).
            // First-writer-wins means the auto-titler's own write, once it
            // finally runs, must lose.
            let chat = wait_for("the manual rename to be the final word", || {
                core.workspace
                    .doc()
                    .chat(chat_id)
                    .ok()
                    .flatten()
                    .filter(|c| c.title_manual)
            })
            .await;
            assert_eq!(
                chat.title.as_deref(),
                Some("Manually Renamed Meanwhile"),
                "first-writer-wins: the manual rename must stick (attempt {attempt})"
            );

            // Give any still in-flight branch-rename work a moment to
            // finish, then assert it never touched the branch: the
            // auto-titler's title LOST the write, so it must never have
            // reached git.
            tokio::time::sleep(Duration::from_millis(500)).await;
            let chat = core
                .workspace
                .doc()
                .chat(chat_id)
                .expect("chat")
                .expect("row");
            assert_eq!(
                chat.branch.as_deref(),
                Some(original_branch.as_str()),
                "a title that lost the write race must not rename the worktree branch \
                 (caught on attempt {attempt})"
            );
            core.shutdown().await;
            return;
        }

        // Missed the window this attempt (or — under the fix — the window
        // structurally never opens at all): fall through to the plain
        // success-path assertions as a sanity check, then try again.
        let chat = wait_for("branch renamed to the landed title", || {
            core.workspace
                .doc()
                .chat(chat_id)
                .ok()
                .flatten()
                .filter(|c| {
                    c.title.as_deref() == Some("Fix Login Flow")
                        && c.branch.as_deref().is_some_and(|b| b != original_branch)
                })
        })
        .await;
        // `starts_with`, not an exact match: attempts share one repo (kept
        // outside the loop to stay well under the slow-test bound), so a
        // later attempt's identical title collides with an earlier
        // attempt's already-renamed branch and gets the hash-suffixed form
        // (`rename_worktree_branch`'s own collision handling, exercised
        // elsewhere by `rename_worktree_branch_guards_and_collisions`) —
        // that is expected here, not a bug.
        assert!(
            chat.branch
                .as_deref()
                .is_some_and(|b| b.starts_with("comet/fix-login-flow")),
            "branch should be renamed from the landed title (attempt {attempt}): {:?}",
            chat.branch
        );
        core.shutdown().await;
    }
}

/// A harness registered under an arbitrary [`HarnessId`], streaming a fixed
/// script — stands in for Grok (out of reach in this crate's tests) the same
/// way `crates/engine/src/rpc.rs`'s own `ScriptedHarness` does.
struct ScriptedHarness {
    id: HarnessId,
    script: Vec<AgentEvent>,
}

#[async_trait::async_trait]
impl comet_harness::Harness for ScriptedHarness {
    fn id(&self) -> HarnessId {
        self.id
    }
    fn display_name(&self) -> &str {
        "Scripted"
    }
    fn capabilities(&self) -> comet_proto::HarnessCapabilities {
        comet_proto::HarnessCapabilities::default()
    }
    async fn models(&self) -> Result<comet_proto::ModelCatalog, comet_harness::HarnessError> {
        Ok(comet_proto::ModelCatalog::built_in(Vec::new()))
    }
    async fn run(
        &self,
        _request: comet_proto::RunRequest,
        _controls: comet_harness::RunControls,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<comet_proto::AgentEvent, comet_harness::HarnessError>,
        >,
        comet_harness::HarnessError,
    > {
        let events: Vec<_> = self.script.iter().cloned().map(Ok).collect();
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// PR9's Important 3 fix: before it, only the model-run titling path
/// (`TitleGenerator::generate`, exercised above) renamed the worktree branch
/// from the title — a Grok chat (self-titling, so `generate` never runs) kept
/// its generated branch name forever. `TitleGenerator::apply_agent_title`
/// must do the identical rename.
#[tokio::test]
async fn agent_authored_title_also_renames_the_worktree_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_repo(&repo_dir).await;
    let repos = Repos::with_worktrees_root(
        &tmp.path().join("data"),
        "device-test",
        tmp.path().join("worktrees"),
    );
    let worktree = repos
        .create_worktree(&repo_dir, "main")
        .await
        .expect("worktree");

    std::fs::create_dir_all(tmp.path().join("data")).expect("data dir");
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(ScriptedHarness {
        id: HarnessId::Grok,
        script: vec![
            AgentEvent::SessionStarted {
                harness: HarnessId::Grok,
                model: "grok-code-fast".into(),
                tools: Vec::new(),
                cwd: worktree.path.clone(),
                session_id: "session-1".into(),
                assistant_message_id: "assistant-1".into(),
                runtime_mode: comet_proto::RuntimeMode::default(),
            },
            AgentEvent::SessionTitled {
                title: "Fix Login Flow".into(),
            },
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            },
        ],
    }));
    let core = EngineCore::assemble(
        &tmp.path().join("data"),
        Arc::new(registry),
        HarnessId::Grok,
        None,
    )
    .expect("engine assembles");

    let chat_id = "chat-title-grok";
    core.workspace
        .create_space(
            "space-title-grok",
            &core.device_id,
            &repo_dir.to_string_lossy(),
            None,
            true,
        )
        .expect("create space");
    core.workspace
        .create_chat(
            chat_id,
            "space-title-grok",
            None,
            Some(worktree.path.clone()),
        )
        .expect("create chat");
    core.workspace
        .set_chat_branch(chat_id, &worktree.branch)
        .expect("set branch");

    let request = comet_proto::RunRequest {
        prompt: "please fix the login flow".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: worktree.path.clone(),
        runtime_mode: comet_proto::RuntimeMode::default(),
        sandbox: SandboxLevel::WorkspaceWrite,
        attachments: Vec::new(),
        resume: None,
    };
    core.sessions
        .dispatch(chat_id, HarnessId::Grok, request, None)
        .await
        .expect("dispatch");

    // Same double-gate as the model-run test above: the branch rename is a
    // background task spawned once the title write lands, so its visibility
    // is not ordered against the title's.
    let original_branch = worktree.branch.clone();
    let chat = wait_for("agent-titled chat on its renamed branch", || {
        core.workspace
            .doc()
            .chat(chat_id)
            .ok()
            .flatten()
            .filter(|c| {
                c.title.as_deref() == Some("Fix Login Flow")
                    && c.branch.as_deref().is_some_and(|b| b != original_branch)
            })
    })
    .await;
    assert_eq!(chat.branch.as_deref(), Some("comet/fix-login-flow"));
    let head = tokio::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(&worktree.path)
        .output()
        .await
        .expect("git");
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        "comet/fix-login-flow"
    );

    core.shutdown().await;
}

#[tokio::test]
async fn rename_worktree_branch_guards_and_collisions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_repo(&repo_dir).await;
    let repos = Repos::with_worktrees_root(
        &tmp.path().join("data"),
        "device-test",
        tmp.path().join("worktrees"),
    );
    let wt = repos
        .create_worktree(&repo_dir, "main")
        .await
        .expect("worktree");
    let wt_path = Path::new(&wt.path);

    // Guard: expected branch mismatch → no-op, returns the actual branch.
    let unchanged = repos
        .rename_worktree_branch(wt_path, "comet/not-this-one", "Some Title")
        .await
        .expect("guarded");
    assert_eq!(unchanged, wt.branch);

    // Happy path: renamed to the title slug.
    let renamed = repos
        .rename_worktree_branch(wt_path, &wt.branch, "Add Dark Mode!")
        .await
        .expect("renamed");
    assert_eq!(renamed, "comet/add-dark-mode");

    // Already renamed → the guard (branch no longer comet/<folder>) makes any
    // further title rename a no-op.
    let again = repos
        .rename_worktree_branch(wt_path, "comet/add-dark-mode", "Different Title")
        .await
        .expect("second rename");
    assert_eq!(again, "comet/add-dark-mode");

    // Collision: a second worktree whose title slug already exists gets the
    // stable hash suffix.
    let wt2 = repos
        .create_worktree(&repo_dir, "main")
        .await
        .expect("worktree 2");
    let renamed2 = repos
        .rename_worktree_branch(Path::new(&wt2.path), &wt2.branch, "Add Dark Mode!")
        .await
        .expect("suffixed rename");
    assert!(
        renamed2.starts_with("comet/add-dark-mode-")
            && renamed2.len() == "comet/add-dark-mode-".len() + 6,
        "suffixed: {renamed2}"
    );

    // Slug edge cases.
    assert_eq!(
        worktree_branch_from_title("  Fix `Login` Flow!  "),
        "comet/fix-login-flow"
    );
    assert_eq!(worktree_branch_from_title("***"), "comet/update");
    assert_eq!(
        worktree_branch_from_title("Cafe's Dark Mode"),
        "comet/cafes-dark-mode"
    );
}

// ---------------------------------------------------------------------------
// RPC dispatch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rpc_dispatch_for_m5c_methods() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let core = assemble_with_mock(&tmp.path().join("data"), Vec::new());
    let client = comet_rpc::memory_client(core.rpc_service());

    // Uploads: chunk → commit → readback over the wire.
    let payload = b"fake png bytes".to_vec();
    let ok = client
        .call(
            methods::UPLOAD_CHUNK,
            serde_json::json!({ "uploadId": "rpc-up", "data": BASE64.encode(&payload), "seq": 0 }),
        )
        .await
        .expect("UploadChunk");
    assert_eq!(ok["ok"], true);
    let committed = client
        .call(
            methods::UPLOAD_COMMIT,
            serde_json::json!({ "uploadId": "rpc-up", "fileName": "shot.png" }),
        )
        .await
        .expect("UploadCommit");
    let path = committed["path"].as_str().expect("path").to_string();
    assert!(path.ends_with("rpc-up-shot.png"));
    let chunk = client
        .call(
            methods::READ_ATTACHMENT_CHUNK,
            serde_json::json!({ "path": path, "offset": 0 }),
        )
        .await
        .expect("ReadAttachmentChunk");
    assert_eq!(chunk["mimeType"], "image/png");
    assert_eq!(chunk["done"], true);
    assert_eq!(
        BASE64
            .decode(chunk["data"].as_str().expect("data"))
            .expect("base64"),
        payload
    );
    // Jail holds over RPC too.
    assert!(
        client
            .call(
                methods::READ_ATTACHMENT_CHUNK,
                serde_json::json!({ "path": "/etc/passwd", "offset": 0 })
            )
            .await
            .is_err()
    );

    // Agent accounts: snapshot shape (this machine's real CLI state may or may
    // not include logins — assert the envelope, not the contents).
    let snapshot = client
        .call(methods::LIST_AGENT_ACCOUNTS, serde_json::json!({}))
        .await
        .expect("ListAgentAccounts");
    assert!(snapshot["accounts"].is_array());
    assert!(snapshot["warnings"].is_array());

    // Login lifecycle: start (paste-code) → poll pending → cancel → gone.
    let start = client
        .call(
            methods::START_AGENT_LOGIN,
            serde_json::json!({ "harness": "claude-code" }),
        )
        .await
        .expect("StartAgentLogin");
    assert_eq!(start["mode"], "paste-code");
    assert!(
        start["url"]
            .as_str()
            .expect("url")
            .contains("claude.ai/oauth/authorize")
    );
    let login_id = start["loginId"].as_str().expect("loginId").to_string();
    let poll = client
        .call(
            methods::POLL_AGENT_LOGIN,
            serde_json::json!({ "loginId": login_id }),
        )
        .await
        .expect("PollAgentLogin");
    assert_eq!(poll["status"], "pending");
    let cancelled = client
        .call(
            methods::CANCEL_AGENT_LOGIN,
            serde_json::json!({ "loginId": login_id }),
        )
        .await
        .expect("CancelAgentLogin");
    assert_eq!(cancelled["ok"], true);
    assert!(
        client
            .call(
                methods::POLL_AGENT_LOGIN,
                serde_json::json!({ "loginId": login_id })
            )
            .await
            .is_err(),
        "cancelled login is expired"
    );

    // Error paths: junk account ids and dead logins fail cleanly.
    assert!(
        client
            .call(
                methods::FORGET_AGENT_ACCOUNT,
                serde_json::json!({ "harness": "claude-code", "accountId": "../nope" })
            )
            .await
            .is_err()
    );
    assert!(
        client
            .call(
                methods::ACTIVATE_AGENT_ACCOUNT,
                serde_json::json!({ "harness": "claude-code", "accountId": "0123456789abcdef" })
            )
            .await
            .is_err(),
        "unknown slot cannot be activated"
    );
    assert!(
        client
            .call(
                methods::COMPLETE_AGENT_LOGIN,
                serde_json::json!({ "loginId": "no-such-login", "code": "x#y" })
            )
            .await
            .is_err()
    );
    core.shutdown().await;
}
