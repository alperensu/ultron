//! Kaynak sözleşmesi testi; servis başlatmaz ve gerçek kullanıcı verisi okumaz.

#[test]
fn every_registered_command_has_manifest_and_capability_permission() {
    let lib = include_str!("../src/lib.rs");
    let build = include_str!("../build.rs");
    let commands = include_str!("../src/commands.rs");
    let capabilities: serde_json::Value =
        serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
    let permissions = capabilities["permissions"].as_array().unwrap();
    let registered = lib.split("tauri::generate_handler![").nth(1).unwrap().split("])").next().unwrap();
    let mut count = 0;
    for command in registered.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let name = command.strip_prefix("commands::").unwrap();
        assert!(commands.contains(&format!("pub async fn {name}(")), "eksik komut: {name}");
        assert!(build.contains(&format!("\"{name}\"")), "manifest komutu eksik: {name}");
        let permission = format!("allow-{}", name.replace('_', "-"));
        assert!(permissions.iter().any(|p| p.as_str() == Some(permission.as_str())), "capability eksik: {name}");
        count += 1;
    }
    assert!(count >= 22);
    let ts = include_str!("../../src/lib/bridge.ts");
    let real_bridge = ts.split("export function createTauriBridge()").nth(1).unwrap().split("// --- Sahte köprü").next().unwrap();
    for call in real_bridge.split("call(\"").skip(1) {
        let name = call.split('"').next().unwrap();
        assert!(registered.contains(&format!("commands::{name},")), "TS komutu kayıtlı değil: {name}");
    }
}

#[test]
fn desktop_management_ipc_roundtrips_without_leaking_settings_in_debug() {
    use ultron_core::ipc::{PolicySettings, Request};
    let settings = PolicySettings { config: ultron_core::policy::PolicyConfig::default(), trash_retention_days: 30 };
    let requests = [Request::TrashList, Request::TrashRestore { id: "example".into() }, Request::TrashPurge { id: "example".into() }, Request::GetPolicyConfig, Request::SetPolicyConfig { settings }];
    for request in requests {
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(serde_json::from_value::<Request>(json).unwrap(), request);
    }
    let args: ultron_desktop::commands::PolicySettingsArgs = serde_json::from_value(serde_json::json!({
        "settings": { "config": {}, "trash_retention_days": 30 }
    })).unwrap();
    assert_eq!(args.settings.trash_retention_days, 30);
    let request = Request::SetPolicyConfig { settings: PolicySettings {
        config: ultron_core::policy::PolicyConfig { allow_list: vec!["synthetic-sensitive-value".into()], ..Default::default() }, trash_retention_days: 30,
    }};
    assert!(!format!("{request:?}").contains("synthetic-sensitive-value"));
}

#[test]
fn approval_arguments_cannot_select_approver_identity() {
    use ultron_desktop::commands::{ListApprovalsArgs, ResolveApprovalArgs};
    let args: ResolveApprovalArgs = serde_json::from_value(serde_json::json!({"id":"example", "approve":true})).unwrap();
    assert_eq!(args.id, "example");
    assert!(args.approve);
    assert!(serde_json::from_value::<ResolveApprovalArgs>(serde_json::json!({"id":"example", "approve":"true"})).is_err());
    let page: ListApprovalsArgs = serde_json::from_value(serde_json::json!({"status":"pending"})).unwrap();
    assert!(page.cursor.is_none());
    assert!(page.limit.is_none());
}
