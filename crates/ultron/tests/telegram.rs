use std::io::{Cursor, Write};
use std::process::{Command, Stdio};

use clap::Parser;
use ultron::commands::{Cli, Cmd, TelegramCmd, render_telegram_status, render_telegram_test, telegram_token_request};
use ultron_core::config::{NotifyMode, TelegramConfig};
use ultron_core::ipc::{Request, TelegramConnectionState, TelegramErrorCode, TelegramSettings, TelegramStatusSnapshot, TelegramTestReport};

#[test]
fn token_is_stdin_only_and_request_debug_is_redacted() {
    let cli = Cli::try_parse_from(["ultron", "telegram", "set-token"]).unwrap();
    assert!(matches!(cli.command, Cmd::Telegram { action: TelegramCmd::SetToken }));
    for args in [
        vec!["ultron", "telegram", "set-token", "101:synthetic"],
        vec!["ultron", "telegram", "set-token", "--token", "101:synthetic"],
        vec!["ultron", "telegram", "test", "--chat", "101"],
    ] { assert!(Cli::try_parse_from(args).is_err()); }
    for input in ["101:synthetic_secret", "101:synthetic_secret\n", "101:synthetic_secret\r\n"] {
        let request = telegram_token_request(&mut Cursor::new(input)).unwrap();
        assert!(!format!("{request:?}").contains("synthetic_secret"));
        match request {
            Request::TelegramSetToken { token } => assert_eq!(token, "101:synthetic_secret"),
            _ => panic!("yanlış istek"),
        }
    }
}

#[test]
fn stdin_rejects_empty_malformed_and_oversized_without_echo() {
    for input in [String::new(), "\n".into(), "synthetic-secret".into(), "101:secret with space".into(), "101:secret\r".into(), "101:secret\0".into(), "101:şifre".into(), format!("101:{}", "s".repeat(4096))] {
        let error = telegram_token_request(&mut Cursor::new(input.as_bytes())).unwrap_err();
        let diagnostic = format!("{error:?}\n{error:#}");
        assert!(!diagnostic.contains("synthetic-secret") && !diagnostic.contains("secret with space") && !diagnostic.contains("şifre"));
    }
}

#[test]
fn reader_stops_at_first_line_and_bounds_consumption() {
    let mut input = Cursor::new(b"101:synthetic\nsecond-line");
    telegram_token_request(&mut input).unwrap();
    assert_eq!(input.position(), 14);
    let mut input = Cursor::new(vec![b'x'; 100_000]);
    assert!(telegram_token_request(&mut input).is_err());
    assert_eq!(input.position(), 4097);
}

#[test]
fn invalid_stdin_exits_before_daemon_or_credentials_and_does_not_echo() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ultron"))
        .args(["telegram", "set-token"])
        .env("ULTRON_HOME", tmp.path())
        .env("ULTRON_PIPE_NAME", format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4()))
        .env("ULTRON_TELEGRAM_TOKEN", "101:synthetic_env_must_not_be_used")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"synthetic-invalid-secret\n").unwrap();
    let output = child.wait_with_output().unwrap();
    let all = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(!output.status.success());
    assert!(all.contains("token girdisi geçersiz"), "{all}");
    assert!(!all.contains("synthetic-invalid-secret") && !all.contains("synthetic_env_must_not_be_used"));
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
}

#[test]
fn status_and_partial_delivery_show_only_safe_summary() {
    let settings = TelegramSettings::from(&TelegramConfig { notify: NotifyMode::Away, ..Default::default() });
    for (state, expected) in [
        (TelegramConnectionState::Connected, "Bağlı"),
        (TelegramConnectionState::MissingToken, "Token yok"),
        (TelegramConnectionState::MissingAllowedUsers, "izinli kullanıcı yok"),
        (TelegramConnectionState::Error, "Hata"),
    ] {
        let status = TelegramStatusSnapshot { state, allowed_user_count: 2, settings: settings.clone(), error: Some(TelegramErrorCode::NotVerified) };
        let output = render_telegram_status(&status);
        assert!(output.contains(expected) && output.contains("izinli kullanıcı: 2") && output.contains("45 dk") && output.contains("uzaktayken"));
    }
    assert_eq!(render_telegram_test(&TelegramTestReport { attempted: 3, delivered: 1, failed: 1, uncertain: 1 }), "Telegram testi: 3 alıcı; 1 teslim, 1 başarısız, 1 belirsiz");
}
