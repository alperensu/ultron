use std::path::PathBuf;

use serde_json::json;
use ultron_core::Paths;
use ultron_core::policy::{
    Action, Decision, PolicyConfig, PolicyContext, RiskCategory, evaluate, normalize_shell,
};

fn context() -> PolicyContext {
    PolicyContext {
        project_root: Some(PathBuf::from("C:/workspace/project")),
        workspace_root: PathBuf::from("C:/workspace"),
    }
}

fn shell(command: &str, cfg: &PolicyConfig) -> Decision {
    evaluate(
        &Action::Shell {
            command: command.into(),
            cwd: PathBuf::from("C:/workspace/project"),
        },
        cfg,
        &context(),
    )
}

fn ask(command: &str, category: RiskCategory) {
    assert!(
        matches!(shell(command, &PolicyConfig::default()), Decision::Ask { category: actual, .. } if actual == category),
        "{command:?}: {:?}",
        shell(command, &PolicyConfig::default())
    );
}

#[test]
fn destructive_commands_ask() {
    for command in [
        "rm -rf target",
        "del file.txt",
        "erase file.txt",
        "rmdir /s cache",
        "rd cache",
        "Remove-Item -Recurse cache",
        "ri cache",
        "Clear-Content data.txt",
    ] {
        ask(command, RiskCategory::Delete);
    }
    for command in [
        "git push --force",
        "git push -f origin main",
        "git push --force-with-lease",
        "git reset --hard",
        "git rebase main",
        "git filter-repo --force",
        "git filter-branch main",
        "git clean -fd",
        "git branch -D old",
    ] {
        ask(command, RiskCategory::GitHistory);
    }
    for command in [
        "vercel",
        "wrangler deploy",
        "wrangler publish",
        "netlify deploy",
        "ssh host",
        "scp a host:b",
        "rsync a b",
        "systemctl restart app",
        "kubectl apply -f app.yaml",
        "kubectl delete pod app",
        "docker push image",
    ] {
        ask(command, RiskCategory::Deploy);
    }
    for command in [
        "winget install app",
        "winget uninstall app",
        "npm i -g tool",
        "npm install --global tool",
        "pip install tool",
        "cargo install tool",
        "choco install app",
    ] {
        ask(command, RiskCategory::Install);
    }
    for command in [
        "reg add HKCU\\Software\\Example",
        "reg delete HKCU\\Software\\Example",
        "schtasks /create /tn example",
        "schtasks /delete /tn example",
        "netsh show",
        "Set-ExecutionPolicy RemoteSigned",
    ] {
        ask(command, RiskCategory::SystemConfig);
    }
    for command in [
        "gh repo edit --visibility public",
        "gh release create v1",
        "npm publish",
        "cargo publish",
    ] {
        ask(command, RiskCategory::Publish);
    }
}

// Yalnız test verisi üretir; PowerShell çalıştırmaz.
fn encoded(script: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<_> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[(a >> 2) as usize] as char);
        output.push(ALPHABET[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

#[test]
fn obfuscated_destructive_commands_are_caught() {
    for command in [
        "cmd /c \"del x\"",
        "CMD.EXE /D /S /K \"ErAsE x\"",
        "powershell -c Remove-Item x",
        "pwsh -NoProfile -Command 'ri x'",
        "bash -c 'rm -rf x'",
        "sh -c \"rmdir x\"",
        "cmd /c powershell -Command \"ri x\"",
        "echo ok && del x",
        "echo ok || rm x",
        "ls; Remove-Item x",
        "echo ok | ri x",
        "r`m x",
        "d^el x",
        "'Remove-Item' 'two words'",
        "& 'Remove-Item' x",
        "C:\\Windows\\System32\\cmd.exe /c \"rd x\"",
        "unknown-wrapper rm -rf x",
        "custom-script --command=Remove-Item",
        "bash -c 'echo $(rm x)'",
    ] {
        assert!(
            matches!(
                shell(command, &PolicyConfig::default()),
                Decision::Ask { .. }
            ),
            "{command}"
        );
    }
    for command in [
        "git -C repo push -f",
        "GIT -c core.autocrlf=false --no-pager push --force",
        "git --git-dir repo/.git --work-tree repo branch -D old",
        "git -C repo -c key=value clean -xdf",
        "git --namespace=test reset --hard",
    ] {
        ask(command, RiskCategory::GitHistory);
    }
    for program in [
        "powershell -EncodedCommand",
        "pwsh -NoProfile -enc",
        "pwsh -e",
    ] {
        ask(
            &format!("{program} {}", encoded("Remove-Item 'öğe.txt'")),
            RiskCategory::Delete,
        );
        assert!(matches!(
            shell(
                &format!("{program} {}", encoded("diskpart")),
                &PolicyConfig::default()
            ),
            Decision::Deny { .. }
        ));
    }
}

#[test]
fn bypass_flags_are_denied_even_if_allowlisted() {
    for command in [
        "claude --dangerously-skip-permissions",
        "tool bypassPermissions",
        "codex --sandbox danger-full-access",
        "cmd /c \"tool bypassPermissions\"",
        "pwsh -c 'diskpart'",
        "format D:",
        "FORMAT.COM D:",
        "cipher /w:C:\\temp",
        "vssadmin delete shadows",
        "echo ok && format D:",
        "rm x; diskpart",
        "unknown diskpart",
    ] {
        let cfg = PolicyConfig {
            allow_list: vec![command.into()],
            ..Default::default()
        };
        assert!(
            matches!(shell(command, &cfg), Decision::Deny { .. }),
            "{command}"
        );
    }
    let action = Action::ToolCall {
        tool: "agent".into(),
        input: json!({"permissionMode": "bypassPermissions"}),
    };
    assert!(matches!(
        evaluate(&action, &PolicyConfig::default(), &context()),
        Decision::Deny { .. }
    ));
}

#[test]
fn finance_paths_require_approval() {
    let cfg = PolicyConfig {
        finance_paths: vec![PathBuf::from("C:/workspace/project/ledger")],
        protected_paths: vec![PathBuf::from("C:/workspace/project/settings")],
        ..Default::default()
    };
    for path in [
        "C:/workspace/project/ledger/entry.csv",
        "ledger/entry.csv",
        "LEDGER\\entry.csv",
        "settings/../ledger/entry.csv",
    ] {
        let action = Action::FileWrite { path: path.into() };
        assert!(
            matches!(
                evaluate(&action, &cfg, &context()),
                Decision::Ask {
                    category: RiskCategory::Finance,
                    ..
                }
            ),
            "{path}"
        );
    }
    assert!(matches!(
        shell("python update.py ledger/entry.csv", &cfg),
        Decision::Ask {
            category: RiskCategory::Finance,
            ..
        }
    ));
    assert!(matches!(
        evaluate(
            &Action::FileWrite {
                path: "settings/config.toml".into()
            },
            &cfg,
            &context()
        ),
        Decision::Ask {
            category: RiskCategory::SystemConfig,
            ..
        }
    ));
    assert_eq!(
        evaluate(
            &Action::FileWrite {
                path: "ledger-old/a.txt".into()
            },
            &cfg,
            &context()
        ),
        Decision::Allow
    );
    assert_eq!(
        evaluate(
            &Action::FileWrite {
                path: "ledger/../src/a.rs".into()
            },
            &cfg,
            &context()
        ),
        Decision::Allow
    );
}

#[test]
fn secret_files_require_approval() {
    for path in [
        ".env",
        ".env.local",
        "cert.key",
        "id_rsa",
        "id_rsa.pub",
        "credentials.toml",
        ".credentials.json",
        "nested/.env",
    ] {
        assert!(
            matches!(
                evaluate(
                    &Action::FileWrite { path: path.into() },
                    &PolicyConfig::default(),
                    &context()
                ),
                Decision::Ask {
                    category: RiskCategory::Secrets,
                    ..
                }
            ),
            "{path}"
        );
        ask(&format!("cat '{path}'"), RiskCategory::Secrets);
        let action = Action::ToolCall {
            tool: "Read".into(),
            input: json!({"file_path": path}),
        };
        assert!(matches!(
            evaluate(&action, &PolicyConfig::default(), &context()),
            Decision::Ask {
                category: RiskCategory::Secrets,
                ..
            }
        ));
    }
    ask("echo value > .env", RiskCategory::Secrets);
    ask(
        "Get-Content C:\\keys\\credentials.json",
        RiskCategory::Secrets,
    );
}

#[test]
fn protected_runtime_wildcards_expansions_and_directory_access_never_allow() {
    let run = Paths::resolve().unwrap().run;
    let cases = [
        ("Get-Content -Path approver-*".to_owned(), run.clone()),
        (r"type ..\run\appr*".to_owned(), run.join("../work")),
        // Yol sentetiktir; kullanıcı dizini okunmaz ve komut çalıştırılmaz.
        (
            r"cat \C:\Users\synthetic\AppData\Local\\ultron\\run\\*".to_owned(),
            "C:/workspace".into(),
        ),
        ("Get-ChildItem".to_owned(), run.join("nested")),
        (format!("ls '{}'", run.display()), "C:/workspace".into()),
        (format!("cat '{}/*'", run.display()), "C:/workspace".into()),
        ("cat approver-?oken".into(), "C:/workspace".into()),
        ("cat approver-[t]oken".into(), "C:/workspace".into()),
        (
            "cat %ULTRON_HOME%/run/approver-token".into(),
            "C:/workspace".into(),
        ),
        ("cat '$env:ULTRON_HOME/run/*'".into(), "C:/workspace".into()),
        ("cat ~/ultron/run".into(), "C:/workspace".into()),
    ];
    for (command, cwd) in cases {
        let cfg = PolicyConfig {
            allow_list: vec![command.clone()],
            ..Default::default()
        };
        for action in [
            Action::Shell {
                command: command.clone(),
                cwd: cwd.clone(),
            },
            Action::ToolCall {
                tool: "powershell".into(),
                input: json!({"command": command, "cwd": cwd}),
            },
        ] {
            assert!(
                matches!(
                    evaluate(&action, &cfg, &context()),
                    Decision::Ask { .. } | Decision::Deny { .. }
                ),
                "{action:?}"
            );
        }
    }
    for path in [run.clone(), run.join("*"), "../run/appr*".into()] {
        for action in [
            Action::FileWrite { path: path.clone() },
            Action::ToolCall {
                tool: "Read".into(),
                input: json!({"path": path}),
            },
        ] {
            assert!(matches!(
                evaluate(&action, &PolicyConfig::default(), &context()),
                Decision::Ask { .. }
            ));
        }
    }
}

#[test]
fn safe_commands_allow() {
    for command in [
        "git status",
        "git diff",
        "git show HEAD:README.md",
        "git show HEAD:src/lib.rs",
        "cargo test",
        "cargo test parser::tests",
        "cargo test -p ultron-core parser::tests",
        "npm test",
        "npm run lint",
        "ls",
        "Get-ChildItem",
        "cat README.md",
        "Get-Content ./docs/readme.txt",
        "Get-Content -LiteralPath:./README.md",
        "Get-Content -Path=./README.md",
        "Get-ChildItem ./src",
        "Copy-Item ./src/readme.txt ./copy.txt",
        "tar -cf archive.tar ./src",
        "git branch -d merged",
        "git clean -n",
        "git -C repo status",
        "npm install",
        ".venv/Scripts/pip.exe install tool",
        "venv/bin/python -m pip install tool",
        "pwsh -Command 'Get-ChildItem'",
        "cmd /c \"git status && cargo test\"",
    ] {
        assert_eq!(
            shell(command, &PolicyConfig::default()),
            Decision::Allow,
            "{command}"
        );
    }
    assert_eq!(
        shell(
            &format!("pwsh -EncodedCommand {}", encoded("Get-ChildItem")),
            &PolicyConfig::default()
        ),
        Decision::Allow
    );
    assert_eq!(
        evaluate(
            &Action::Network {
                url: "https://example.invalid/docs".into()
            },
            &PolicyConfig::default(),
            &context()
        ),
        Decision::Allow
    );
}

#[test]
fn directory_changes_make_subsequent_relative_access_unclassifiable() {
    for change in [
        "cd C:/sandbox/safe",
        "chdir C:/sandbox/safe",
        "Set-Location C:/sandbox/safe; Set-Location ..",
        "sl C:/sandbox/safe",
        "Push-Location C:/sandbox/safe",
        "pushd C:/sandbox/safe",
        "pushd C:/sandbox/safe; popd",
        "popd",
        "Pop-Location",
        "[Environment]::CurrentDirectory = 'C:/sandbox/safe'",
        "[IO.Directory]::SetCurrentDirectory('C:/sandbox/safe')",
        "Start-Process cmd -WorkingDirectory C:/sandbox/safe",
        "Start-Process cmd -WorkingDirectory:C:/sandbox/safe",
        "cmd /c \"cd C:/sandbox/safe\"",
        "Invoke-Expression 'cd C:/sandbox/safe'",
        "iex 'cd C:/sandbox/safe'",
        "& { Set-Location C:/sandbox/safe }",
        "{ cd C:/sandbox/safe }",
        "(cd C:/sandbox/safe)",
    ] {
        for access in [
            "Get-ChildItem ultron -Recurse -File | Get-Content",
            "Get-Content -LiteralPath:./README.md",
            "cat README.md",
            "Get-ChildItem",
            "Copy-Item ./README.md ./copy.txt",
            "tar -cf archive.tar ./src",
            "New-Item -ItemType HardLink -Path ./link -Target ./README.md",
        ] {
            let command = format!("{change}; {access}");
            for allow_list in [vec![], vec![command.clone()]] {
                let cfg = PolicyConfig {
                    allow_list,
                    ..Default::default()
                };
                for action in [
                    Action::Shell {
                        command: command.clone(),
                        cwd: "C:/workspace/project".into(),
                    },
                    Action::ToolCall {
                        tool: "powershell".into(),
                        input: json!({"command": command, "cwd": "C:/workspace/project"}),
                    },
                ] {
                    assert!(
                        matches!(evaluate(&action, &cfg, &context()), Decision::Ask { .. }),
                        "{action:?}"
                    );
                }
            }
        }
    }
    for command in [
        "cat ./README.md; cd C:/sandbox/safe",
        "cd C:/sandbox/safe; cat C:/workspace/project/README.md",
        "cmd /c \"cd C:/sandbox/safe && type README.md\"",
        "& { cd C:/sandbox/safe; Get-Content README.md }",
        "iex 'cd C:/sandbox/safe; cat README.md'",
    ] {
        let decision = shell(command, &PolicyConfig::default());
        if command.starts_with("cat ./") || command.ends_with("C:/workspace/project/README.md") {
            assert_eq!(decision, Decision::Allow, "{command}");
        } else {
            assert!(matches!(decision, Decision::Ask { .. }), "{command}");
        }
    }
}

#[test]
fn parsed_path_parameter_values_keep_sensitive_path_checks() {
    let cfg = PolicyConfig {
        finance_paths: vec!["ledger".into()],
        ..Default::default()
    };
    for command in [
        "Get-Content -LiteralPath:./ledger/entry.csv",
        "Get-Content -LiteralPath=./ledger/entry.csv",
    ] {
        assert!(
            matches!(
                shell(command, &cfg),
                Decision::Ask {
                    category: RiskCategory::Finance,
                    ..
                }
            ),
            "{command}"
        );
    }
    for command in [
        "Get-Content -LiteralPath:./.env",
        "cargo test --manifest-path ./.env",
        "npm test --prefix ./credentials",
        "git show -- HEAD:README.md",
        "git diff --no-index HEAD:README.md ./README.md",
        "git status HEAD:README.md",
    ] {
        assert!(
            matches!(
                shell(command, &PolicyConfig::default()),
                Decision::Ask { .. }
            ),
            "{command}"
        );
    }
}

#[test]
fn unclassifiable_paths_fail_closed_before_allow_list() {
    for path in [
        r"\sandbox\ultron",
        "C:sandbox\\ultron",
        "/sandbox/ultron",
        r"c:SANDBOX/ultron",
        r"/sandbox\ultron",
        r"\/server\share/ultron",
        r"\\server\share\ultron",
        r"\\?\UNC\server\share\ultron",
        r"\\.\pipe\synthetic",
        r"\??\C:\sandbox\ultron",
        r"\/?.c:/invalid",
        r"\\?\sandbox\ultron",
        "C:/SYNTH~1/ultron",
        "%SYNTHETIC_HOME%/ultron",
        "$env:SYNTHETIC_HOME/ultron",
        "~/ultron",
        "C:/sandbox/*",
        "C:/sandbox/[u]ltron",
        "C:/sandbox/file:stream",
        "C:/sandbox/nul",
    ] {
        for command in [
            format!("Get-ChildItem '{path}' -Recurse -File | Get-Content"),
            format!("Get-ChildItem -LiteralPath:'{path}'"),
            format!("cat '{path}'"),
            format!("Copy-Item '{path}' ./copy -Recurse"),
            format!("tar -cf archive.tar '{path}'"),
            format!("New-Item -ItemType Junction -Path ./link -Target '{path}'"),
        ] {
            let cfg = PolicyConfig {
                allow_list: vec![command.clone()],
                ..Default::default()
            };
            for action in [
                Action::Shell {
                    command: command.clone(),
                    cwd: "C:/workspace".into(),
                },
                Action::ToolCall {
                    tool: "powershell".into(),
                    input: json!({"command": command, "cwd": "C:/workspace"}),
                },
                Action::ToolCall {
                    tool: "custom".into(),
                    input: json!({"command": command}),
                },
                Action::ToolCall {
                    tool: "Read".into(),
                    input: json!({"path": path}),
                },
            ] {
                assert!(
                    matches!(
                        evaluate(&action, &cfg, &context()),
                        Decision::Ask { .. } | Decision::Deny { .. }
                    ),
                    "{action:?}"
                );
            }
        }
    }
    // Normalleştirilebilen aygıt önekleri ve karışık slashlar güvenli yolu bozmaz.
    for path in [
        "C:/workspace/project/readme.txt",
        r"\\?\C:\workspace\project\readme.txt",
        r"\\.\C:\workspace\project\readme.txt",
        r"/\?\C:/workspace\project/readme.txt",
        r"\/.\c:/WORKSPACE/project/readme.txt",
    ] {
        let command = format!("cat '{path}'");
        assert_eq!(
            shell(&command, &PolicyConfig::default()),
            Decision::Allow,
            "{command}"
        );
    }
}

#[test]
fn protected_runtime_ancestor_reads_never_allow_even_with_allow_list() {
    let paths = Paths::resolve().unwrap();
    for ancestor in paths.run.ancestors() {
        let path = ancestor.to_string_lossy().replace('\\', "/");
        for command in [
            format!("Get-ChildItem '{path}' -Recurse -File | Get-Content"),
            format!("Get-ChildItem -LiteralPath:'{path}' -Recurse | Get-Content"),
            format!("Get-Content -Path:'{path}'"),
            format!("cmd /c dir /s \"{path}\""),
            format!("ls -R '{path}'"),
            format!("findstr /s value '{path}'"),
            format!("rg value '{path}'"),
            format!("grep -r value '{path}'"),
            format!("Select-String -Recurse -Path '{path}' value"),
            format!("robocopy '{path}' C:/workspace/copy /s"),
            format!("xcopy '{path}' C:/workspace/copy /e"),
            format!("Copy-Item '{path}' C:/workspace/copy -Recurse"),
            format!("Compress-Archive -Path '{path}' -DestinationPath archive.zip"),
            format!("tar -cf archive.tar '{path}'"),
        ] {
            let cfg = PolicyConfig {
                allow_list: vec![command.clone()],
                ..Default::default()
            };
            for tool in ["powershell", "custom"] {
                let action = Action::ToolCall {
                    tool: tool.into(),
                    input: json!({"command": command}),
                };
                assert!(
                    matches!(evaluate(&action, &cfg, &context()), Decision::Ask { .. }),
                    "{action:?}"
                );
            }
            assert!(
                matches!(shell(&command, &cfg), Decision::Ask { .. }),
                "{command}"
            );
        }
        for action in [
            Action::Shell {
                command: "Get-ChildItem -Recurse -File | Get-Content".into(),
                cwd: ancestor.into(),
            },
            Action::ToolCall {
                tool: "Read".into(),
                input: json!({"path": ancestor, "recursive": true}),
            },
        ] {
            assert!(
                matches!(
                    evaluate(&action, &PolicyConfig::default(), &context()),
                    Decision::Ask { .. }
                ),
                "{action:?}"
            );
        }
    }
}

#[test]
fn link_creation_and_junction_read_chain_never_allow() {
    let root = Paths::resolve()
        .unwrap()
        .root
        .to_string_lossy()
        .replace('\\', "/");
    for command in [
        format!(
            "New-Item -ItemType Junction -Path C:/workspace/u -Target '{root}'; Get-ChildItem C:/workspace/u/run -File | Get-Content"
        ),
        format!("New-Item -ItemType Junction -Path C:/workspace/u -Target '{root}'"),
        "New-Item -ItemType SymbolicLink -Path C:/workspace/u -Target C:/workspace/safe".into(),
        "ni -Type HardLink -Path C:/workspace/u -Target C:/workspace/safe".into(),
        format!("mklink /J C:/workspace/u \"{root}\""),
        format!("cmd /c mklink /J C:/workspace/u \"{root}\""),
        "fsutil hardlink create C:/workspace/u C:/workspace/safe".into(),
        "ln -s safe u".into(),
    ] {
        let cfg = PolicyConfig {
            allow_list: vec![command.clone()],
            ..Default::default()
        };
        assert!(
            matches!(shell(&command, &cfg), Decision::Ask { .. }),
            "{command}"
        );
        let action = Action::ToolCall {
            tool: "custom".into(),
            input: json!({"command": command}),
        };
        assert!(
            matches!(evaluate(&action, &cfg, &context()), Decision::Ask { .. }),
            "{action:?}"
        );
    }
    for action in [
        Action::ToolCall {
            tool: "create_symlink".into(),
            input: json!({"target": "safe", "path": "u"}),
        },
        Action::ToolCall {
            tool: "New-Item".into(),
            input: json!({"ItemType": "Junction", "target": "safe", "path": "u"}),
        },
    ] {
        assert!(matches!(
            evaluate(&action, &PolicyConfig::default(), &context()),
            Decision::Ask { .. }
        ));
    }
}

#[test]
fn runtime_ancestor_case_slashes_device_prefixes_and_short_names_require_approval() {
    let root = Paths::resolve()
        .unwrap()
        .root
        .to_string_lossy()
        .replace('\\', "/");
    for path in [
        root.to_uppercase(),
        format!("{root}///"),
        format!(r"\\?\{}", root.replace('/', "\\")),
        format!(r"\\.\{}", root.replace('/', "\\")),
        format!("//?/{root}"),
        format!("//./{root}"),
        "C:/Users/SYNTHE~1/AppData/Local/ULTRON~1".into(),
    ] {
        let command = format!("Get-ChildItem '{path}' -Recurse | Get-Content");
        let cfg = PolicyConfig {
            allow_list: vec![command.clone()],
            ..Default::default()
        };
        assert!(
            matches!(shell(&command, &cfg), Decision::Ask { .. }),
            "{command}"
        );
        let action = Action::ToolCall {
            tool: "Read".into(),
            input: json!({"path": path}),
        };
        assert!(
            matches!(evaluate(&action, &cfg, &context()), Decision::Ask { .. }),
            "{action:?}"
        );
    }
}

#[test]
fn ultron_trash_is_allowed() {
    assert_eq!(
        shell("ultron trash target.txt", &PolicyConfig::default()),
        Decision::Allow
    );
    assert_eq!(
        shell("ultron.exe trash target.txt", &PolicyConfig::default()),
        Decision::Allow
    );
    assert_eq!(
        shell(
            "cmd /c \"ultron trash target.txt\"",
            &PolicyConfig::default()
        ),
        Decision::Allow
    );
    assert!(matches!(
        shell("ultron trash x && rm y", &PolicyConfig::default()),
        Decision::Ask { .. }
    ));
    let cfg = PolicyConfig {
        allow_list: vec!["x.txt".into()],
        ..Default::default()
    };
    assert!(matches!(
        evaluate(
            &Action::FileDelete {
                path: "x.txt".into()
            },
            &cfg,
            &context()
        ),
        Decision::Ask {
            category: RiskCategory::Delete,
            ..
        }
    ));
}

#[test]
fn normalize_shell_unwraps_and_preserves_argument_boundaries() {
    assert_eq!(
        normalize_shell("CMD /K \"echo 'Two Words' && PWSh -c 'r`m x'\""),
        vec![vec!["echo", "two words"], vec!["rm", "x"]]
    );
    assert_eq!(
        normalize_shell("echo 'a;b|c&&d' || ls; pwd | cat"),
        vec![
            vec!["echo", "a;b|c&&d"],
            vec!["ls"],
            vec!["pwd"],
            vec!["cat"]
        ]
    );
    assert_eq!(
        normalize_shell("pwsh -EncodedCommand RwBlAHQALQBDAGgAaQBsAGQASQB0AGUAbQA="),
        vec![vec!["get-childitem"]]
    );
}

#[test]
fn malformed_or_unsupported_risky_commands_ask() {
    for command in [
        "rm 'unterminated",
        "cmd /c",
        "pwsh -EncodedCommand !!!",
        "pwsh -EncodedCommand YQ==",
        "pwsh -EncodedCommand AAA",
        "pwsh -File cleanup.ps1 rm",
        "git --unknown-option x reset --hard",
        "unknown 'rm -rf x'",
        "unknown r`m",
        "echo $(Remove-Item x)",
    ] {
        assert!(
            matches!(
                shell(command, &PolicyConfig::default()),
                Decision::Ask { .. }
            ),
            "{command}"
        );
    }
    let nested = format!("{}rm x", "cmd /c ".repeat(100));
    assert!(matches!(
        shell(&nested, &PolicyConfig::default()),
        Decision::Ask { .. }
    ));
}

#[test]
fn policy_config_loads_defaults_and_toml() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let paths = Paths::from_root(temp.path().to_path_buf());
    assert_eq!(PolicyConfig::load(&paths)?, PolicyConfig::default());
    std::fs::write(
        temp.path().join("policy.toml"),
        "finance_paths = ['ledger']\nprotected_paths = ['settings']\ndeploy_patterns = ['custom-deploy']\nallow_list = ['ssh example.invalid']\n",
    )?;
    let cfg = PolicyConfig::load(&paths)?;
    assert_eq!(cfg.finance_paths, vec![PathBuf::from("ledger")]);
    assert!(matches!(
        shell("custom-deploy", &cfg),
        Decision::Ask {
            category: RiskCategory::Deploy,
            ..
        }
    ));
    assert_eq!(shell("ssh example.invalid", &cfg), Decision::Allow);
    assert!(matches!(shell("rm x", &cfg), Decision::Ask { .. }));
    std::fs::write(temp.path().join("policy.toml"), "finance_paths = [")?;
    assert!(PolicyConfig::load(&paths).is_err());
    Ok(())
}

#[test]
fn policy_public_types_roundtrip() -> anyhow::Result<()> {
    let action = Action::ToolCall {
        tool: "Bash".into(),
        input: json!({"command": "rm x"}),
    };
    assert_eq!(
        serde_json::from_str::<Action>(&serde_json::to_string(&action)?)?,
        action
    );
    let decision = shell("rm x", &PolicyConfig::default());
    assert_eq!(
        serde_json::from_str::<Decision>(&serde_json::to_string(&decision)?)?,
        decision
    );
    Ok(())
}

#[test]
fn quoted_finance_paths_and_nested_tool_denials() {
    let cfg = PolicyConfig {
        finance_paths: vec![PathBuf::from("C:/workspace/project/finance data")],
        ..Default::default()
    };
    assert!(matches!(
        shell("python update.py 'finance data/ledger.csv'", &cfg),
        Decision::Ask {
            category: RiskCategory::Finance,
            ..
        }
    ));
    for input in [
        json!({"command": "rm x; diskpart"}),
        json!({"nested": [{"command": format!("pwsh -enc {}", encoded("diskpart"))}]}),
    ] {
        let action = Action::ToolCall {
            tool: "custom".into(),
            input,
        };
        assert!(matches!(
            evaluate(&action, &cfg, &context()),
            Decision::Deny { .. }
        ));
    }
}

#[test]
fn unknown_wrappers_preserve_risky_subcommand_arguments() {
    for command in [
        "unknown-wrapper git -C repo push -f",
        "unknown-wrapper git reset --hard",
        "unknown-wrapper pip install tool",
        "unknown-wrapper npm i -g tool",
        "unknown-wrapper kubectl delete pod example",
    ] {
        assert!(
            matches!(
                shell(command, &PolicyConfig::default()),
                Decision::Ask { .. }
            ),
            "{command}"
        );
    }
}

#[test]
fn ultron_admin_commands_from_engine_shell_always_ask() {
    for command in [
        "ultron import remove-chats",
        "ultron import projects --dry-run",
        "ultron.exe import projects",
        r"C:\tools\ultron.exe service install",
        "cmd /c ultron import remove-chats",
        "echo x && ultron import projects",
        "ultron",
    ] {
        ask(command, RiskCategory::SystemConfig);
    }
    // Salt okunur / geri alınabilir alt komutlar serbest kalır.
    for command in ["ultron policy check", "ultron history search \"x\"", "ultron status"] {
        assert_eq!(shell(command, &PolicyConfig::default()), Decision::Allow, "{command}");
    }
}
