//! Der ganze Installationsweg gegen Wurzeln im Temp-Verzeichnis: packen, planen, Rechte
//! prüfen, zustimmen, anwenden, upgraden, entfernen — ohne Umgebung, ohne Binary, ohne wasm32.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sepp_pkg::{
    apply_install, check_collisions, check_rights, consent_lines, list, pack_dir, plan_install,
    remove, resolve_rights, resolve_vars, trust_publisher, Installed, PkgArchive, Roots,
    SigningKey, TrustStatus,
};
use sepp_policy::{NetGrant, PolicyFile, ResolveCtx};

const PLUGIN_TOML: &str = "name = \"zaehler\"\nabi = 1\n[capabilities]\nfs_read = [\"./\"]\nnet = [\"api.example.com\", \"*.acme.example\"]\nenv = [\"ACME_TOKEN\"]\n";

fn source_dir(dir: &Path, name: &str, version: &str, rights: &str) {
    std::fs::create_dir_all(dir.join("skills/rechnung")).unwrap();
    std::fs::create_dir_all(dir.join("prompts")).unwrap();
    std::fs::create_dir_all(dir.join("hooks")).unwrap();
    std::fs::create_dir_all(dir.join("plugins")).unwrap();
    std::fs::write(dir.join("skills/rechnung/SKILL.md"), "# Rechnung\n").unwrap();
    std::fs::write(dir.join("prompts/pruefen.md"), "Prüfe die Rechnung.\n").unwrap();
    std::fs::write(
        dir.join("hooks/log.rhai"),
        "fn on_tool_call(ctx) { continue_() }\n",
    )
    .unwrap();
    std::fs::write(dir.join("plugins/zaehler.wasm"), b"\0asm\x01\0\0\0fake").unwrap();
    std::fs::write(dir.join("plugins/zaehler.toml"), PLUGIN_TOML).unwrap();
    std::fs::write(
        dir.join("manifest.toml"),
        format!(
            "format = 1\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"Demo\"\n\n\
             [publisher]\nname = \"acme\"\n\n\
             [vars.BELEGE_DIR]\ndescription = \"Ordner mit den Belegen\"\nkind = \"path\"\n\n\
             [vars.MANDANT]\ndescription = \"Mandant\"\ndefault = \"nord\"\n\n{rights}"
        ),
    )
    .unwrap();
}

const RIGHTS: &str = "[rights.zaehler]\nfs_read = [\"${BELEGE_DIR}\"]\nnet = [\"api.${MANDANT}.acme.example\"]\nenv = [\"ACME_TOKEN\"]\n";

struct Fixture {
    _tmp: tempfile::TempDir,
    roots: Roots,
    ctx: ResolveCtx,
    home: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    // Rechte werden kanonisch geschrieben; auf macOS liegt `TMPDIR` unter `/var` → `/private/var`,
    // also muss auch die Erwartung vom kanonischen Pfad ausgehen.
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let roots = Roots {
        config: home.join(".sepp"),
        state: home.join(".sepp"),
    };
    std::fs::create_dir_all(&roots.config).unwrap();
    // Eine Policy des Nutzers, die unangetastet bleiben muss.
    std::fs::write(
        roots.policy_path(),
        "# meine Policy\n[agent]\nnet = true\n\n[plugin.eigenes]\nfs_read = [\"~/x\"]\n",
    )
    .unwrap();
    let ctx = ResolveCtx {
        home: Some(home.clone()),
        cwd: home.clone(),
        tmpdir: base.join("tmp"),
    };
    Fixture {
        _tmp: tmp,
        roots,
        ctx,
        home,
    }
}

fn pack(fx: &Fixture, name: &str, version: &str, rights: &str, key: &SigningKey) -> PathBuf {
    let src = fx.home.join(format!("src-{name}-{version}"));
    source_dir(&src, name, version, rights);
    let out = fx.home.join(format!("{name}-{version}.seppkg"));
    pack_dir(&src, key, &out).unwrap();
    out
}

fn vars(belege: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("BELEGE_DIR".to_string(), belege.to_string())])
}

#[test]
fn install_writes_block_files_and_receipt_then_remove_restores_everything() {
    let fx = fixture();
    let policy_before = std::fs::read_to_string(fx.roots.policy_path()).unwrap();
    let (key, _) = SigningKey::generate().unwrap();
    let pkg = pack(&fx, "demo", "1.0.0", RIGHTS, &key);

    let archive = PkgArchive::open(&pkg).unwrap();
    let plan = plan_install(&fx.roots, &archive).unwrap();
    assert!(matches!(plan.trust, TrustStatus::New { .. }));
    assert_eq!(plan.inventory.plugins, vec!["zaehler".to_string()]);
    assert_eq!(plan.inventory.hooks, vec!["log.rhai".to_string()]);
    assert!(plan.plugin_manifests.contains_key("zaehler"));

    // Vertrauen, Variablen, Rechte.
    trust_publisher(&fx.roots, &plan.manifest().publisher, "test").unwrap();
    let resolved = resolve_vars(plan.manifest(), &vars("~/belege"), None).unwrap();
    assert!(resolved.missing.is_empty(), "{:?}", resolved.missing);
    assert_eq!(resolved.values["MANDANT"], "nord");
    let rights = resolve_rights(plan.manifest(), &resolved.values, &fx.ctx).unwrap();
    let warnings = check_rights(&plan, &rights, &fx.ctx).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let c = check_collisions(&fx.roots, &plan);
    assert!(c.errors.is_empty() && c.warnings.is_empty(), "{c:?}");

    let lines = consent_lines(&plan, &rights, &resolved.values, &warnings);
    let text = lines.join("\n");
    assert!(text.contains("Paket demo 1.0.0 — Demo"), "{text}");
    assert!(text.contains("Herausgeber acme"), "{text}");
    assert!(
        text.contains(&format!("fs_read  {}", fx.home.join("belege").display())),
        "{text}"
    );
    assert!(text.contains("net      api.nord.acme.example"), "{text}");
    assert!(text.contains("env      ACME_TOKEN"), "{text}");
    assert!(text.contains("Hook log.rhai"), "{text}");

    let report = apply_install(&fx.roots, &archive, &plan, &resolved.values, &rights).unwrap();
    assert!(report.policy_written);
    assert_eq!(report.dir, fx.roots.package_dir("demo"));
    assert!(fx
        .roots
        .package_dir("demo")
        .join("plugins/zaehler.wasm")
        .is_file());
    assert!(fx
        .roots
        .package_dir("demo")
        .join("skills/rechnung/SKILL.md")
        .is_file());
    assert!(!fx.roots.package_dir("demo").join("manifest.toml").exists());
    assert!(fx
        .roots
        .package_dirs()
        .iter()
        .all(|d| !d.to_string_lossy().contains(".staging")));

    let policy = std::fs::read_to_string(fx.roots.policy_path()).unwrap();
    assert!(
        policy.starts_with(&policy_before),
        "Nutzerteil bleibt: {policy}"
    );
    let f = PolicyFile::parse(&policy).unwrap();
    let g = &f.plugin["zaehler"];
    assert_eq!(
        g.fs_read,
        vec![fx.home.join("belege").to_string_lossy().to_string()]
    );
    assert_eq!(g.net, NetGrant::Hosts(vec!["api.nord.acme.example".into()]));
    assert_eq!(g.env, vec!["ACME_TOKEN".to_string()]);
    assert!(f.plugin.contains_key("eigenes"));

    let inst = Installed::load(&fx.roots).unwrap();
    let e = &inst.packages["demo"];
    assert_eq!(e.version, "1.0.0");
    assert_eq!(e.publisher, "acme");
    assert_eq!(e.publisher_fp, key.fingerprint());
    assert_eq!(e.vars["BELEGE_DIR"], "~/belege");
    assert_eq!(e.plugins, vec!["zaehler".to_string()]);

    let listed = list(&fx.roots).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].dir_present && listed[0].receipt.is_some());

    let r = remove(&fx.roots, "demo").unwrap();
    assert!(r.policy_removed && r.dir_removed && r.receipt_removed);
    assert_eq!(
        std::fs::read_to_string(fx.roots.policy_path()).unwrap(),
        policy_before
    );
    assert!(!fx.roots.package_dir("demo").exists());
    assert!(Installed::load(&fx.roots).unwrap().packages.is_empty());
    assert!(
        remove(&fx.roots, "demo").is_err(),
        "zweimal entfernen ist ein Fehler"
    );
    // Das Vertrauen bleibt.
    assert!(fx.roots.trusted_keys_dir().join("acme.json").is_file());
}

#[test]
fn upgrade_replaces_dir_and_block_reuses_vars_and_rejects_lower_versions() {
    let fx = fixture();
    let (key, _) = SigningKey::generate().unwrap();
    let v1 = pack(&fx, "demo", "1.0.0", RIGHTS, &key);
    let a1 = PkgArchive::open(&v1).unwrap();
    let p1 = plan_install(&fx.roots, &a1).unwrap();
    trust_publisher(&fx.roots, &p1.manifest().publisher, "test").unwrap();
    let vals = resolve_vars(p1.manifest(), &vars("~/belege"), None)
        .unwrap()
        .values;
    let rights = resolve_rights(p1.manifest(), &vals, &fx.ctx).unwrap();
    apply_install(&fx.roots, &a1, &p1, &vals, &rights).unwrap();
    let marker = std::fs::write(fx.roots.package_dir("demo").join("alt.txt"), "alt");
    marker.unwrap();

    // Gleiche Version noch einmal → Fehler.
    let e = plan_install(&fx.roots, &a1).unwrap_err().to_string();
    assert!(e.contains("nicht neuer"), "{e}");

    // Höhere Version mit anderen Rechten: Vars werden übernommen.
    let rights2 = "[rights.zaehler]\nfs_read = [\"${BELEGE_DIR}\"]\nnet = [\"api.example.com\"]\n";
    let v2 = pack(&fx, "demo", "1.1.0", rights2, &key);
    let a2 = PkgArchive::open(&v2).unwrap();
    let p2 = plan_install(&fx.roots, &a2).unwrap();
    assert_eq!(p2.trust, TrustStatus::Known);
    assert_eq!(p2.previous.as_ref().unwrap().version, "1.0.0");
    let r = resolve_vars(
        p2.manifest(),
        &BTreeMap::new(),
        p2.previous.as_ref().map(|p| &p.vars),
    )
    .unwrap();
    assert!(r.missing.is_empty());
    assert_eq!(r.values["BELEGE_DIR"], "~/belege");
    let rights = resolve_rights(p2.manifest(), &r.values, &fx.ctx).unwrap();
    let report = apply_install(&fx.roots, &a2, &p2, &r.values, &rights).unwrap();
    assert_eq!(report.upgraded_from.as_deref(), Some("1.0.0"));
    assert!(
        !fx.roots.package_dir("demo").join("alt.txt").exists(),
        "altes Verzeichnis ersetzt"
    );

    let policy = std::fs::read_to_string(fx.roots.policy_path()).unwrap();
    assert_eq!(policy.matches("# von sepp pkg: demo").count(), 1);
    assert!(policy.contains("demo 1.1.0"), "{policy}");
    let f = PolicyFile::parse(&policy).unwrap();
    assert_eq!(
        f.plugin["zaehler"].net,
        NetGrant::Hosts(vec!["api.example.com".into()])
    );
    assert!(f.plugin["zaehler"].env.is_empty());
    assert_eq!(
        Installed::load(&fx.roots).unwrap().packages["demo"].version,
        "1.1.0"
    );

    // Ein niedrigeres Paket ist nicht neuer.
    let v0 = pack(&fx, "demo", "0.9.0", RIGHTS, &key);
    let e = plan_install(&fx.roots, &PkgArchive::open(&v0).unwrap())
        .unwrap_err()
        .to_string();
    assert!(e.contains("nicht neuer"), "{e}");
}

#[test]
fn tofu_rejects_a_second_key_and_a_second_publisher_for_the_same_package() {
    let fx = fixture();
    let (k1, _) = SigningKey::generate().unwrap();
    let (k2, _) = SigningKey::generate().unwrap();
    let v1 = pack(&fx, "demo", "1.0.0", "", &k1);
    let a1 = PkgArchive::open(&v1).unwrap();
    let p1 = plan_install(&fx.roots, &a1).unwrap();
    trust_publisher(&fx.roots, &p1.manifest().publisher, "test").unwrap();
    apply_install(&fx.roots, &a1, &p1, &BTreeMap::new(), &[]).unwrap();
    assert!(
        !fx.roots.policy_path().exists() || {
            let t = std::fs::read_to_string(fx.roots.policy_path()).unwrap();
            !t.contains("sepp pkg")
        },
        "ohne Rechte kein Block"
    );

    // Gleicher Herausgeber-Name, anderer Schlüssel.
    let v2 = pack(&fx, "demo", "1.1.0", "", &k2);
    let p2 = plan_install(&fx.roots, &PkgArchive::open(&v2).unwrap()).unwrap();
    assert!(
        matches!(p2.trust, TrustStatus::Mismatch { .. }),
        "{:?}",
        p2.trust
    );
    assert!(trust_publisher(&fx.roots, &p2.manifest().publisher, "x").is_err());

    // Anderer Herausgeber für ein installiertes Paket.
    let src = fx.home.join("src-fremd");
    source_dir(&src, "demo", "2.0.0", "");
    let m = std::fs::read_to_string(src.join("manifest.toml"))
        .unwrap()
        .replace("name = \"acme\"", "name = \"fremd\"");
    std::fs::write(src.join("manifest.toml"), m).unwrap();
    let out = fx.home.join("fremd.seppkg");
    pack_dir(&src, &k2, &out).unwrap();
    let e = plan_install(&fx.roots, &PkgArchive::open(&out).unwrap())
        .unwrap_err()
        .to_string();
    assert!(e.contains("gehört einem Herausgeber"), "{e}");
}

#[test]
fn collisions_with_user_plugins_are_errors_and_prompts_are_warnings() {
    let fx = fixture();
    std::fs::create_dir_all(fx.roots.user_plugins_dir()).unwrap();
    std::fs::write(fx.roots.user_plugins_dir().join("zaehler.wasm"), b"x").unwrap();
    std::fs::create_dir_all(fx.roots.user_prompts_dir()).unwrap();
    std::fs::write(fx.roots.user_prompts_dir().join("pruefen.md"), "meins").unwrap();
    let (key, _) = SigningKey::generate().unwrap();
    let pkg = pack(&fx, "demo", "1.0.0", "", &key);
    let plan = plan_install(&fx.roots, &PkgArchive::open(&pkg).unwrap()).unwrap();
    let c = check_collisions(&fx.roots, &plan);
    assert_eq!(c.errors.len(), 1, "{c:?}");
    assert!(c.errors[0].contains("zaehler"), "{c:?}");
    assert_eq!(c.warnings.len(), 1, "{c:?}");
    assert!(c.warnings[0].contains("/pruefen"), "{c:?}");

    // Ein zweites Paket mit demselben Plugin: ebenfalls Fehler; das eigene beim Upgrade nicht.
    std::fs::remove_file(fx.roots.user_plugins_dir().join("zaehler.wasm")).unwrap();
    std::fs::remove_file(fx.roots.user_prompts_dir().join("pruefen.md")).unwrap();
    let plan = plan_install(&fx.roots, &PkgArchive::open(&pkg).unwrap()).unwrap();
    trust_publisher(&fx.roots, &plan.manifest().publisher, "t").unwrap();
    apply_install(
        &fx.roots,
        &PkgArchive::open(&pkg).unwrap(),
        &plan,
        &BTreeMap::new(),
        &[],
    )
    .unwrap();
    let other = pack(&fx, "anderes", "1.0.0", "", &key);
    let plan2 = plan_install(&fx.roots, &PkgArchive::open(&other).unwrap()).unwrap();
    let c = check_collisions(&fx.roots, &plan2);
    assert!(c.errors.iter().any(|e| e.contains("Paket `demo`")), "{c:?}");
    let up = pack(&fx, "demo", "1.0.1", "", &key);
    let plan3 = plan_install(&fx.roots, &PkgArchive::open(&up).unwrap()).unwrap();
    let c = check_collisions(&fx.roots, &plan3);
    assert!(
        c.errors.is_empty(),
        "Upgrade kollidiert nicht mit sich selbst: {c:?}"
    );
}

#[test]
fn rights_beyond_the_plugin_manifest_are_refused_and_outside_paths_warn() {
    let fx = fixture();
    let (key, _) = SigningKey::generate().unwrap();

    // Host, den das Plugin-Manifest nicht nennt.
    let pkg = pack(
        &fx,
        "demo",
        "1.0.0",
        "[rights.zaehler]\nnet = [\"evil.example\"]\n",
        &key,
    );
    let plan = plan_install(&fx.roots, &PkgArchive::open(&pkg).unwrap()).unwrap();
    let rights = resolve_rights(plan.manifest(), &BTreeMap::new(), &fx.ctx).unwrap();
    let e = check_rights(&plan, &rights, &fx.ctx)
        .unwrap_err()
        .to_string();
    assert!(e.contains("evil.example"), "{e}");

    // Variable, die das Plugin nicht deklariert.
    let pkg = pack(
        &fx,
        "demo2",
        "1.0.0",
        "[rights.zaehler]\nenv = [\"ANDERES\"]\n",
        &key,
    );
    let plan = plan_install(&fx.roots, &PkgArchive::open(&pkg).unwrap()).unwrap();
    let rights = resolve_rights(plan.manifest(), &BTreeMap::new(), &fx.ctx).unwrap();
    let e = check_rights(&plan, &rights, &fx.ctx)
        .unwrap_err()
        .to_string();
    assert!(e.contains("ANDERES"), "{e}");

    // Schreibrecht ohne fs_write im Manifest.
    let pkg = pack(
        &fx,
        "demo3",
        "1.0.0",
        "[rights.zaehler]\nfs_write = [\"/tmp/x\"]\n",
        &key,
    );
    let plan = plan_install(&fx.roots, &PkgArchive::open(&pkg).unwrap()).unwrap();
    let rights = resolve_rights(plan.manifest(), &BTreeMap::new(), &fx.ctx).unwrap();
    assert!(check_rights(&plan, &rights, &fx.ctx).is_err());

    // Pfad außerhalb des Manifest-Präfixes (`./` = cwd = home): Warnung, kein Fehler.
    let pkg = pack(
        &fx,
        "demo4",
        "1.0.0",
        "[rights.zaehler]\nfs_read = [\"/srv/belege\"]\n",
        &key,
    );
    let plan = plan_install(&fx.roots, &PkgArchive::open(&pkg).unwrap()).unwrap();
    let rights = resolve_rights(plan.manifest(), &BTreeMap::new(), &fx.ctx).unwrap();
    let w = check_rights(&plan, &rights, &fx.ctx).unwrap();
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].contains("/srv/belege"), "{w:?}");
    // Unter dem cwd dagegen: keine Warnung.
    let pkg = pack(
        &fx,
        "demo5",
        "1.0.0",
        "[rights.zaehler]\nfs_read = [\"${BELEGE_DIR}\"]\n",
        &key,
    );
    let plan = plan_install(&fx.roots, &PkgArchive::open(&pkg).unwrap()).unwrap();
    let rights = resolve_rights(plan.manifest(), &vars("~/belege"), &fx.ctx).unwrap();
    assert!(check_rights(&plan, &rights, &fx.ctx).unwrap().is_empty());
}

#[test]
fn missing_vars_without_default_are_reported_and_a_tampered_package_is_refused() {
    let fx = fixture();
    let (key, _) = SigningKey::generate().unwrap();
    let pkg = pack(&fx, "demo", "1.0.0", RIGHTS, &key);
    let plan = plan_install(&fx.roots, &PkgArchive::open(&pkg).unwrap()).unwrap();
    let r = resolve_vars(plan.manifest(), &BTreeMap::new(), None).unwrap();
    assert_eq!(r.missing.len(), 1);
    assert_eq!(r.missing[0].0, "BELEGE_DIR");
    assert_eq!(r.values["MANDANT"], "nord");
    // Ohne Wert kein Recht: substitute scheitert.
    assert!(resolve_rights(plan.manifest(), &r.values, &fx.ctx).is_err());

    // Ein Byte im Archiv kippen → Signatur oder Hash schlägt an, nichts wird installiert.
    let mut bytes = std::fs::read(&pkg).unwrap();
    let i = bytes.len() / 2;
    bytes[i] ^= 0x55;
    let bad = fx.home.join("bad.seppkg");
    std::fs::write(&bad, &bytes).unwrap();
    let res = PkgArchive::open(&bad).and_then(|a| plan_install(&fx.roots, &a).map(|_| ()));
    assert!(res.is_err());
    assert!(!fx.roots.package_dir("demo").exists());
}
