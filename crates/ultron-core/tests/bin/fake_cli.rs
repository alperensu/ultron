//! Testler için sahte motor CLI'ı. Davranış ilk argümanla seçilir:
//! `replay <fixture>`, `echo <args...>`, `partial <n> <exit>`, `sleep <pidfile>`, `hold`.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("");
    let mut out = std::io::stdout();
    match mode {
        "replay" => {
            let text = std::fs::read_to_string(&args[1]).expect("fixture okunamadı");
            for line in text.lines() {
                writeln!(out, "{line}").unwrap();
            }
            out.flush().unwrap();
        }
        "echo" => {
            let cwd = std::env::current_dir().unwrap().to_string_lossy().into_owned();
            let payload = serde_json::json!({ "args": &args[1..], "cwd": cwd });
            let ev = serde_json::json!({ "type": "assistant_text", "text": payload.to_string() });
            writeln!(out, "{ev}").unwrap();
            out.flush().unwrap();
        }
        "partial" => {
            let n: usize = args[1].parse().unwrap();
            let code: i32 = args[2].parse().unwrap();
            for i in 0..n {
                writeln!(out, "{{\"type\":\"assistant_text\",\"text\":\"satır {i}\"}}").unwrap();
            }
            write!(out, "{{\"type\":\"assistant_te").unwrap();
            out.flush().unwrap();
            std::process::exit(code);
        }
        "sleep" => {
            let exe = std::env::current_exe().unwrap();
            let mut grandchild = Command::new(exe)
                .arg("hold")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("alt süreç başlatılamadı");
            std::fs::write(&args[1], format!("{}\n{}\n", std::process::id(), grandchild.id())).unwrap();
            writeln!(out, "{{\"type\":\"assistant_text\",\"text\":\"uyuyor\"}}").unwrap();
            out.flush().unwrap();
            std::thread::sleep(Duration::from_secs(120));
            let _ = grandchild.wait();
        }
        "hold" => std::thread::sleep(Duration::from_secs(120)),
        other => {
            eprintln!("bilinmeyen kip: {other}");
            std::process::exit(2);
        }
    }
}
