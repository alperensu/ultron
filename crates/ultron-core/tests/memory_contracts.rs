//! Aşama 5a Task 1: hafıza sözleşmeleri (sentetik veriyle).

use chrono::{DateTime, Utc};
use serde_json::json;
use ultron_core::EngineEvent;
use ultron_core::ipc::{MAX_LINE_BYTES, Request};
use ultron_core::memory::*;

fn ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn hash(tag: &str) -> Sha256Hash {
    Sha256Hash::of(tag.as_bytes())
}

fn sample_yaml() -> String {
    format!(
        "schema_version: 1\n\
         id: \"11111111-1111-4111-8111-111111111111\"\n\
         type: user\n\
         title: \"Sentetik yakalama\"\n\
         created_at: \"2026-11-18T09:00:00Z\"\n\
         updated_at: \"2026-11-18T09:00:00Z\"\n\
         revision: 1\n\
         summary: \"Örnek tercih notu.\"\n\
         project: null\n\
         visibility: private\n\
         remote_allowed: false\n\
         sensitivity: personal\n\
         review_at: null\n\
         aliases: []\n\
         facts: {{}}\n\
         tags: [ornek]\n\
         receipts: [\"{}\"]\n",
        hash("makbuz").prefixed()
    )
}

fn sample() -> FrontMatter {
    parse_front_matter(&sample_yaml()).unwrap()
}

fn replace_line(yaml: &str, key: &str, new_line: &str) -> String {
    yaml.lines()
        .map(|l| if l.starts_with(&format!("{key}:")) { new_line.to_string() } else { l.to_string() })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

// ---------------------------------------------------------------------------------------------
// Ön bilgi
// ---------------------------------------------------------------------------------------------

#[test]
fn plan_example_front_matter_parses_with_safe_defaults() {
    let front = sample();
    assert_eq!(front.memory_type, MemoryType::User);
    assert_eq!(front.visibility, Visibility::Private);
    assert!(!front.remote_allowed);
    assert_eq!(front.sensitivity, Sensitivity::Personal);
    assert_eq!(front.revision, Revision::FIRST);
    assert_eq!(front.title, "Sentetik yakalama");
    assert_eq!(front.receipts, vec![hash("makbuz")]);

    // Yalnız zorunlu alanlar: en kısıtlayıcı varsayılanlar uygulanır.
    let minimal = "schema_version: 1\nid: \"22222222-2222-4222-8222-222222222222\"\ntype: tool\ntitle: Minimal\n\
                   created_at: \"2026-11-18T09:00:00Z\"\nupdated_at: \"2026-11-18T09:00:00Z\"\nrevision: 3\n";
    let front = parse_front_matter(minimal).unwrap();
    assert_eq!(front.visibility, Visibility::Private);
    assert!(!front.remote_allowed);
    assert_eq!(front.sensitivity, Sensitivity::Personal);
    assert!(front.receipts.is_empty());
}

#[test]
fn note_roundtrip_is_lossless_including_free_keys_and_body() {
    let mut front = sample();
    front.title = "Başlık: \"tırnaklı\" & * ! # ğüşiöç".into();
    front.summary = "Satır bir\nSatır iki".into();
    front.tags = vec!["türkçe".into(), "a:b".into(), "*yıldız".into()];
    front.aliases = vec!["takma ad".into()];
    front.facts.insert("sayı".into(), json!(42));
    front.facts.insert("iç".into(), json!({"liste": [1, 2.5, null, true, "x y"], "boş": {}}));
    front.extra.insert("obsidian_ozel".into(), json!({"a": [1, 2], "b": "c"}));
    front.extra.insert("null".into(), json!("ayrılmış anahtar"));
    front.extra.insert("a b".into(), json!(false));
    front.project = Some("proje-x".into());
    front.review_at = Some(ts("2026-12-01T00:00:00Z"));

    let body = "Gövde ilk satır\r\n---\r\nikinci bölüm: değil ön bilgi\n\n  girinti\u{00a0}ve ğ\n";
    let text = render_note(&front, body).unwrap();
    let (parsed, parsed_body) = parse_note(&text).unwrap();
    assert_eq!(parsed, front);
    assert_eq!(parsed_body, body);
    // İkinci tur aynı metni üretir.
    assert_eq!(render_note(&parsed, &parsed_body).unwrap(), text);
}

#[test]
fn render_escapes_characters_yaml_readers_reject() {
    let mut front = sample();
    front.facts.insert("özel".into(), json!("a\u{2028}b\u{0085}c\u{007f}d\u{feff}e"));
    let text = render_note(&front, "g").unwrap();
    assert!(!text.contains('\u{2028}') && !text.contains('\u{007f}'));
    let (parsed, _) = parse_note(&text).unwrap();
    assert_eq!(parsed.facts["özel"], json!("a\u{2028}b\u{0085}c\u{007f}d\u{feff}e"));
}

#[test]
fn block_style_yaml_from_external_editors_is_accepted() {
    let yaml = "schema_version: 1\nid: \"33333333-3333-4333-8333-333333333333\"\ntype: project\ntitle: Dış düzenleme\n\
        created_at: 2026-11-18T09:00:00Z\nupdated_at: 2026-11-18T09:00:00Z\nrevision: 2\ntags:\n  - a\n  - b\n\
        facts:\n  anahtar: değer\n  iç:\n    x: 1\ncustom: 'tek tırnak ''kaçışlı'''\n";
    let front = parse_front_matter(yaml).unwrap();
    assert_eq!(front.tags, vec!["a", "b"]);
    assert_eq!(front.facts["iç"], json!({"x": 1}));
    assert_eq!(front.extra["custom"], json!("tek tırnak 'kaçışlı'"));
}

#[test]
fn rejects_bad_type_revision_hash_id_and_schema() {
    let base = sample_yaml();
    for (key, line) in [
        ("type", "type: banana"),
        ("revision", "revision: 0"),
        ("revision", "revision: -1"),
        ("revision", "revision: 1.5"),
        ("revision", "revision: \"bir\""),
        ("id", "id: \"11111111-1111-4111-8111-11111111111\""),
        ("id", "id: \"11111111-1111-4111-8111-11111111111G\""),
        ("id", "id: \"11111111-1111-4111-8111-111111111111-x\""),
        ("title", "title: \"\""),
        ("visibility", "visibility: public"),
        ("sensitivity", "sensitivity: secret"),
        ("remote_allowed", "remote_allowed: \"evet\""),
        ("created_at", "created_at: \"dün\""),
        ("updated_at", "updated_at: \"2026-11-17T09:00:00Z\""),
    ] {
        let yaml = replace_line(&base, key, line);
        assert!(parse_front_matter(&yaml).is_err(), "kabul edilmemeliydi: {line}");
    }
    // Büyük harfli UUID kanonik değildir.
    let upper = replace_line(&base, "id", "id: \"11111111-1111-4111-8111-11111111111A\"");
    assert_eq!(parse_front_matter(&upper).unwrap_err(), ContractError::InvalidField);
    for bad_hash in [
        "sha256:ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        "sha256:abc",
        "sha1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ] {
        let yaml = replace_line(&base, "receipts", &format!("receipts: [\"{bad_hash}\"]"));
        assert!(parse_front_matter(&yaml).is_err(), "{bad_hash}");
    }
}

#[test]
fn rejects_unknown_or_missing_schema_version() {
    let base = sample_yaml();
    for line in ["schema_version: 2", "schema_version: 0", "schema_version: \"1\"", "schema_version: 1.5"] {
        let yaml = replace_line(&base, "schema_version", line);
        assert_eq!(parse_front_matter(&yaml).unwrap_err(), ContractError::UnknownSchemaVersion, "{line}");
    }
    let missing: String = base.lines().filter(|l| !l.starts_with("schema_version")).collect::<Vec<_>>().join("\n");
    assert_eq!(parse_front_matter(&missing).unwrap_err(), ContractError::UnknownSchemaVersion);
}

#[test]
fn rejects_oversized_and_too_deep_front_matter() {
    let base = sample_yaml();
    // Boyut sınırı.
    let big = format!("{base}note: \"{}\"\n", "a".repeat(MAX_FRONT_MATTER_BYTES));
    assert_eq!(parse_front_matter(&big).unwrap_err(), ContractError::TooLarge);
    // Tek skaler sınırı.
    let long_scalar = format!("{base}note: \"{}\"\n", "a".repeat(MAX_FRONT_MATTER_SCALAR_BYTES + 1));
    assert_eq!(parse_front_matter(&long_scalar).unwrap_err(), ContractError::TooLarge);
    // Düğüm sayısı sınırı.
    let many = format!("{base}extra_list: [{}]\n", vec!["1"; MAX_FRONT_MATTER_NODES + 1].join(","));
    assert!(parse_front_matter(&many).is_err());
    // Akış iç içeliği: ayrıştırıcıya girmeden reddedilir (yığın taşması/alias bombası yok).
    let deep = format!("{base}d: {}{}\n", "[".repeat(5000), "]".repeat(5000));
    assert_eq!(parse_front_matter(&deep).unwrap_err(), ContractError::TooDeep);
    // Orta derinlik: tarayıcıdan geçer, değer derinliği sınırında reddedilir.
    let nested = format!("{base}d: {}1{}\n", "[".repeat(9), "]".repeat(9));
    assert_eq!(parse_front_matter(&nested).unwrap_err(), ContractError::TooDeep);
    // Blok biçimli derin harita.
    let mut block = base.clone();
    for level in 0..8 {
        block.push_str(&format!("{}k{level}:\n", "  ".repeat(level)));
    }
    block.push_str(&format!("{}son: 1\n", "  ".repeat(8)));
    assert!(parse_front_matter(&block).is_err());
}

#[test]
fn rejects_yaml_aliases_anchors_and_tags_before_parsing() {
    let base = sample_yaml();
    let bomb = "a: &a [x,x,x,x,x,x,x,x,x]\nb: &b [*a,*a,*a,*a,*a,*a,*a,*a,*a]\nc: &c [*b,*b,*b,*b,*b,*b,*b,*b,*b]\nd: &d [*c,*c,*c,*c,*c,*c,*c,*c,*c]\ne: &e [*d,*d,*d,*d,*d,*d,*d,*d,*d]\n";
    assert_eq!(parse_front_matter(&format!("{base}{bomb}")).unwrap_err(), ContractError::YamlAlias);
    for (extra, expected) in [
        ("facts2: &x 1\n", ContractError::YamlAlias),
        ("facts2: *x\n", ContractError::YamlAlias),
        ("facts2: [*x]\n", ContractError::YamlAlias),
        ("facts2: {k: *x}\n", ContractError::YamlAlias),
        ("facts2: {\"k\":*x}\n", ContractError::YamlAlias),
        ("facts2:\n  - &x 1\n  - *x\n", ContractError::YamlAlias),
        ("<<: *x\n", ContractError::YamlAlias),
        ("facts2: !!str 1\n", ContractError::YamlTag),
        ("facts2: !tag value\n", ContractError::YamlTag),
        ("facts2: !<tag:yaml.org,2002:str> x\n", ContractError::YamlTag),
        ("facts2: |\n  blok\n", ContractError::YamlUnsupported),
        ("facts2: >\n  blok\n", ContractError::YamlUnsupported),
        // Düz skaler `abc 'def` olarak sürer; sonraki satırdaki gerçek çapa olay düzeyinde yakalanır.
        ("k: abc\n  'def\nz: &a [1]\nw: y'\n", ContractError::YamlAlias),
        // Sahte tırnak aynı satırda düğüm başı olmadan açılamaz; gerçek alias yakalanır.
        ("t: it 'x\nz: &a [1]\nw: y'\n", ContractError::YamlAlias),
    ] {
        let err = parse_front_matter(&format!("{base}{extra}")).unwrap_err();
        assert_eq!(err, expected, "{extra:?}");
    }
    // Satıra yayılan çift tırnaklı skaler tek bir metindir; içindeki `&a` çapa değildir.
    let multi = parse_front_matter(&format!("{base}k: \"açık\nz: &a [1]\"\n"));
    if let Ok(front) = multi {
        assert_eq!(front.extra["k"], json!("açık z: &a [1]"));
    }
    // Tırnak içindeki ve düğüm başında olmayan işaretler meşru metindir.
    let ok = format!("{base}ok1: \"a &b *c !d\"\nok2: 'x *y'\nok3: Fiyat & vergi! %5 #değil\nok4: a*b\n");
    let front = parse_front_matter(&ok).unwrap();
    assert_eq!(front.extra["ok1"], json!("a &b *c !d"));
    assert_eq!(front.extra["ok3"], json!("Fiyat & vergi! %5"));
    assert_eq!(front.extra["ok4"], json!("a*b"));
}

#[test]
fn rejects_duplicate_keys_non_object_and_control_characters() {
    let base = sample_yaml();
    assert!(parse_front_matter(&format!("{base}title: ikinci\n")).is_err());
    assert!(parse_front_matter("- a\n- b\n").is_err());
    assert!(parse_front_matter("").is_err());
    assert_eq!(parse_front_matter(&format!("{base}x: \"a\u{0007}b\"\n")).unwrap_err(), ContractError::InvalidText);
}

#[test]
fn note_splitting_requires_delimiters() {
    assert!(parse_note("başlık yok\n").is_err());
    assert!(parse_note("---\nschema_version: 1\n").is_err());
    let front = sample();
    let text = render_note(&front, "").unwrap();
    assert!(parse_note(&format!("\u{feff}{text}")).is_ok());
    assert!(parse_note(&text.replace('\n', "\r\n")).is_ok());
    let big_body = "x".repeat(MAX_BODY_BYTES + 1);
    assert_eq!(render_note(&front, &big_body).unwrap_err(), ContractError::TooLarge);
    assert_eq!(render_note(&front, "a\u{1b}[31m").unwrap_err(), ContractError::InvalidText);
}

// ---------------------------------------------------------------------------------------------
// Kaynak, makbuz, yol
// ---------------------------------------------------------------------------------------------

#[test]
fn locator_validation_rejects_traversal_and_platform_escapes() {
    for ok in ["receipts/sources/abc.md", "inbox/not.md", "projects/ğüşiöç/not.md", "a.b/c"] {
        assert!(validate_locator(ok).is_ok(), "{ok}");
    }
    let long = "a/".repeat(200);
    for bad in [
        "", "..", "../x", "a/../b", "a/..", "./a", "a/./b", "/abs", "a//b", "a/", "C:/x", "C:\\x", "c:x",
        "\\\\srv\\share\\x", "//srv/share", "a\\b", "a:stream", "con", "CON.txt", "a/nul", "a/COM1.md", "lpt9",
        "x.", "x /y", "a\u{0}b", "a\nb", "a*b", "a?b", "a|b", "a<b", "\"a\"", &long,
    ] {
        assert!(validate_locator(bad).is_err(), "kabul edilmemeliydi: {bad:?}");
    }
}

#[test]
fn source_ref_roundtrip_and_rejections() {
    let sources = [
        SourceRef::Event { chat_id: "sohbet-1".into(), seq_from: 3, seq_to: 9 },
        SourceRef::File { locator: "receipts/sources/a.md".into(), content_hash: hash("dosya") },
        SourceRef::Import { importer: "claude_history".into(), source_hash: hash("kaynak"), record_key: "kayıt/1".into() },
    ];
    for source in sources {
        let wire = serde_json::to_value(&source).unwrap();
        assert_eq!(serde_json::from_value::<SourceRef>(wire).unwrap(), source);
    }
    let h = hash("x").prefixed();
    for bad in [
        json!({"kind": "event", "chat_id": "c", "seq_from": 0, "seq_to": 1}),
        json!({"kind": "event", "chat_id": "c", "seq_from": 5, "seq_to": 4}),
        json!({"kind": "event", "chat_id": "c", "seq_from": -1, "seq_to": 4}),
        json!({"kind": "event", "chat_id": "c/../d", "seq_from": 1, "seq_to": 1}),
        json!({"kind": "event", "chat_id": "", "seq_from": 1, "seq_to": 1}),
        json!({"kind": "file", "locator": "../kacis.md", "content_hash": h}),
        json!({"kind": "file", "locator": "a.md", "content_hash": "sha256:kisa"}),
        json!({"kind": "import", "importer": "x y", "source_hash": h, "record_key": "k"}),
        json!({"kind": "telegram", "chat_id": "1"}),
        json!({"kind": "event", "chat_id": "c", "seq_from": 1, "seq_to": 1, "extra": 1}),
    ] {
        assert!(serde_json::from_value::<SourceRef>(bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn receipts_are_masked_canonical_and_hash_bound() {
    let secret = ["sentetik", "deger"].join("_");
    let source = SourceRef::Event { chat_id: "sohbet-1".into(), seq_from: 1, seq_to: 2 };
    let receipt = Receipt::new(
        source.clone(),
        &format!("Kullanıcı notu example --password {secret} ğüşiöç"),
        ts("2026-11-18T09:00:00Z"),
        ClaimStatus::Claimed,
    )
    .unwrap();
    assert!(!receipt.excerpt.contains(&secret));
    assert!(receipt.excerpt.contains("ğüşiöç"));

    let bytes = receipt.canonical_json().unwrap();
    let h = receipt.hash().unwrap();
    assert_eq!(h, receipt.hash().unwrap());
    assert_eq!(Receipt::relative_path(&h), format!("receipts/{}.json", h.hex()));
    assert_eq!(Receipt::from_canonical_json(&bytes, &h).unwrap(), receipt);
    // Başka hash ile eşleşmez; bozulmuş içerik reddedilir.
    assert!(Receipt::from_canonical_json(&bytes, &hash("baska")).is_err());
    let tampered = String::from_utf8(bytes.clone()).unwrap().replace("claimed", "verified");
    assert!(Receipt::from_canonical_json(tampered.as_bytes(), &h).is_err());
    // Kanonik olmayan (boşluklu) biçim de reddedilir.
    let pretty = serde_json::to_vec_pretty(&receipt).unwrap();
    assert!(Receipt::from_canonical_json(&pretty, &Sha256Hash::of(&pretty)).is_err());
    // Alıntı değişince hash değişir.
    let other = Receipt::new(source.clone(), "başka", ts("2026-11-18T09:00:00Z"), ClaimStatus::Claimed).unwrap();
    assert_ne!(other.hash().unwrap(), h);
    // Maskelenmemiş içerik doğrudan yapılandırılsa bile doğrulama reddeder.
    let raw = Receipt {
        version: RECEIPT_VERSION,
        source,
        excerpt: format!("example --password {secret}"),
        captured_at: ts("2026-11-18T09:00:00Z"),
        claim_status: ClaimStatus::UserAuthored,
    };
    assert!(raw.validate().is_err());
    let mut wrong_version = receipt.clone();
    wrong_version.version = 2;
    assert_eq!(wrong_version.validate().unwrap_err(), ContractError::UnknownSchemaVersion);
    let long = Receipt::new(receipt.source.clone(), &"ğ".repeat(5000), ts("2026-11-18T09:00:00Z"), ClaimStatus::Claimed).unwrap();
    assert_eq!(long.excerpt.chars().count(), MAX_RECEIPT_EXCERPT_CHARS);
}

#[test]
fn value_types_reject_bad_serde_input() {
    assert!(serde_json::from_value::<Revision>(json!(0)).is_err());
    assert!(serde_json::from_value::<Revision>(json!(-3)).is_err());
    assert!(serde_json::from_value::<Revision>(json!(u64::MAX)).is_err());
    assert_eq!(serde_json::from_value::<Revision>(json!(7)).unwrap().get(), 7);
    assert!(serde_json::from_value::<MemoryId>(json!("ABC")).is_err());
    assert!(serde_json::from_value::<MemoryId>(json!("11111111111141118111111111111111")).is_err());
    assert!(serde_json::from_value::<Sha256Hash>(json!("sha256:zz")).is_err());
    let id = MemoryId::new();
    assert_eq!(serde_json::from_value::<MemoryId>(serde_json::to_value(&id).unwrap()).unwrap(), id);
    assert_eq!(Sha256Hash::from_hex(hash("a").hex()).unwrap(), hash("a"));
    assert!(Sha256Hash::from_hex("ABC").is_err());
}

// ---------------------------------------------------------------------------------------------
// Öneri, bağlam, gölge, yetki
// ---------------------------------------------------------------------------------------------

fn candidate() -> SuggestionCandidate {
    SuggestionCandidate {
        target: SuggestionTarget::New,
        memory_type: MemoryType::User,
        title: "Sentetik aday".into(),
        summary: "Özet".into(),
        body: "Gövde ğü".into(),
        tags: vec!["a".into()],
        project: None,
    }
}

#[test]
fn candidate_hash_binds_displayed_content_and_requires_masking() {
    let c = candidate();
    let rs = vec![hash("r")];
    let h = c.candidate_hash(&rs).unwrap();
    assert_eq!(h, c.candidate_hash(&rs).unwrap());
    let mut changed = c.clone();
    changed.body.push('!');
    assert_ne!(changed.candidate_hash(&rs).unwrap(), h);
    let secret = ["sentetik", "deger"].join("_");
    let mut raw = c.clone();
    raw.body = format!("example --password {secret}");
    assert!(raw.candidate_hash(&rs).is_err());
    let mut update = c.clone();
    update.target = SuggestionTarget::Update { id: MemoryId::new(), expected_revision: Revision::FIRST };
    assert!(update.modifies_existing() && !c.modifies_existing());
    let wire = serde_json::to_value(&update).unwrap();
    assert_eq!(serde_json::from_value::<SuggestionCandidate>(wire).unwrap(), update);
    assert!(serde_json::from_value::<SuggestionCandidate>(json!({"target": {"kind": "new"}, "memory_type": "user",
        "title": "t", "summary": "", "body": "b", "oturum": 1})).is_err());

    let suggestion = MemorySuggestion {
        id: MemoryId::new(),
        chat_id: "sohbet-1".into(),
        logical_turn_id: "tur-1".into(),
        end_seq: 4,
        candidate: c.clone(),
        candidate_hash: h.clone(),
        receipts: vec![hash("r")],
        status: SuggestionStatus::Pending,
        created_at: ts("2026-11-18T09:00:00Z"),
        decided_at: None,
        decided_by: None,
    };
    assert!(suggestion.validate().is_ok());
    let mut wrong_hash = suggestion.clone();
    wrong_hash.candidate_hash = hash("yanlis");
    assert!(wrong_hash.validate().is_err());
    let mut accepted_without_decision = suggestion.clone();
    accepted_without_decision.status = SuggestionStatus::Accepted;
    assert!(accepted_without_decision.validate().is_err());
    let mut pending_with_decision = suggestion;
    pending_with_decision.decided_by = Some(DecisionSource::Desktop);
    assert!(pending_with_decision.validate().is_err());
}

#[test]
fn shadow_score_is_bounded_and_not_writable_over_ipc() {
    let score = ApprovalShadowScore {
        approval_id: "onay-1".into(),
        model: "jeb:4b".into(),
        prompt_version: "shadow-b-v1".into(),
        p_safe: 0.5,
        measured_at: ts("2026-11-18T09:00:00Z"),
        duration_ms: 12,
        action_fingerprint: hash("eylem"),
    };
    assert!(score.validate().is_ok());
    for bad in [-0.01, 1.01, f64::NAN, f64::INFINITY] {
        let mut s = score.clone();
        s.p_safe = bad;
        assert!(s.validate().is_err(), "{bad}");
    }
    let wire = serde_json::to_value(&score).unwrap();
    assert!(wire.get("decision").is_none() && wire.get("threshold").is_none() && wire.get("approve").is_none());
    assert!(serde_json::from_value::<ApprovalShadowScore>(json!({
        "approval_id": "o", "model": "m", "prompt_version": "v", "p_safe": 0.9,
        "measured_at": "2026-11-18T09:00:00Z", "duration_ms": 1,
        "action_fingerprint": hash("x").prefixed(), "approve": true
    })).is_err());
    // İstemci puan yazamaz: hiçbir Request varyantı yok.
    for kind in ["approval_shadow_score", "set_shadow_score", "approvals_shadow_score"] {
        assert!(serde_json::from_value::<Request>(json!({"type": kind, "approval_id": "o", "p_safe": 1.0})).is_err());
    }
}

#[test]
fn authz_matrix_is_exact() {
    use MemoryCaller::*;
    use MemoryOp::*;
    let all = [
        List, Get, Search, Capture, Update, Delete, ListSuggestions,
        ResolveSuggestion { modifies_existing: false }, ResolveSuggestion { modifies_existing: true },
        Refresh, ShadowReport,
    ];
    for op in all {
        assert_eq!(authorize(Desktop, op), Ok(Audience::Desktop), "{op:?}");
    }
    for op in all {
        let expected = match op {
            Search | Capture | ResolveSuggestion { modifies_existing: false } => Ok(Audience::Telegram),
            ResolveSuggestion { modifies_existing: true } => Err(AuthzDenied::UseDesktop),
            _ => Err(AuthzDenied::NotAllowed),
        };
        assert_eq!(authorize(Telegram, op), expected, "telegram {op:?}");
        let engine = match op {
            Search | Get => Ok(Audience::Engine),
            _ => Err(AuthzDenied::NotAllowed),
        };
        assert_eq!(authorize(Engine { live_turn_token: true }, op), engine, "engine {op:?}");
        let stale = match op {
            Search | Get => Err(AuthzDenied::TokenRequired),
            _ => Err(AuthzDenied::NotAllowed),
        };
        assert_eq!(authorize(Engine { live_turn_token: false }, op), stale, "engine tokensiz {op:?}");
    }
}

#[test]
fn audience_filters_private_and_cross_project_items() {
    use Audience::*;
    // Masaüstü her şeyi görür.
    assert!(Desktop.admits(Visibility::Private, false, Some("p"), None));
    for audience in [Engine, Telegram] {
        // Varsayılan (private, remote_allowed=false) hiçbir zaman dışarı çıkmaz; private+remote da.
        assert!(!audience.admits(Visibility::Private, false, None, None));
        assert!(!audience.admits(Visibility::Private, true, None, None));
        assert!(!audience.admits(Visibility::Shared, false, None, Some("p")));
        // Açık izin + projesiz paylaşım açılır.
        assert!(audience.admits(Visibility::Shared, true, None, None));
        // Proje kapsamı eşleşmeli.
        assert!(audience.admits(Visibility::Project, true, Some("p"), Some("p")));
        assert!(!audience.admits(Visibility::Project, true, Some("p"), Some("q")));
        assert!(!audience.admits(Visibility::Shared, true, Some("p"), None));
        // Projesiz "project" görünürlüğü bozuk sayılır.
        assert!(!audience.admits(Visibility::Project, true, None, Some("p")));
    }
}

#[test]
fn token_bound_is_utf8_bytes_not_chars_over_four() {
    assert_eq!(token_upper_bound("abcd"), 4);
    assert_eq!(token_upper_bound("ğğğğ"), 8);
}

// ---------------------------------------------------------------------------------------------
// IPC
// ---------------------------------------------------------------------------------------------

fn capture(text: &str) -> MemoryCaptureRequest {
    MemoryCaptureRequest {
        request_id: "istek-1".into(),
        text: text.into(),
        title: Some("Başlık".into()),
        project: None,
        tags: vec![],
        chat_ref: Some(ChatRef { chat_id: "sohbet-1".into(), seq_from: 1, seq_to: 2 }),
    }
}

fn memory_requests() -> Vec<Request> {
    let id = MemoryId::new();
    vec![
        Request::MemoryList { cursor: Some("imlec".into()), limit: Some(50), project: Some("p".into()), memory_type: Some(MemoryType::Tool) },
        Request::MemoryList { cursor: None, limit: None, project: None, memory_type: None },
        Request::MemoryGet { id: id.clone() },
        Request::MemorySearch { query: "çalışma notu".into(), project: None, limit: Some(10) },
        Request::MemoryCapture(capture("sentetik not")),
        Request::MemoryUpdate(MemoryUpdateRequest {
            request_id: "istek-2".into(),
            id: id.clone(),
            expected_revision: Revision::FIRST,
            patch: MemoryPatch { title: Some("Yeni".into()), project: Some(None), remote_allowed: Some(true), ..Default::default() },
        }),
        Request::MemoryDelete { request_id: "istek-3".into(), id: id.clone(), expected_revision: Revision::new(2).unwrap() },
        Request::MemorySuggestions { status: Some(SuggestionStatus::Pending), cursor: None, limit: Some(10) },
        Request::MemoryResolveSuggestion { request_id: "istek-4".into(), id, accept: true, expected_candidate_hash: hash("aday") },
        Request::MemoryRefresh { request_id: "istek-5".into() },
        Request::ApprovalsShadowReport { since_days: Some(7) },
        Request::ApprovalsShadowReport { since_days: None },
    ]
}

#[test]
fn memory_requests_roundtrip_and_validate() {
    for request in memory_requests() {
        assert!(request.is_memory_request());
        request.validate_memory().unwrap_or_else(|e| panic!("{request:?}: {e}"));
        let wire = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&wire).unwrap(), request, "{wire}");
    }
    assert!(!Request::Status.is_memory_request());
    assert!(Request::Status.validate_memory().is_ok());
    // `project: null` ile "projeden ayır" ve "dokunma" ayrımı korunur.
    let detach: Request = serde_json::from_value(json!({"type": "memory_update", "request_id": "r", "id": MemoryId::new(),
        "expected_revision": 1, "patch": {"project": null}})).unwrap();
    let Request::MemoryUpdate(update) = detach else { panic!() };
    assert_eq!(update.patch.project, Some(None));
    let keep: Request = serde_json::from_value(json!({"type": "memory_update", "request_id": "r", "id": MemoryId::new(),
        "expected_revision": 1, "patch": {"title": "t"}})).unwrap();
    let Request::MemoryUpdate(update) = keep else { panic!() };
    assert_eq!(update.patch.project, None);
}

#[test]
fn memory_requests_reject_out_of_bounds_input() {
    let id = MemoryId::new();
    let bad: Vec<Request> = vec![
        Request::MemoryList { cursor: None, limit: Some(0), project: None, memory_type: None },
        Request::MemoryList { cursor: None, limit: Some(51), project: None, memory_type: None },
        Request::MemoryList { cursor: Some("a b".into()), limit: None, project: None, memory_type: None },
        Request::MemoryList { cursor: Some("x".repeat(MAX_CURSOR_BYTES + 1)), limit: None, project: None, memory_type: None },
        Request::MemorySearch { query: String::new(), project: None, limit: None },
        Request::MemorySearch { query: "x".repeat(MAX_QUERY_CHARS + 1), project: None, limit: None },
        Request::MemorySearch { query: "a\nb".into(), project: None, limit: None },
        Request::MemorySuggestions { status: None, cursor: None, limit: Some(11) },
        Request::MemoryCapture(MemoryCaptureRequest { request_id: "../x".into(), ..capture("a") }),
        Request::MemoryCapture(MemoryCaptureRequest { request_id: String::new(), ..capture("a") }),
        Request::MemoryCapture(capture("")),
        Request::MemoryCapture(capture(&"x".repeat(MAX_CAPTURE_TEXT_BYTES + 1))),
        Request::MemoryCapture(capture("a\u{1b}b")),
        Request::MemoryCapture(MemoryCaptureRequest { tags: vec!["t".into(); MAX_TAGS + 1], ..capture("a") }),
        Request::MemoryCapture(MemoryCaptureRequest { chat_ref: Some(ChatRef { chat_id: "c".into(), seq_from: 0, seq_to: 1 }), ..capture("a") }),
        Request::MemoryUpdate(MemoryUpdateRequest { request_id: "r".into(), id: id.clone(), expected_revision: Revision::FIRST, patch: MemoryPatch::default() }),
        Request::MemoryUpdate(MemoryUpdateRequest {
            request_id: "r".into(), id: id.clone(), expected_revision: Revision::FIRST,
            patch: MemoryPatch { body: Some("x".repeat(MAX_BODY_BYTES + 1)), ..Default::default() },
        }),
        Request::MemoryDelete { request_id: "a b".into(), id: id.clone(), expected_revision: Revision::FIRST },
        Request::MemoryRefresh { request_id: "x".repeat(MAX_REQUEST_ID_CHARS + 1) },
        Request::ApprovalsShadowReport { since_days: Some(0) },
        Request::ApprovalsShadowReport { since_days: Some(366) },
    ];
    for request in bad {
        assert!(request.validate_memory().is_err(), "kabul edilmemeliydi: {request:?}");
    }
    // Bilinmeyen alan, serbest kaynak yolu veya Telegram kimliği taşınamaz.
    for smuggled in [json!({"source": "C:/x"}), json!({"telegram_user_id": 1}), json!({"audience": "desktop"})] {
        let mut wire = json!({"type": "memory_capture", "request_id": "r", "text": "t"});
        wire.as_object_mut().unwrap().extend(smuggled.as_object().unwrap().clone());
        assert!(serde_json::from_value::<Request>(wire).is_err());
    }
    assert!(serde_json::from_value::<Request>(json!({"type": "memory_get", "id": "../../etc"})).is_err());
    assert!(serde_json::from_value::<Request>(json!({"type": "memory_delete", "request_id": "r",
        "id": MemoryId::new(), "expected_revision": 0})).is_err());
}

fn view(c: char) -> MemoryItemView {
    MemoryItemView {
        id: MemoryId::new(),
        memory_type: MemoryType::Decision,
        title: c.to_string().repeat(MAX_TITLE_CHARS),
        summary: c.to_string().repeat(MAX_SUMMARY_CHARS),
        project: Some(c.to_string().repeat(MAX_PROJECT_CHARS)),
        visibility: Visibility::Project,
        remote_allowed: true,
        sensitivity: Sensitivity::Finance,
        revision: Revision::new(i64::MAX as u64).unwrap(),
        content_hash: hash("v"),
        created_at: ts("2026-11-18T09:00:00Z"),
        updated_at: ts("2026-11-18T09:00:00Z"),
        review_at: Some(ts("2026-12-18T09:00:00Z")),
        tags: vec![c.to_string().repeat(MAX_TAG_CHARS); MAX_TAGS],
        index_state: IndexState::Quarantined,
        receipt_count: u32::MAX,
    }
}

fn frame_len<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).unwrap().len() + 1
}

#[test]
fn worst_case_dtos_fit_in_one_ipc_frame() {
    // En kötü karakter: 4 baytlık UTF-8 ve (gövdelerde) JSON'da 2 bayta açılan tırnak.
    let wide = '\u{1d538}';
    let page: MemoryPage<MemoryItemView> = MemoryPage {
        items: (0..MAX_PAGE_LIMIT).map(|_| view(wide)).collect(),
        next_cursor: Some("c".repeat(MAX_CURSOR_BYTES)),
    };
    assert!(frame_len(&page) < MAX_LINE_BYTES, "liste {}", frame_len(&page));
    let hits: MemoryPage<MemorySearchHit> = MemoryPage {
        items: (0..MAX_PAGE_LIMIT).map(|_| MemorySearchHit { item: view(wide), snippet: wide.to_string().repeat(MAX_SNIPPET_CHARS) }).collect(),
        next_cursor: None,
    };
    assert!(frame_len(&hits) < MAX_LINE_BYTES, "arama {}", frame_len(&hits));

    let receipt = ReceiptView {
        hash: hash("r"),
        source: SourceRef::File { locator: "a/".repeat(MAX_LOCATOR_BYTES / 2 - 1) + "b", content_hash: hash("c") },
        claim_status: ClaimStatus::UserAuthored,
        captured_at: ts("2026-11-18T09:00:00Z"),
        excerpt: "\"".repeat(MAX_RECEIPT_EXCERPT_CHARS),
        evidence_available: false,
    };
    let detail = MemoryItemDetail { item: view(wide), body: "\"".repeat(MAX_BODY_BYTES), receipts: vec![receipt; MAX_DETAIL_RECEIPTS] };
    assert!(frame_len(&detail) < MAX_LINE_BYTES, "ayrıntı {}", frame_len(&detail));

    let suggestion = |c: char| {
        let candidate = SuggestionCandidate {
            target: SuggestionTarget::Update { id: MemoryId::new(), expected_revision: Revision::FIRST },
            memory_type: MemoryType::Rule,
            title: c.to_string().repeat(MAX_TITLE_CHARS),
            summary: c.to_string().repeat(MAX_SUMMARY_CHARS),
            body: "\"".repeat(MAX_CANDIDATE_BODY_BYTES),
            tags: vec![c.to_string().repeat(MAX_TAG_CHARS); MAX_TAGS],
            project: Some(c.to_string().repeat(MAX_PROJECT_CHARS)),
        };
        MemorySuggestion {
            id: MemoryId::new(),
            chat_id: "c".repeat(MAX_ID_TOKEN_CHARS),
            logical_turn_id: "t".repeat(MAX_ID_TOKEN_CHARS),
            end_seq: i64::MAX,
            candidate_hash: hash("a"),
            candidate,
            receipts: vec![hash("r"); MAX_RECEIPTS_PER_ITEM],
            status: SuggestionStatus::Accepted,
            created_at: ts("2026-11-18T09:00:00Z"),
            decided_at: Some(ts("2026-11-18T09:00:00Z")),
            decided_by: Some(DecisionSource::Telegram),
        }
    };
    let page = MemoryPage { items: (0..MAX_SUGGESTION_PAGE_LIMIT).map(|_| suggestion(wide)).collect::<Vec<_>>(), next_cursor: None };
    assert!(frame_len(&page) < MAX_LINE_BYTES, "öneri {}", frame_len(&page));

    // İstekler de çerçeveye sığar (en büyük gövdeler).
    let update = Request::MemoryUpdate(MemoryUpdateRequest {
        request_id: "r".into(),
        id: MemoryId::new(),
        expected_revision: Revision::FIRST,
        patch: MemoryPatch {
            title: Some(wide.to_string().repeat(MAX_TITLE_CHARS)),
            summary: Some(wide.to_string().repeat(MAX_SUMMARY_CHARS)),
            body: Some("\"".repeat(MAX_BODY_BYTES)),
            tags: Some(vec![wide.to_string().repeat(MAX_TAG_CHARS); MAX_TAGS]),
            project: Some(Some(wide.to_string().repeat(MAX_PROJECT_CHARS))),
            ..Default::default()
        },
    });
    update.validate_memory().unwrap();
    assert!(frame_len(&update) < MAX_LINE_BYTES, "güncelleme {}", frame_len(&update));
    let capture = Request::MemoryCapture(MemoryCaptureRequest { text: "\"".repeat(MAX_CAPTURE_TEXT_BYTES), ..capture("a") });
    capture.validate_memory().unwrap();
    assert!(frame_len(&capture) < MAX_LINE_BYTES);

    let context = MemoryContext {
        items: (0..20)
            .map(|_| MemoryContextItem {
                item_id: MemoryId::new(),
                revision: Revision::FIRST,
                title: wide.to_string().repeat(MAX_TITLE_CHARS),
                summary: wide.to_string().repeat(MAX_SUMMARY_CHARS),
                snippet: wide.to_string().repeat(MAX_SNIPPET_CHARS),
                receipts: vec![hash("r"); MAX_RECEIPTS_PER_ITEM],
                index_state: IndexState::Indexed,
                truncated: true,
            })
            .collect(),
        truncated: true,
        budget: MemoryBudget { max_tokens: 8000, max_chars: 32000 },
        used_chars: 32000,
        token_upper_bound: 8000,
    };
    assert!(frame_len(&context) < MAX_LINE_BYTES, "bağlam {}", frame_len(&context));
}

#[test]
fn debug_output_never_prints_content_or_tokens() {
    let marker = "SENTETIK_GIZLI_ICERIK";
    let id = MemoryId::new();
    let mut dumps: Vec<String> = Vec::new();
    dumps.push(format!("{:?}", Request::MemoryCapture(capture(marker))));
    dumps.push(format!("{:?}", Request::MemorySearch { query: marker.into(), project: Some(marker.into()), limit: None }));
    dumps.push(format!(
        "{:?}",
        Request::MemoryUpdate(MemoryUpdateRequest {
            request_id: "r".into(),
            id: id.clone(),
            expected_revision: Revision::FIRST,
            patch: MemoryPatch { title: Some(marker.into()), body: Some(marker.into()), summary: Some(marker.into()), ..Default::default() },
        })
    ));
    dumps.push(format!("{:?}", MemoryToolRequest::MemorySearch { query: marker.into(), limit: None }));
    let mut item = view('a');
    item.title = marker.into();
    item.summary = marker.into();
    item.project = Some(marker.into());
    item.tags = vec![marker.into()];
    dumps.push(format!("{item:?}"));
    dumps.push(format!("{:?}", MemorySearchHit { item: item.clone(), snippet: marker.into() }));
    dumps.push(format!("{:?}", MemoryItemDetail { item: item.clone(), body: marker.into(), receipts: vec![] }));
    dumps.push(format!(
        "{:?}",
        ReceiptView {
            hash: hash("r"),
            source: SourceRef::File { locator: marker.into(), content_hash: hash("c") },
            claim_status: ClaimStatus::Claimed,
            captured_at: ts("2026-11-18T09:00:00Z"),
            excerpt: marker.into(),
            evidence_available: true,
        }
    ));
    dumps.push(format!(
        "{:?}",
        Receipt {
            version: 1,
            source: SourceRef::Event { chat_id: marker.into(), seq_from: 1, seq_to: 1 },
            excerpt: marker.into(),
            captured_at: ts("2026-11-18T09:00:00Z"),
            claim_status: ClaimStatus::Claimed,
        }
    ));
    let mut cand = candidate();
    cand.title = marker.into();
    cand.summary = marker.into();
    cand.body = marker.into();
    cand.tags = vec![marker.into()];
    dumps.push(format!("{cand:?}"));
    let mut front = sample();
    front.title = marker.into();
    front.summary = marker.into();
    front.extra.insert("x".into(), json!(marker));
    dumps.push(format!("{front:?}"));
    dumps.push(format!(
        "{:?}",
        MemoryContext {
            items: vec![MemoryContextItem {
                item_id: id,
                revision: Revision::FIRST,
                title: marker.into(),
                summary: marker.into(),
                snippet: marker.into(),
                receipts: vec![],
                index_state: IndexState::Indexed,
                truncated: false,
            }],
            truncated: false,
            budget: MemoryBudget { max_tokens: 1, max_chars: 1 },
            used_chars: 1,
            token_upper_bound: 1,
        }
    ));
    dumps.push(format!("{:?}", SourceRef::Event { chat_id: marker.into(), seq_from: 1, seq_to: 2 }));
    for dump in dumps {
        assert!(!dump.contains(marker), "içerik sızdı: {dump}");
    }
    // Hata metni de içerik taşımaz.
    let err = parse_front_matter(&format!("{}x: \"{marker}\u{0007}\"\n", sample_yaml())).unwrap_err();
    assert!(!err.to_string().contains(marker) && !format!("{err:?}").contains(marker));
}

// ---------------------------------------------------------------------------------------------
// Olaylar
// ---------------------------------------------------------------------------------------------

#[test]
fn new_engine_events_roundtrip_without_content_fields() {
    let events = [
        EngineEvent::MemorySuggestionCreated { suggestion_id: MemoryId::new(), candidate_hash: hash("a"), end_seq: 3 },
        EngineEvent::MemorySuggestionResolved { suggestion_id: MemoryId::new(), status: SuggestionStatus::Rejected, by: DecisionSource::Desktop },
        EngineEvent::MemoryItemChanged {
            item_id: MemoryId::new(),
            revision: Revision::new(2).unwrap(),
            content_hash: hash("b"),
            change: MemoryChangeKind::ExternalChange,
            source_seq: Some(7),
        },
        EngineEvent::MemoryItemChanged { item_id: MemoryId::new(), revision: Revision::FIRST, content_hash: hash("c"), change: MemoryChangeKind::Trashed, source_seq: None },
        EngineEvent::MemoryIndexStatusChanged { status: MemoryIndexStatus::Degraded, pending: 4 },
        EngineEvent::ApprovalShadowScored { approval_id: "onay-1".into(), model: "jeb:4b".into(), prompt_version: "shadow-b-v1".into(), p_safe: 0.25 },
    ];
    let kinds = [
        "memory_suggestion_created", "memory_suggestion_resolved", "memory_item_changed", "memory_item_changed",
        "memory_index_status_changed", "approval_shadow_scored",
    ];
    for (event, kind) in events.into_iter().zip(kinds) {
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], kind);
        for forbidden in ["body", "title", "summary", "text", "candidate", "snippet", "excerpt"] {
            assert!(wire.get(forbidden).is_none(), "{kind}: {forbidden}");
        }
        assert_eq!(serde_json::from_value::<EngineEvent>(wire).unwrap(), event);
    }
    assert!(serde_json::from_value::<EngineEvent>(json!({"type": "memory_item_changed", "item_id": "x",
        "revision": 1, "content_hash": "y", "change": "created", "source_seq": null})).is_err());
}

// ---------------------------------------------------------------------------------------------
// Düzeltme turu (inceleme bulguları)
// ---------------------------------------------------------------------------------------------

#[test]
fn rejects_cr_only_and_unicode_line_breaks_that_hide_aliases() {
    let base = sample_yaml();
    // libyaml `\r`, NEL, LS ve PS'yi satır sonu sayar; tarayıcı yalnız `\n`'yi tanır.
    for sep in ["\r", "\u{2028}", "\u{2029}", "\u{0085}"] {
        let bomb = format!("list:{sep}- 1{sep}- &a [x,x,x]{sep}- &b [*a,*a,*a]{sep}- *b{sep}");
        let err = parse_front_matter(&format!("{base}{bomb}")).unwrap_err();
        assert_eq!(err, ContractError::InvalidText, "{sep:?}");
        // Yorum satırı sonu ve tırnak içi satır sonu ile gizleme girişimleri.
        for hidden in [
            format!("c: 1 # yorum{sep}d: &a [1]{sep}e: *a{sep}"),
            format!("q: \"açık{sep}z: &a [1]\"{sep}"),
            format!("q: 'açık{sep}z: *a'{sep}"),
            format!("t: !!str x{sep}"),
        ] {
            assert!(parse_front_matter(&format!("{base}{hidden}")).is_err(), "{hidden:?}");
        }
    }
    // Çıplak \r yalnızca satır sonu olarak `\r\n` ile kabul edilir; tarayıcı bunu doğru işler.
    let crlf = base.replace('\n', "\r\n");
    assert!(parse_front_matter(&crlf).is_ok());
    let crlf_alias = format!("{crlf}x: &a [1]\r\ny: *a\r\n");
    assert_eq!(parse_front_matter(&crlf_alias).unwrap_err(), ContractError::YamlAlias);
    let crlf_comment = format!("{crlf}# yorum &a\r\nok: 1\r\n");
    assert!(parse_front_matter(&crlf_comment).is_ok());
}

#[test]
fn yaml_conversion_rejects_non_string_keys_and_non_finite_numbers() {
    let base = sample_yaml();
    for extra in ["1: a\n", "true: a\n", "? [a]\n: b\n", "x: .nan\n", "x: .inf\n", "x: -.inf\n", "f:\n  2: a\n"] {
        assert!(parse_front_matter(&format!("{base}{extra}")).is_err(), "{extra:?}");
    }
    let ok = parse_front_matter(&format!("{base}x: 1.5\ny: 18446744073709551615\n")).unwrap();
    assert_eq!(ok.extra["x"], json!(1.5));
    assert_eq!(ok.extra["y"], json!(18446744073709551615u64));
}

#[test]
fn front_matter_extra_cannot_shadow_known_keys_and_private_cannot_be_remote() {
    let mut front = sample();
    front.extra.insert("title".into(), json!("gölge"));
    assert!(front.validate().is_err());
    assert!(render_note(&front, "g").is_err());
    let mut front = sample();
    front.remote_allowed = true; // sample: private
    assert_eq!(front.validate().unwrap_err(), ContractError::InvalidField);
    assert!(parse_front_matter(&replace_line(&sample_yaml(), "remote_allowed", "remote_allowed: true")).is_err());
    front.visibility = Visibility::Shared;
    assert!(front.validate().is_ok());
    // Yamada çelişki.
    let patch = MemoryPatch { visibility: Some(Visibility::Private), remote_allowed: Some(true), ..Default::default() };
    assert!(patch.validate().is_err());
    let ok = MemoryPatch { visibility: Some(Visibility::Shared), remote_allowed: Some(true), ..Default::default() };
    assert!(ok.validate().is_ok());
    assert!(check_visibility_pair(Visibility::Private, true).is_err());
    assert!(check_visibility_pair(Visibility::Project, true).is_ok());
}

#[test]
fn new_note_defaults_follow_sensitivity() {
    assert_eq!(default_visibility_for(Sensitivity::Normal), (Visibility::Shared, true));
    assert_eq!(default_visibility_for(Sensitivity::Personal), (Visibility::Private, false));
    assert_eq!(default_visibility_for(Sensitivity::Finance), (Visibility::Private, false));
    for s in [Sensitivity::Normal, Sensitivity::Personal, Sensitivity::Finance] {
        let (vis, remote) = default_visibility_for(s);
        assert!(check_visibility_pair(vis, remote).is_ok());
    }
}

#[test]
fn audience_matrix_private_never_leaves_the_machine() {
    use Audience::*;
    for vis in [Visibility::Private, Visibility::Project, Visibility::Shared] {
        for remote in [false, true] {
            for audience in [Engine, Telegram] {
                let expected = vis != Visibility::Private && remote;
                // Projesiz öğe; `Project` projesiz bozuk sayılır.
                let none = audience.admits(vis, remote, None, Some("p"));
                assert_eq!(none, expected && vis != Visibility::Project, "{audience:?} {vis:?} {remote}");
                // Projeli öğe yalnız eşleşen kapsamda.
                assert_eq!(audience.admits(vis, remote, Some("p"), Some("p")), expected);
                assert!(!audience.admits(vis, remote, Some("p"), Some("q")));
                assert!(!audience.admits(vis, remote, Some("p"), None));
            }
            assert!(Desktop.admits(vis, remote, Some("p"), None));
        }
    }
    // Finans notu: varsayılan (private, remote=false) kapalı; açıkça shared+remote ise açılır.
    let (vis, remote) = default_visibility_for(Sensitivity::Finance);
    assert!(!Engine.admits(vis, remote, None, None) && !Telegram.admits(vis, remote, None, None));
    assert!(Engine.admits(Visibility::Shared, true, None, None));
}

#[test]
fn request_replay_requires_same_fingerprint() {
    let payload_a = json!({"text": "bir"});
    let payload_b = json!({"text": "iki"});
    let h_a = request_fingerprint(MemoryOperationKind::Capture, OperationSource::Desktop, &payload_a).unwrap();
    assert_eq!(h_a, request_fingerprint(MemoryOperationKind::Capture, OperationSource::Desktop, &payload_a).unwrap());
    let others = [
        request_fingerprint(MemoryOperationKind::Capture, OperationSource::Desktop, &payload_b).unwrap(),
        request_fingerprint(MemoryOperationKind::Update, OperationSource::Desktop, &payload_a).unwrap(),
        request_fingerprint(MemoryOperationKind::Capture, OperationSource::Telegram, &payload_a).unwrap(),
    ];
    let op = MemoryOperation {
        request_id: "istek-1".into(),
        request_hash: h_a.clone(),
        source: OperationSource::Desktop,
        kind: MemoryOperationKind::Capture,
        target_id: None,
        expected_revision: None,
        state: MemoryOperationState::Completed,
        result_revision: Some(Revision::FIRST),
        content_hash: None,
        receipt_hash: None,
    };
    assert!(op.check_replay(&h_a).is_ok());
    for other in others {
        assert_ne!(other, h_a);
        assert_eq!(op.check_replay(&other).unwrap_err(), ContractError::RequestConflict);
    }
}

#[test]
fn locator_rejects_superscript_devices_and_trailing_space_stems() {
    for bad in ["COM\u{b9}", "lpt\u{b2}.txt", "a/COM\u{b3}.md", "NUL .txt", "con  .md", "a/PRN .x", "AUX\t.t"] {
        assert!(validate_locator(bad).is_err(), "{bad:?}");
    }
    for ok in ["COM10.md", "console.md", "nul2/x", "COMx"] {
        assert!(validate_locator(ok).is_ok(), "{ok}");
    }
}

#[test]
fn event_ranges_are_bounded() {
    let max = MAX_CHAT_REF_EVENTS;
    assert!(ChatRef { chat_id: "c".into(), seq_from: 1, seq_to: max }.validate().is_ok());
    assert!(ChatRef { chat_id: "c".into(), seq_from: 1, seq_to: max + 1 }.validate().is_err());
    assert!(ChatRef { chat_id: "c".into(), seq_from: 5, seq_to: i64::MAX }.validate().is_err());
    assert!(serde_json::from_value::<SourceRef>(json!({"kind": "event", "chat_id": "c", "seq_from": 1, "seq_to": max + 1})).is_err());
}

#[test]
fn candidate_hash_covers_the_receipt_list() {
    let c = candidate();
    let r1 = hash("r1");
    let r2 = hash("r2");
    let base = c.candidate_hash(std::slice::from_ref(&r1)).unwrap();
    assert_ne!(base, c.candidate_hash(&[r1.clone(), r2.clone()]).unwrap());
    assert_ne!(base, c.candidate_hash(&[]).unwrap());
    // Sıra ve yineleme hash'i değiştirmez.
    assert_eq!(c.candidate_hash(&[r1.clone(), r2.clone()]).unwrap(), c.candidate_hash(&[r2, r1.clone(), r1]).unwrap());
}

#[test]
fn rejects_document_markers_directives_and_scans_flow_openings_everywhere() {
    let base = sample_yaml();
    for extra in [
        "--- [&a x, *a]\n",
        "--- {k: &x [1], j: *x}\n",
        "--- &a x\n",
        "--- !!str x\n",
        "---\n",
        "--- ...\n",
        "---  \n",
        "...\n",
        "... [*a]\n",
        "  ---\n",
        "%YAML 1.2\n",
        "%TAG ! tag:x,2000:\n",
        "\t%YAML 1.1\n",
    ] {
        assert_eq!(parse_front_matter(&format!("{base}{extra}")).unwrap_err(), ContractError::YamlUnsupported, "{extra:?}");
    }
    // Blok bağlamında `x [&a 1, *a]` düz bir skalerdir (çapa/alias yok): metin olarak kabul edilir.
    for (extra, key, expected) in [
        ("k: x [&a 1, *a]\n", "k", "x [&a 1, *a]"),
        ("t: x [&y]\n", "t", "x [&y]"),
        ("t: a & b\n", "t", "a & b"),
        ("t: 5 * 3\n", "t", "5 * 3"),
        ("u: http://x/&a\n", "u", "http://x/&a"),
    ] {
        let front = parse_front_matter(&format!("{base}{extra}")).unwrap_or_else(|e| panic!("{extra:?}: {e:?}"));
        assert_eq!(front.extra.get(key).or_else(|| front.facts.get(key)), Some(&json!(expected)), "{extra:?}");
    }
    // `title: foo [*]` düz bir metindir (bilinen alan olduğundan satır değiştirilir).
    let titled = parse_front_matter(&replace_line(&base, "title", "title: foo [*]")).unwrap();
    assert_eq!(titled.title, "foo [*]");
    // split_note: kapanış olmayan `---` biçimleri ön bilgiye girer ve reddedilir.
    for line in ["--- ...", "---  ", "--- [&a x, *a]"] {
        let note = format!("---\n{base}{line}\n---\ngövde\n");
        assert!(parse_note(&note).is_err(), "{line:?}");
    }
    // Meşru metin etkilenmez.
    let ok = parse_front_matter(&format!("{base}n: \"--- %5 ...\"\nm: a-b ... c\n")).unwrap();
    assert_eq!(ok.extra["n"], json!("--- %5 ..."));
}

#[test]
fn event_parser_rejects_every_known_anchor_alias_and_tag_bypass() {
    let base = sample_yaml();
    for (extra, expected) in [
        ("k: [:&a [x,x,x], :*a]\n", ContractError::YamlAlias),
        ("k: [\"a\"#]\n, &x 1]\n", ContractError::YamlAlias),
        ("k: [&a x, *a]\n", ContractError::YamlAlias),
        ("k: {a: &b 1, c: *b}\n", ContractError::YamlAlias),
        ("k: [!!str x]\n", ContractError::YamlTag),
        ("k: [!t x]\n", ContractError::YamlTag),
        ("k: &a\n  - 1\nj: *a\n", ContractError::YamlAlias),
        ("k: !!map {a: 1}\n", ContractError::YamlTag),
        ("k: !!seq [1]\n", ContractError::YamlTag),
        ("--- [&a x, *a]\n", ContractError::YamlUnsupported),
        ("%TAG ! tag:x,2000:\n", ContractError::YamlUnsupported),
    ] {
        let result = parse_front_matter(&format!("{base}{extra}"));
        assert!(result.is_err(), "kabul edilmemeliydi: {extra:?}");
        // Neden alias/etiket ya da ayrıştırıcı hatası olabilir; asla başarılı değer üretilmez.
        let err = result.unwrap_err();
        assert!(err == expected || err == ContractError::MalformedFrontMatter, "{extra:?}: {err:?}");
    }
}

#[test]
fn flow_lookalikes_are_plain_text_not_anchors_or_tags() {
    // YAML dilbilgisine göre `:&a x`, `:!!str x`, `?&a x` düz skalerdir; olay ayrıştırıcısı bunları
    // çapa/etiket olarak üretmez, değer olarak metin kalır (alias/etiket çözümü hiç çalışmaz).
    let base = sample_yaml();
    for (extra, expected) in [
        ("k: [:&a x]\n", json!([":&a x"])),
        ("k: {:&a x}\n", json!({":&a x": null})),
        ("k: [:!!str x]\n", json!([":!!str x"])),
        ("k: {?&a x, ?*a}\n", json!({"?&a x": null, "?*a": null})),
    ] {
        let front = parse_front_matter(&format!("{base}{extra}")).unwrap();
        assert_eq!(front.extra["k"], expected, "{extra:?}");
    }
}

#[test]
fn event_parser_rejects_billion_laughs_and_multi_document_streams() {
    let base = sample_yaml();
    let mut bomb = String::from("l0: &l0 [\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\"]\n");
    for i in 1..=8 {
        let prev = i - 1;
        bomb.push_str(&format!("l{i}: &l{i} [{}]\n", vec![format!("*l{prev}"); 9].join(",")));
    }
    assert_eq!(parse_front_matter(&format!("{base}{bomb}")).unwrap_err(), ContractError::YamlAlias);
    // Çok belgeli akış: `---` satırı ön kontrolde reddedilir.
    assert!(parse_front_matter(&format!("{base}---\nb: 1\n")).is_err());
    assert!(parse_front_matter(&format!("{base}...\n---\nb: 1\n")).is_err());
}

#[test]
fn plain_scalars_that_only_look_like_yaml_indicators_are_text() {
    let base = sample_yaml();
    let ok = parse_front_matter(&format!(
        "{base}n1: 5\nn2: -3\nn3: 0x1F\nn4: 1e3\nn5: ~\nn6: yes\nn7: 1.2.3\nn8: \"007\"\nn9: 'q'\nn10: [a & b, 5 * 3, \"x\"]\n"
    ))
    .unwrap();
    assert_eq!(ok.extra["n1"], json!(5));
    assert_eq!(ok.extra["n2"], json!(-3));
    assert_eq!(ok.extra["n3"], json!(31));
    assert_eq!(ok.extra["n4"], json!(1000.0));
    assert_eq!(ok.extra["n5"], json!(null));
    assert_eq!(ok.extra["n6"], json!("yes"));
    assert_eq!(ok.extra["n7"], json!("1.2.3"));
    assert_eq!(ok.extra["n8"], json!("007"));
    assert_eq!(ok.extra["n9"], json!("q"));
    assert_eq!(ok.extra["n10"], json!(["a & b", "5 * 3", "x"]));
}
