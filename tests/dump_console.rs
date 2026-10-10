//! Renders console pages to `design/current/` for design work.
//!
//! Ignored by default: this writes files and is a tool rather than a check. Run it with
//! `cargo test --test dump_console -- --ignored --nocapture` when the markup has changed and the
//! design reference needs to catch up.

use chrono::Utc;
use lumberroom_server::console::clients;
use lumberroom_server::console::pages::Health;
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::types::Sensitivity;
use lumberroom_server::ports::oauth::OauthClientRecord;

fn health() -> Health {
    Health {
        key_verified: true,
        keys_configured: true,
        embedder: "Xenova/bge-base-en-v1.5".into(),
        last_write: Some(Utc::now() - chrono::Duration::minutes(7)),
        now: Utc::now(),
    }
}

fn client(name: &str, via: &str, consented: bool, revoked: bool) -> OauthClientRecord {
    OauthClientRecord {
        client_id: format!("{}-4f2a9c1e7b3d8a05", name),
        secret_hash: None,
        client_name: name.into(),
        redirect_uris: vec!["https://claude.ai/api/mcp/auth_callback".into()],
        grant_types: vec!["authorization_code".into(), "refresh_token".into()],
        registered_via: via.into(),
        software_id: None,
        read: NamespaceGrant::everything(),
        write: vec![NamespaceGrant::new("*", Sensitivity::Open)],
        registry_write: false,
        sealed_capable: false,
        may_delete: false,
        may_ingest: name.contains("cleanup"),
        may_read_history: false,
        consented_at: consented.then(Utc::now),
        profile: Some("read-write".into()),
        created_at: Utc::now() - chrono::Duration::days(3),
        last_used_at: Some(Utc::now() - chrono::Duration::hours(2)),
        revoked_at: revoked.then(Utc::now),
        owner_label: None,
    }
}

#[test]
#[ignore = "writes files; a tool rather than a check"]
fn dump_clients_page() {
    let clients_list = vec![
        client("claude-code-mac", "manual", true, false),
        client("claude-desktop", "dcr", true, false),
        client("cleanup", "manual", true, false),
        client("chatgpt", "dcr", false, false),
        client("an-old-laptop", "manual", true, true),
    ];
    let token = |_: &str, _: &str| "csrf-token-placeholder".to_string();

    let namespaces: Vec<String> = ["global", "user:me", "project:lumberroom", "project:sivella"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let health = health();
    let view = |issued, error| clients::View {
        clients: &clients_list,
        namespaces: &namespaces,
        health: &health,
        issued,
        error,
        done: None,
    };

    let listing = clients::html(view(None, None), &token);
    std::fs::write("design/current/clients.html", &listing).unwrap();
    println!("wrote design/current/clients.html ({} bytes)", listing.len());

    let issued = clients::Issued {
        client_id: "9f3c1a7e4b2d8056".into(),
        secret: Some("lumberroom_s_4a91c7e3b58d02f6a1c94e7b3d8056fa".into()),
        name: "claude-desktop".into(),
    };
    let after = clients::html(view(Some(&issued), None), &token);
    std::fs::write("design/current/clients-issued.html", &after).unwrap();
    println!("wrote design/current/clients-issued.html ({} bytes)", after.len());

    let empty = clients::html(
        clients::View {
            clients: &[],
            namespaces: &namespaces,
            health: &health,
            issued: None,
            error: Some("that is not one of the shapes on this form"),
            done: None,
        },
        &token,
    );
    std::fs::write("design/current/clients-empty.html", &empty).unwrap();
    println!("wrote design/current/clients-empty.html ({} bytes)", empty.len());
}
