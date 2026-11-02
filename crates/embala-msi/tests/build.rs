//! End-to-end build assertions: structural validity (via the msi crate),
//! byte-for-byte reproducibility, and a payload round-trip through msitools'
//! msiextract.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use embala_msi::{
    FileSpec, MsiArch, MsiSpec, RegistryData, RegistryRoot, RegistryValue, ServiceSpec,
    ServiceStart, build,
};

mod support;
use support::pe::versioned_pe;

const PAYLOAD_EXE: &[u8] = b"MZ fake windows executable payload";
const PAYLOAD_TXT: &[u8] = b"readme contents\n";

fn tmp(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// A two-file spec (one nested dest) over a freshly written payload dir.
fn test_spec(payload_dir: &Path) -> MsiSpec {
    fs::create_dir_all(payload_dir.join("data")).unwrap();
    fs::write(payload_dir.join("hello.exe"), PAYLOAD_EXE).unwrap();
    fs::write(payload_dir.join("readme.txt"), PAYLOAD_TXT).unwrap();
    MsiSpec {
        name: "hello".to_string(),
        display_name: "Embala Hello".to_string(),
        version: "0.1.0".to_string(),
        identifier: "ca.hrndz.embala.hello".to_string(),
        publisher: "Carlos Hernandez".to_string(),
        description: "Embala end-to-end test fixture".to_string(),
        homepage: Some("https://github.com/hurricanehrndz/embala".to_string()),
        arch: MsiArch::X86_64,
        main_executable: "hello.exe".to_string(),
        files: vec![
            FileSpec {
                src: payload_dir.join("hello.exe"),
                dest: "hello.exe".to_string(),
            },
            FileSpec {
                src: payload_dir.join("readme.txt"),
                dest: "data/readme.txt".to_string(),
            },
        ],
        service: None,
        add_to_path: false,
        registry: vec![],
    }
}

#[test]
fn build_is_reproducible_and_structurally_valid() {
    let spec = test_spec(&tmp("payload-repro"));
    let out1 = tmp("repro-1.msi");
    let out2 = tmp("repro-2.msi");
    build(&spec, &out1).unwrap();
    build(&spec, &out2).unwrap();

    // Reproducibility: identical spec => byte-identical installer.
    assert_eq!(
        fs::read(&out1).unwrap(),
        fs::read(&out2).unwrap(),
        "two consecutive builds must be byte-identical"
    );

    // Structural checks, reopening with the msi crate itself. These are the
    // msiexec acceptance gotchas: ANSI codepage, Template arch;lang,
    // compressed-source word count.
    let package = msi::Package::open(fs::File::open(&out1).unwrap()).unwrap();
    assert_eq!(package.database_codepage(), msi::CodePage::Windows1252);
    let summary = package.summary_info();
    assert_eq!(summary.codepage(), msi::CodePage::Windows1252);
    assert_eq!(summary.arch(), Some("x64"));
    assert_eq!(summary.languages(), vec![msi::Language::from_code(1033)]);
    assert_eq!(summary.word_count(), Some(2));
    assert_eq!(summary.page_count(), Some(200));
    assert!(summary.uuid().is_some(), "PackageCode must be set");
}

#[test]
fn major_upgrade_tables_are_authored() {
    let spec = test_spec(&tmp("payload-upgrade"));
    let out = tmp("upgrade.msi");
    build(&spec, &out).unwrap();
    let mut package = msi::Package::open(fs::File::open(&out).unwrap()).unwrap();

    // Upgrade: one remove-older row and one detect-newer (OnlyDetect) row,
    // both keyed on the stable UpgradeCode. These drive RemoveExistingProducts
    // and the downgrade LaunchCondition respectively.
    // Rows rendered as "UpgradeCode min..max attrs -> prop" for comparison.
    let rows: Vec<String> = package
        .select_rows(msi::Select::table("Upgrade"))
        .unwrap()
        .map(|row| {
            format!(
                "{} {}..{} {} -> {}",
                row[0].as_str().unwrap(),
                row[1].as_str().unwrap_or(""),
                row[2].as_str().unwrap_or(""),
                row[4].as_int().unwrap(),
                row[6].as_str().unwrap(),
            )
        })
        .collect();
    let upgrade_code = "{AEEE1525-B29B-5F4D-AA97-3E85DD62F6D2}";
    // Attributes: 1 = msidbUpgradeAttributesMigrateFeatures,
    //             2 = msidbUpgradeAttributesOnlyDetect,
    //             0x200 = msidbUpgradeAttributesVersionMaxInclusive (513 = 1 | 0x200).
    // The inclusive max is what lets a build-number bump (1.2.3.4 -> 1.2.3.5)
    // replace the old install: Windows Installer ignores the fourth field, so
    // an exclusive max would leave both installed side by side.
    assert!(
        rows.contains(&format!("{upgrade_code} ..0.1.0 513 -> OLDPRODUCTFOUND")),
        "missing remove-older Upgrade row: {rows:?}"
    );
    assert!(
        rows.contains(&format!("{upgrade_code} 0.1.0.. 2 -> NEWERVERSIONDETECTED")),
        "missing detect-newer Upgrade row: {rows:?}"
    );
    assert_eq!(rows.len(), 2);

    // FindRelatedProducts only assigns the action properties if they are
    // secure; without this the elevated execute sequence can't see them.
    let secure: Vec<String> = package
        .select_rows(msi::Select::table("Property"))
        .unwrap()
        .filter(|row| row[0].as_str() == Some("SecureCustomProperties"))
        .map(|row| row[1].as_str().unwrap().to_string())
        .collect();
    assert_eq!(secure, vec!["OLDPRODUCTFOUND;NEWERVERSIONDETECTED"]);

    // The downgrade guard.
    let conditions: Vec<String> = package
        .select_rows(msi::Select::table("LaunchCondition"))
        .unwrap()
        .map(|row| row[0].as_str().unwrap().to_string())
        .collect();
    assert_eq!(conditions, vec!["NOT NEWERVERSIONDETECTED"]);

    // The actions that make the tables take effect must be sequenced:
    // detection + guard early, RemoveExistingProducts right after
    // InstallValidate (old product is gone before new files go down).
    let seq: Vec<(String, i32)> = package
        .select_rows(msi::Select::table("InstallExecuteSequence"))
        .unwrap()
        .map(|row| {
            (
                row[0].as_str().unwrap().to_string(),
                row[2].as_int().unwrap(),
            )
        })
        .collect();
    for expected in [
        ("FindRelatedProducts", 25),
        ("LaunchConditions", 100),
        ("MigrateFeatureStates", 1200),
        ("RemoveExistingProducts", 1401),
    ] {
        assert!(
            seq.iter().any(|(a, s)| (a.as_str(), *s) == expected),
            "InstallExecuteSequence missing {expected:?}: {seq:?}"
        );
    }
}

#[test]
fn msiextract_round_trips_the_payload() {
    let available = Command::new("msiextract").arg("--version").output().is_ok();
    assert!(
        available,
        "msiextract not on PATH — run tests inside the devenv shell"
    );

    let spec = test_spec(&tmp("payload-extract"));
    let out = tmp("extract.msi");
    build(&spec, &out).unwrap();

    let extract_dir = tmp("extracted");
    let _ = fs::remove_dir_all(&extract_dir);
    fs::create_dir_all(&extract_dir).unwrap();
    let status = Command::new("msiextract")
        .arg("-C")
        .arg(&extract_dir)
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "msiextract failed");

    // msiextract lays files out under the resolved install path; find ours by
    // name and compare bytes with the original payload.
    let exe = find_file(&extract_dir, "hello.exe").expect("hello.exe extracted");
    assert_eq!(fs::read(exe).unwrap(), PAYLOAD_EXE);
    let txt = find_file(&extract_dir, "readme.txt").expect("readme.txt extracted");
    assert_eq!(fs::read(&txt).unwrap(), PAYLOAD_TXT);
    assert_eq!(
        txt.parent().and_then(|p| p.file_name()).unwrap(),
        "data",
        "nested dest must extract into its subdirectory"
    );
}

#[test]
fn non_ascii_spec_is_rejected() {
    let payload = tmp("payload-ascii");
    let mut spec = test_spec(&payload);
    spec.publisher = "Çarlos".to_string();
    let err = build(&spec, &tmp("ascii.msi")).unwrap_err();
    assert!(
        matches!(err, embala_msi::Error::NonAscii { .. }),
        "unexpected error: {err}"
    );
}

fn service_spec(start: ServiceStart) -> ServiceSpec {
    ServiceSpec {
        name: "foo".to_string(),
        display_name: Some("Foo".to_string()),
        description: Some("Foo service".to_string()),
        start,
        arguments: Some("--service".to_string()),
        executable: None,
    }
}

/// Every row of `table`, rendered tab-separated (NULL = empty), the shape
/// `msiinfo export` prints.
fn rows(package: &mut msi::Package<fs::File>, table: &str) -> Vec<String> {
    package
        .select_rows(msi::Select::table(table))
        .unwrap()
        .map(|row| {
            (0..row.len())
                .map(|i| match &row[i] {
                    msi::Value::Null => String::new(),
                    msi::Value::Int(n) => n.to_string(),
                    msi::Value::Str(s) => s.clone(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect()
}

#[test]
fn service_tables_match_wixl() {
    let mut spec = test_spec(&tmp("payload-service"));
    spec.service = Some(service_spec(ServiceStart::Auto));
    let out = tmp("service.msi");
    build(&spec, &out).unwrap();
    let mut package = msi::Package::open(fs::File::open(&out).unwrap()).unwrap();

    // Expected rows are wixl's output for <ServiceInstall Type="ownProcess"
    // Start="auto" ErrorControl="normal"> and <ServiceControl Start="install"
    // Stop="both" Remove="uninstall" Wait="yes">, on the component whose
    // KeyPath is the main executable (c1: components follow sorted dests,
    // and data/readme.txt sorts first).
    assert_eq!(
        rows(&mut package, "ServiceInstall"),
        vec!["AppService\tfoo\tFoo\t16\t2\t1\t\t\t\t\t--service\tc1\tFoo service"]
    );
    assert_eq!(
        rows(&mut package, "ServiceControl"),
        vec!["AppServiceControl\tfoo\t163\t\t1\tc1"]
    );
    let seq = rows(&mut package, "InstallExecuteSequence");
    for expected in [
        "StopServices\tVersionNT\t1900",
        "DeleteServices\tVersionNT\t2000",
        "InstallServices\tVersionNT\t5800",
        "StartServices\tVersionNT\t5900",
    ] {
        assert!(seq.contains(&expected.to_string()), "{expected}: {seq:?}");
    }

    // A disabled service cannot be started: StartServices would fail the
    // install, so the Start-on-install bit (0x1) is dropped.
    spec.service = Some(service_spec(ServiceStart::Disabled));
    build(&spec, &out).unwrap();
    let mut package = msi::Package::open(fs::File::open(&out).unwrap()).unwrap();
    assert_eq!(
        rows(&mut package, "ServiceControl"),
        vec!["AppServiceControl\tfoo\t162\t\t1\tc1"]
    );
    assert!(rows(&mut package, "ServiceInstall")[0].contains("\t16\t4\t1\t"));

    // No service: no service tables at all.
    spec.service = None;
    build(&spec, &out).unwrap();
    let package = msi::Package::open(fs::File::open(&out).unwrap()).unwrap();
    assert!(!package.has_table("ServiceInstall"));
    assert!(!package.has_table("ServiceControl"));
}

/// CLI payloads are only usable once their directory is on PATH. The row is
/// wixl's for <Environment Name="PATH" Value="[INSTALLDIR]" Part="last"
/// Action="set" System="yes" Permanent="no"> on the main executable's
/// component (c1, as in service_tables_match_wixl), plus the `-` flag wixl
/// omits: without it msiexec leaves the entry on PATH after uninstall
/// (verified on Windows 11).
#[test]
fn add_to_path_tables() {
    let mut spec = test_spec(&tmp("payload-path"));
    spec.add_to_path = true;
    let out = tmp("path.msi");
    build(&spec, &out).unwrap();
    let mut package = msi::Package::open(fs::File::open(&out).unwrap()).unwrap();
    assert_eq!(
        rows(&mut package, "Environment"),
        vec!["PathEnv\t=-*PATH\t[~];[INSTALLDIR]\tc1"]
    );
    let seq = rows(&mut package, "InstallExecuteSequence");
    for expected in [
        "RemoveEnvironmentStrings\t\t3300",
        "WriteEnvironmentStrings\t\t5200",
    ] {
        assert!(seq.contains(&expected.to_string()), "{expected}: {seq:?}");
    }

    spec.add_to_path = false;
    build(&spec, &out).unwrap();
    let package = msi::Package::open(fs::File::open(&out).unwrap()).unwrap();
    assert!(!package.has_table("Environment"));
}

#[test]
fn invalid_service_name_is_rejected() {
    let mut spec = test_spec(&tmp("payload-service-name"));
    for bad in ["", "a/b", "a\\b", "[PROP]", &"x".repeat(257)] {
        let mut service = service_spec(ServiceStart::Auto);
        service.name = bad.to_string();
        spec.service = Some(service);
        let err = build(&spec, &tmp("service-name.msi")).unwrap_err();
        assert!(
            matches!(err, embala_msi::Error::InvalidServiceName(_)),
            "{bad:?}: {err}"
        );
    }
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
    }
    None
}

/// `ServiceSpec::executable` puts the service on another payload file's
/// component than the shortcut's, so a UI can own the Start-menu entry while
/// an agent owns the service.
#[test]
fn service_executable_selects_its_own_component() {
    let payload = tmp("payload-service-exe");
    let mut spec = test_spec(&payload);
    fs::write(payload.join("agent.exe"), PAYLOAD_EXE).unwrap();
    spec.files.push(FileSpec {
        src: payload.join("agent.exe"),
        dest: "agent.exe".to_string(),
    });
    let mut service = service_spec(ServiceStart::Auto);
    service.executable = Some("agent.exe".to_string());
    spec.service = Some(service);
    let out = tmp("service-exe.msi");
    build(&spec, &out).unwrap();
    let mut package = msi::Package::open(fs::File::open(&out).unwrap()).unwrap();

    // File rows: File, Component_, FileName (short|long), ...
    let component_of = |package: &mut msi::Package<fs::File>, long_name: &str| {
        rows(package, "File")
            .into_iter()
            .map(|r| r.split('\t').map(str::to_string).collect::<Vec<_>>())
            .find(|cols| cols[2].ends_with(&format!("|{long_name}")))
            .map(|cols| cols[1].clone())
            .unwrap_or_else(|| panic!("no File row for {long_name}"))
    };
    let agent = component_of(&mut package, "agent.exe");
    let shortcut_target = component_of(&mut package, "hello.exe");
    assert_ne!(agent, shortcut_target);
    let install = rows(&mut package, "ServiceInstall");
    assert_eq!(install.len(), 1);
    assert_eq!(install[0].split('\t').nth(11), Some(agent.as_str()));
    assert_eq!(
        rows(&mut package, "ServiceControl"),
        vec![format!("AppServiceControl\tfoo\t163\t\t1\t{agent}")]
    );
    // The shortcut still targets the main executable.
    let shortcut = rows(&mut package, "Shortcut");
    let hello_key = rows(&mut package, "File")
        .into_iter()
        .find(|r| r.contains("|hello.exe"))
        .map(|r| r.split('\t').next().unwrap().to_string())
        .unwrap();
    assert!(
        shortcut[0].contains(&format!("[#{hello_key}]")),
        "{shortcut:?}"
    );

    // An executable that is not a payload file is rejected.
    spec.service.as_mut().unwrap().executable = Some("missing.exe".to_string());
    let err = build(&spec, &out).unwrap_err();
    assert!(
        matches!(err, embala_msi::Error::ServiceExecutableNotFound(_)),
        "unexpected error: {err}"
    );
}

/// The 0.8.0 bug: File.Version was always NULL, so on a MajorUpgrade msiexec
/// saw every installed versioned file as newer than the incoming
/// "unversioned" one, skipped its component ("higher versioned keyfile
/// exists"), and the early RemoveExistingProducts then deleted it. A PE with
/// a version resource must carry its file version; anything else stays NULL.
#[test]
fn versioned_pe_files_carry_file_version() {
    let payload = tmp("payload-version");
    let mut spec = test_spec(&payload);
    fs::write(payload.join("app.exe"), versioned_pe([1, 2, 3, 4], &[1033])).unwrap();
    spec.files.push(FileSpec {
        src: payload.join("app.exe"),
        dest: "app.exe".to_string(),
    });
    let out = tmp("version.msi");
    build(&spec, &out).unwrap();
    let mut package = msi::Package::open(fs::File::open(&out).unwrap()).unwrap();
    // File, Component_, FileName, FileSize, Version, Language, ...: sorted by
    // dest, so app.exe, data/readme.txt, hello.exe (a fake "MZ" PE).
    let files: Vec<Vec<String>> = rows(&mut package, "File")
        .into_iter()
        .map(|r| r.split('\t').map(str::to_string).collect())
        .collect();
    let version_of = |name: &str| {
        let row = files
            .iter()
            .find(|cols| cols[2].ends_with(&format!("|{name}")))
            .unwrap();
        (row[4].clone(), row[5].clone())
    };
    assert_eq!(version_of("app.exe"), ("1.2.3.4".into(), "1033".into()));
    assert_eq!(version_of("readme.txt"), (String::new(), String::new()));
    assert_eq!(version_of("hello.exe"), (String::new(), String::new()));
}

/// `File.Language` holds 20 characters; five language ids would fail the
/// whole build, so Language is NULL and the version is kept.
#[test]
fn many_languages_keep_the_version() {
    let payload = tmp("payload-languages");
    let mut spec = test_spec(&payload);
    let pe = versioned_pe([1, 2, 3, 4], &[1033, 1031, 1036, 1040, 3082]);
    fs::write(payload.join("app.exe"), pe).unwrap();
    spec.files.push(FileSpec {
        src: payload.join("app.exe"),
        dest: "app.exe".to_string(),
    });
    let out = tmp("languages.msi");
    build(&spec, &out).unwrap();
    let mut package = msi::Package::open(fs::File::open(&out).unwrap()).unwrap();
    let row = rows(&mut package, "File")
        .into_iter()
        .find(|r| r.contains("|app.exe"))
        .unwrap();
    let cols: Vec<&str> = row.split('\t').collect();
    assert_eq!((cols[4], cols[5]), ("1.2.3.4", ""));
}

fn run_value(value: RegistryData) -> RegistryValue {
    RegistryValue {
        root: RegistryRoot::Hklm,
        key: r"Software\Microsoft\Windows\CurrentVersion\Run".to_string(),
        name: Some("Embala Hello".to_string()),
        value,
    }
}

/// Registry values live on their own component (stable GUID, registry
/// KeyPath, in the main feature), so msiexec writes them on install and
/// removes them with the product.
#[test]
fn registry_values_get_their_own_component() {
    let mut spec = test_spec(&tmp("payload-registry"));
    spec.registry = vec![
        run_value(RegistryData::String(
            r#""[INSTALLDIR]hello.exe""#.to_string(),
        )),
        RegistryValue {
            key: r"Software\Carlos Hernandez\hello".to_string(),
            name: Some("Level".to_string()),
            ..run_value(RegistryData::Integer(7))
        },
        RegistryValue {
            key: r"Software\Carlos Hernandez\hello".to_string(),
            name: None,
            ..run_value(RegistryData::String("#not a number".to_string()))
        },
    ];
    let out = tmp("registry.msi");
    build(&spec, &out).unwrap();
    let mut package = msi::Package::open(fs::File::open(&out).unwrap()).unwrap();
    let registry = rows(&mut package, "Registry");
    for expected in [
        "reg0\t2\tSoftware\\Microsoft\\Windows\\CurrentVersion\\Run\tEmbala Hello\t\"[INSTALLDIR]hello.exe\"\tRegistryComponent",
        "reg1\t2\tSoftware\\Carlos Hernandez\\hello\tLevel\t#7\tRegistryComponent",
        // `##` keeps a leading `#` a literal REG_SZ.
        "reg2\t2\tSoftware\\Carlos Hernandez\\hello\t\t##not a number\tRegistryComponent",
    ] {
        assert!(
            registry.contains(&expected.to_string()),
            "{expected}: {registry:?}"
        );
    }
    assert!(
        rows(&mut package, "Component").contains(
            &"RegistryComponent\t{AFBA0C22-3E60-5CB7-9481-41A8F7DC221C}\tINSTALLDIR\t260\t\treg0"
                .to_string()
        )
    );
    assert!(
        rows(&mut package, "FeatureComponents")
            .contains(&"MainFeature\tRegistryComponent".to_string())
    );

    spec.registry.clear();
    build(&spec, &out).unwrap();
    let mut package = msi::Package::open(fs::File::open(&out).unwrap()).unwrap();
    assert!(
        !rows(&mut package, "Component")
            .iter()
            .any(|r| r.starts_with("RegistryComponent"))
    );
}

#[test]
fn invalid_registry_values_are_rejected() {
    let mut spec = test_spec(&tmp("payload-registry-bad"));
    let ok = || run_value(RegistryData::String("x".to_string()));
    let cases = [
        RegistryValue {
            key: String::new(),
            ..ok()
        },
        RegistryValue {
            key: r"\Software\x".to_string(),
            ..ok()
        },
        RegistryValue {
            key: r"Software\\x".to_string(),
            ..ok()
        },
        RegistryValue {
            name: Some(String::new()),
            ..ok()
        },
        run_value(RegistryData::String(String::new())),
        run_value(RegistryData::String("a[~]b".to_string())),
    ];
    for bad in cases {
        spec.registry = vec![bad.clone()];
        let err = build(&spec, &tmp("registry-bad.msi")).unwrap_err();
        assert!(
            matches!(err, embala_msi::Error::InvalidRegistry { .. }),
            "{bad:?}: {err}"
        );
    }
    // Registry paths are case-insensitive, so this is a duplicate.
    let mut upper = ok();
    upper.key = upper.key.to_uppercase();
    spec.registry = vec![ok(), upper];
    let err = build(&spec, &tmp("registry-bad.msi")).unwrap_err();
    assert!(
        matches!(err, embala_msi::Error::InvalidRegistry { .. }),
        "{err}"
    );
}
