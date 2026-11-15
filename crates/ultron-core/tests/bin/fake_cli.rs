//! Testler için sahte motor CLI'ı. Davranış ilk argümanla seçilir:
//! `replay <fixture>`, `echo <args...>`, `partial <n> <exit>`, `sleep <pidfile>`, `linger`, `hold`,
//! `longline`, `flood`.

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
        "linger" => {
            // Stdout'u miras alan uzun ömürlü bir torun: çocuk çıkar, boru açık kalır.
            let exe = std::env::current_exe().unwrap();
            let _grandchild = Command::new(exe)
                .arg("hold")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("alt süreç başlatılamadı");
            writeln!(out, "{{\"type\":\"assistant_text\",\"text\":\"bitti\"}}").unwrap();
            out.flush().unwrap();
            std::process::exit(0);
        }
        "longline" => {
            // 2 MiB'lık tek satır (atlanmalı), ardından normal bir olay.
            out.write_all(&vec![b'x'; 2 * 1024 * 1024]).unwrap();
            writeln!(out).unwrap();
            writeln!(out, "{{\"type\":\"assistant_text\",\"text\":\"tamam\"}}").unwrap();
            out.flush().unwrap();
        }
        "flood" => {
            // Bütçeyi aşacak kadar çıktı; ana süreç bizi sonlandırmalı.
            let line = format!("{}\n", "y".repeat(8 * 1024));
            for _ in 0..(72 * 1024 * 1024 / line.len()) {
                if out.write_all(line.as_bytes()).is_err() {
                    break;
                }
            }
            let _ = out.flush();
        }
        "tailjson" => {
            // Bir tam satır, ardından sonlandırıcısız geçerli bir JSON; çıkış kodu argümandan.
            let code: i32 = args[1].parse().unwrap();
            writeln!(out, "{{\"type\":\"assistant_text\",\"text\":\"satır 0\"}}").unwrap();
            write!(out, "{{\"type\":\"assistant_text\",\"text\":\"son\"}}").unwrap();
            out.flush().unwrap();
            std::process::exit(code);
        }
        "hold" => std::thread::sleep(Duration::from_secs(120)),
        other => {
            eprintln!("bilinmeyen kip: {other}");
            std::process::exit(2);
        }
    }
}
