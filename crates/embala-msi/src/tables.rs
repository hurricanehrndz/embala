//! MSI table schemas and rows: the subset of the Windows Installer schema
//! needed to install, shortcut, and cleanly uninstall a per-machine app.
//
// Portions ported from deno desktop (cli/tools/desktop.rs),
// Copyright 2018-2026 the Deno authors, MIT license.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, Write};
use std::path::PathBuf;

use msi::{Category, Column, Insert, Value};

use crate::{Error, MsiSpec, Result, guids};

/// Name of the CFB stream holding the embedded cabinet. The Media table
/// references it as `#embala.cab` (the `#` marks an internal stream).
pub(crate) const CAB_STREAM: &str = "embala.cab";

/// A staged file destined for both the embedded cabinet and the `File` table.
pub(crate) struct StagedFile {
    /// `File` primary key; also the file's name inside the cabinet.
    pub key: String,
    /// One component per file; the file is its KeyPath.
    pub component: String,
    /// `short|long` FileName value.
    pub file_name: String,
    pub size: i32,
    pub src: PathBuf,
    pub dir_id: String,
    pub dest: String,
}

pub(crate) struct Staged {
    /// `(id, parent, DefaultDir)` rows for nested dest directories.
    pub extra_dirs: Vec<(String, String, String)>,
    /// In `File.Sequence` order (sorted by dest).
    pub files: Vec<StagedFile>,
    /// `File` key of the main executable (the shortcut target).
    pub shortcut_file_key: String,
    /// `short|long` Name for the Start-menu shortcut.
    pub shortcut_name: String,
}

/// Lowercase base36 encoding of a counter, used to mint unique 8.3 short names.
fn base36(mut n: u32) -> String {
    if n == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// Mint a unique DOS 8.3 short name for the `DefaultDir` / `FileName`
/// "short|long" syntax. The long name carries the real name; the short name
/// only has to be a unique, valid 8.3 token, minted from a global counter:
/// `F<base36>.<EXT>` for files, `D<base36>` for directories.
fn short_name(counter: u32, long: &str, is_dir: bool) -> String {
    let token = base36(counter).to_uppercase();
    if is_dir {
        format!("D{token}")
    } else {
        let ext: String = long
            .rsplit_once('.')
            .map(|(_, e)| e)
            .unwrap_or("")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(3)
            .collect::<String>()
            .to_uppercase();
        if ext.is_empty() {
            format!("F{token}")
        } else {
            format!("F{token}.{ext}")
        }
    }
}

/// Resolve the spec's file list into Directory/Component/File material:
/// deterministic ids (sorted by dest), sizes, short names.
pub(crate) fn stage(spec: &MsiSpec) -> Result<Staged> {
    let mut entries: Vec<(&str, &PathBuf)> = spec
        .files
        .iter()
        .map(|f| (f.dest.as_str(), &f.src))
        .collect();
    entries.sort_by_key(|(dest, _)| *dest);

    // Every ancestor directory of every dest, keyed by its /-joined path.
    let mut rel_dirs = BTreeSet::<String>::new();
    for (dest, _) in &entries {
        let mut prefix = String::new();
        for segment in dest
            .split('/')
            .rev()
            .skip(1)
            .collect::<Vec<_>>()
            .iter()
            .rev()
        {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            rel_dirs.insert(prefix.clone());
        }
    }
    let mut dir_ids = BTreeMap::<String, String>::new();
    dir_ids.insert(String::new(), "INSTALLDIR".to_string());
    for (i, dir) in rel_dirs.iter().enumerate() {
        dir_ids.insert(dir.clone(), format!("d{i}"));
    }

    let mut short_counter: u32 = 1; // 1 was spent on INSTALLDIR's own name
    let mut extra_dirs = Vec::new();
    for dir in &rel_dirs {
        let (parent, name) = match dir.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", dir.as_str()),
        };
        short_counter += 1;
        extra_dirs.push((
            dir_ids[dir].clone(),
            dir_ids[parent].clone(),
            format!("{}|{}", short_name(short_counter, name, true), name),
        ));
    }

    let mut files = Vec::new();
    for (i, (dest, src)) in entries.iter().enumerate() {
        let (dir, name) = match dest.rsplit_once('/') {
            Some((dir, name)) => (dir, name),
            None => ("", *dest),
        };
        let size = std::fs::metadata(src)?.len();
        let size = i32::try_from(size).map_err(|_| Error::FileTooLarge((*src).clone()))?;
        short_counter += 1;
        files.push(StagedFile {
            key: format!("f{i}"),
            component: format!("c{i}"),
            file_name: format!("{}|{}", short_name(short_counter, name, false), name),
            size,
            src: (*src).clone(),
            dir_id: dir_ids[dir].clone(),
            dest: (*dest).to_string(),
        });
    }

    let shortcut_file_key = files
        .iter()
        .find(|f| f.dest == spec.main_executable)
        .map(|f| f.key.clone())
        .ok_or_else(|| Error::MainExecutableNotFound(spec.main_executable.clone()))?;
    short_counter += 1;
    let shortcut_name = format!(
        "{}|{}",
        short_name(short_counter, &spec.display_name, false),
        spec.display_name
    );

    Ok(Staged {
        extra_dirs,
        files,
        shortcut_file_key,
        shortcut_name,
    })
}

/// Create and populate every table.
pub(crate) fn write<F: Read + Write + Seek>(
    package: &mut msi::Package<F>,
    spec: &MsiSpec,
    staged: &Staged,
) -> Result<()> {
    create_schemas(package)?;

    // --- Directory ----------------------------------------------------------
    let mut directory_rows = vec![
        vec![
            Value::Str("TARGETDIR".to_string()),
            Value::Null,
            Value::Str("SourceDir".to_string()),
        ],
        vec![
            Value::Str("ProgramFiles64Folder".to_string()),
            Value::Str("TARGETDIR".to_string()),
            Value::Str(".".to_string()),
        ],
        vec![
            Value::Str("INSTALLDIR".to_string()),
            Value::Str("ProgramFiles64Folder".to_string()),
            Value::Str(format!(
                "{}|{}",
                short_name(1, &spec.display_name, true),
                spec.display_name
            )),
        ],
        // Hosts the app shortcut directly — no subfolder, so no RemoveFolder
        // row is needed on uninstall.
        vec![
            Value::Str("ProgramMenuFolder".to_string()),
            Value::Str("TARGETDIR".to_string()),
            Value::Str(".".to_string()),
        ],
    ];
    for (id, parent, default_dir) in &staged.extra_dirs {
        directory_rows.push(vec![
            Value::Str(id.clone()),
            Value::Str(parent.clone()),
            Value::Str(default_dir.clone()),
        ]);
    }
    package.insert_rows(Insert::into("Directory").rows(directory_rows))?;

    // --- Component: one per file (64-bit, KeyPath = the file) + the shortcut
    // component, whose KeyPath is an HKCU registry value because shortcuts
    // cannot be KeyPaths.
    const ATTR_64BIT: i32 = 256; // msidbComponentAttributes64bit
    const ATTR_REGISTRY_KEYPATH: i32 = 4; // msidbComponentAttributesRegistryKeyPath
    let mut component_rows: Vec<Vec<Value>> = staged
        .files
        .iter()
        .map(|f| {
            vec![
                Value::Str(f.component.clone()),
                Value::Str(guids::component_guid(&spec.identifier, &f.dest)),
                Value::Str(f.dir_id.clone()),
                Value::Int(ATTR_64BIT),
                Value::Null,
                Value::Str(f.key.clone()),
            ]
        })
        .collect();
    component_rows.push(vec![
        Value::Str("ShortcutComponent".to_string()),
        Value::Str(guids::shortcut_component_guid(&spec.identifier)),
        Value::Str("ProgramMenuFolder".to_string()),
        Value::Int(ATTR_64BIT | ATTR_REGISTRY_KEYPATH),
        Value::Null,
        Value::Str("ShortcutRegistry".to_string()),
    ]);
    package.insert_rows(Insert::into("Component").rows(component_rows))?;

    // --- File (sequence numbers match cabinet order, 1-based) ---------------
    const FILE_VITAL: i32 = 512; // msidbFileAttributesVital
    let file_rows: Vec<Vec<Value>> = staged
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            vec![
                Value::Str(f.key.clone()),
                Value::Str(f.component.clone()),
                Value::Str(f.file_name.clone()),
                Value::Int(f.size),
                Value::Null, // Version (not a tracked-version file)
                Value::Null, // Language
                Value::Int(FILE_VITAL),
                Value::Int(1 + i as i32),
            ]
        })
        .collect();
    package.insert_rows(Insert::into("File").rows(file_rows))?;

    // --- Feature + FeatureComponents (single feature) -----------------------
    package.insert_rows(Insert::into("Feature").row(vec![
        Value::Str("MainFeature".to_string()),
        Value::Null,
        Value::Str(spec.display_name.clone()),
        Value::Null,
        Value::Int(1), // Display
        Value::Int(1), // Level
        Value::Str("INSTALLDIR".to_string()),
        Value::Int(0),
    ]))?;
    let mut feature_component_rows: Vec<Vec<Value>> = staged
        .files
        .iter()
        .map(|f| {
            vec![
                Value::Str("MainFeature".to_string()),
                Value::Str(f.component.clone()),
            ]
        })
        .collect();
    feature_component_rows.push(vec![
        Value::Str("MainFeature".to_string()),
        Value::Str("ShortcutComponent".to_string()),
    ]);
    package.insert_rows(Insert::into("FeatureComponents").rows(feature_component_rows))?;

    // --- Media: one disk, everything in the embedded cabinet ----------------
    package.insert_rows(Insert::into("Media").row(vec![
        Value::Int(1),
        Value::Int(staged.files.len() as i32), // LastSequence
        Value::Null,
        Value::Str(format!("#{CAB_STREAM}")),
        Value::Null,
        Value::Null,
    ]))?;

    // --- Shortcut (non-advertised; [#key] resolves to the installed path) ---
    package.insert_rows(Insert::into("Shortcut").row(vec![
        Value::Str("AppShortcut".to_string()),
        Value::Str("ProgramMenuFolder".to_string()),
        Value::Str(staged.shortcut_name.clone()),
        Value::Str("ShortcutComponent".to_string()),
        Value::Str(format!("[#{}]", staged.shortcut_file_key)),
        Value::Null,                          // Arguments
        Value::Null,                          // Description
        Value::Null,                          // Hotkey
        Value::Null,                          // Icon_
        Value::Null,                          // IconIndex
        Value::Null,                          // ShowCmd
        Value::Str("INSTALLDIR".to_string()), // WkDir
    ]))?;

    // --- Registry: the shortcut component's KeyPath -------------------------
    const HKCU: i32 = 1;
    package.insert_rows(Insert::into("Registry").row(vec![
        Value::Str("ShortcutRegistry".to_string()),
        Value::Int(HKCU),
        Value::Str(format!("Software\\{}\\{}", spec.publisher, spec.name)),
        Value::Str("installed".to_string()),
        Value::Str("#1".to_string()), // DWORD 1
        Value::Str("ShortcutComponent".to_string()),
    ]))?;

    // --- Property ------------------------------------------------------------
    let mut property_rows = vec![
        vec![
            Value::Str("ProductCode".to_string()),
            Value::Str(guids::product_code(&spec.identifier, &spec.version)),
        ],
        vec![
            Value::Str("ProductName".to_string()),
            Value::Str(spec.display_name.clone()),
        ],
        vec![
            Value::Str("ProductVersion".to_string()),
            Value::Str(spec.version.clone()),
        ],
        vec![
            Value::Str("ProductLanguage".to_string()),
            Value::Str("1033".to_string()),
        ],
        vec![
            Value::Str("Manufacturer".to_string()),
            Value::Str(spec.publisher.clone()),
        ],
        vec![
            Value::Str("UpgradeCode".to_string()),
            Value::Str(guids::upgrade_code(&spec.identifier)),
        ],
        // Per-machine install (into Program Files).
        vec![
            Value::Str("ALLUSERS".to_string()),
            Value::Str("1".to_string()),
        ],
        // Add/Remove Programs metadata.
        vec![
            Value::Str("ARPCOMMENTS".to_string()),
            Value::Str(spec.description.clone()),
        ],
        // FindRelatedProducts writes into these; marking them secure lets the
        // elevated server side of the install see the values.
        vec![
            Value::Str("SecureCustomProperties".to_string()),
            Value::Str("OLDPRODUCTFOUND;NEWERVERSIONDETECTED".to_string()),
        ],
    ];
    if let Some(homepage) = &spec.homepage {
        property_rows.push(vec![
            Value::Str("ARPURLINFOABOUT".to_string()),
            Value::Str(homepage.clone()),
        ]);
    }
    package.insert_rows(Insert::into("Property").rows(property_rows))?;

    // --- Upgrade: MajorUpgrade detection (attribute values mirror what wixl
    // emits for <MajorUpgrade/>) ----------------------------------------------
    const UPGRADE_MIGRATE_FEATURES: i32 = 1; // msidbUpgradeAttributesMigrateFeatures
    const UPGRADE_ONLY_DETECT: i32 = 2; // msidbUpgradeAttributesOnlyDetect
    let upgrade_code = guids::upgrade_code(&spec.identifier);
    package.insert_rows(Insert::into("Upgrade").rows(vec![
        // Any older version (VersionMax is exclusive, so not this one):
        // RemoveExistingProducts uninstalls it, migrating feature states.
        vec![
            Value::Str(upgrade_code.clone()),
            Value::Null, // VersionMin: no lower bound
            Value::Str(spec.version.clone()),
            Value::Null, // Language: any
            Value::Int(UPGRADE_MIGRATE_FEATURES),
            Value::Null, // Remove: all features
            Value::Str("OLDPRODUCTFOUND".to_string()),
        ],
        // Any strictly newer version (VersionMin is exclusive): detect only,
        // so the LaunchCondition below can refuse the downgrade.
        vec![
            Value::Str(upgrade_code),
            Value::Str(spec.version.clone()),
            Value::Null, // VersionMax: no upper bound
            Value::Null,
            Value::Int(UPGRADE_ONLY_DETECT),
            Value::Null,
            Value::Str("NEWERVERSIONDETECTED".to_string()),
        ],
    ]))?;

    // --- LaunchCondition: block downgrades ------------------------------------
    package.insert_rows(Insert::into("LaunchCondition").row(vec![
        Value::Str("NOT NEWERVERSIONDETECTED".to_string()),
        Value::Str("A newer version of [ProductName] is already installed.".to_string()),
    ]))?;

    // --- Action sequences (standard MSI sequence numbers) --------------------
    let exec_seq: &[(&str, i32)] = &[
        ("FindRelatedProducts", 25),
        ("LaunchConditions", 100),
        ("CostInitialize", 800),
        ("FileCost", 900),
        ("CostFinalize", 1000),
        ("MigrateFeatureStates", 1200),
        ("InstallValidate", 1400),
        // Uninstall the old version up front, before the new files go down.
        ("RemoveExistingProducts", 1401),
        ("InstallInitialize", 1500),
        ("ProcessComponents", 1600),
        ("UnpublishFeatures", 1800),
        ("RemoveRegistryValues", 2600),
        ("RemoveShortcuts", 3200),
        ("RemoveFiles", 3500),
        ("RemoveFolders", 3600),
        ("CreateFolders", 3700),
        ("InstallFiles", 4000),
        ("CreateShortcuts", 4500),
        ("WriteRegistryValues", 5000),
        ("RegisterProduct", 6100),
        ("PublishFeatures", 6300),
        ("PublishProduct", 6400),
        ("InstallFinalize", 6600),
    ];
    package.insert_rows(
        Insert::into("InstallExecuteSequence").rows(
            exec_seq
                .iter()
                .map(|(action, seq)| {
                    vec![
                        Value::Str(action.to_string()),
                        Value::Null,
                        Value::Int(*seq),
                    ]
                })
                .collect(),
        ),
    )?;
    // Minimal UI sequence: cost the install, then hand off to the execute
    // sequence. No Dialog/Control tables — basic-UI msiexec runs use this.
    let ui_seq: &[(&str, i32)] = &[
        ("FindRelatedProducts", 25),
        ("LaunchConditions", 100),
        ("CostInitialize", 800),
        ("FileCost", 900),
        ("CostFinalize", 1000),
        ("MigrateFeatureStates", 1200),
        ("ExecuteAction", 1300),
    ];
    package.insert_rows(
        Insert::into("InstallUISequence").rows(
            ui_seq
                .iter()
                .map(|(action, seq)| {
                    vec![
                        Value::Str(action.to_string()),
                        Value::Null,
                        Value::Int(*seq),
                    ]
                })
                .collect(),
        ),
    )?;

    Ok(())
}

fn create_schemas<F: Read + Write + Seek>(package: &mut msi::Package<F>) -> Result<()> {
    package.create_table(
        "Directory",
        vec![
            Column::build("Directory").primary_key().id_string(72),
            Column::build("Directory_Parent").nullable().id_string(72),
            Column::build("DefaultDir")
                .category(Category::DefaultDir)
                .string(255),
        ],
    )?;
    package.create_table(
        "Component",
        vec![
            Column::build("Component").primary_key().id_string(72),
            Column::build("ComponentId")
                .nullable()
                .category(Category::Guid)
                .string(38),
            Column::build("Directory_").id_string(72),
            Column::build("Attributes").int16(),
            Column::build("Condition")
                .nullable()
                .category(Category::Condition)
                .string(255),
            Column::build("KeyPath").nullable().id_string(72),
        ],
    )?;
    package.create_table(
        "Feature",
        vec![
            Column::build("Feature").primary_key().id_string(38),
            Column::build("Feature_Parent").nullable().id_string(38),
            Column::build("Title").nullable().text_string(64),
            Column::build("Description").nullable().text_string(255),
            Column::build("Display").nullable().int16(),
            Column::build("Level").int16(),
            Column::build("Directory_").nullable().id_string(72),
            Column::build("Attributes").int16(),
        ],
    )?;
    package.create_table(
        "FeatureComponents",
        vec![
            Column::build("Feature_").primary_key().id_string(38),
            Column::build("Component_").primary_key().id_string(72),
        ],
    )?;
    package.create_table(
        "File",
        vec![
            Column::build("File").primary_key().id_string(72),
            Column::build("Component_").id_string(72),
            Column::build("FileName")
                .category(Category::Filename)
                .string(255),
            Column::build("FileSize").int32(),
            Column::build("Version")
                .nullable()
                .category(Category::Version)
                .string(72),
            Column::build("Language").nullable().string(20),
            Column::build("Attributes").nullable().int16(),
            Column::build("Sequence").int16(),
        ],
    )?;
    package.create_table(
        "Media",
        vec![
            Column::build("DiskId").primary_key().int16(),
            Column::build("LastSequence").int16(),
            Column::build("DiskPrompt").nullable().text_string(64),
            Column::build("Cabinet")
                .nullable()
                .category(Category::Cabinet)
                .string(255),
            Column::build("VolumeLabel").nullable().text_string(32),
            Column::build("Source")
                .nullable()
                .category(Category::Property)
                .string(72),
        ],
    )?;
    package.create_table(
        "Property",
        vec![
            Column::build("Property").primary_key().id_string(72),
            Column::build("Value").text_string(0),
        ],
    )?;
    package.create_table(
        "Shortcut",
        vec![
            Column::build("Shortcut").primary_key().id_string(72),
            Column::build("Directory_").id_string(72),
            Column::build("Name")
                .category(Category::Filename)
                .string(128),
            Column::build("Component_").id_string(72),
            Column::build("Target")
                .category(Category::Shortcut)
                .string(72),
            Column::build("Arguments")
                .nullable()
                .category(Category::Formatted)
                .string(255),
            Column::build("Description").nullable().text_string(255),
            Column::build("Hotkey").nullable().int16(),
            Column::build("Icon_").nullable().id_string(72),
            Column::build("IconIndex").nullable().int16(),
            Column::build("ShowCmd").nullable().int16(),
            Column::build("WkDir").nullable().id_string(72),
        ],
    )?;
    package.create_table(
        "Registry",
        vec![
            Column::build("Registry").primary_key().id_string(72),
            Column::build("Root").int16(),
            Column::build("Key").category(Category::RegPath).string(255),
            Column::build("Name")
                .nullable()
                .category(Category::Formatted)
                .string(255),
            Column::build("Value")
                .nullable()
                .category(Category::Formatted)
                .string(0),
            Column::build("Component_").id_string(72),
        ],
    )?;
    package.create_table(
        "Upgrade",
        vec![
            Column::build("UpgradeCode")
                .primary_key()
                .category(Category::Guid)
                .string(38),
            Column::build("VersionMin")
                .primary_key()
                .nullable()
                .category(Category::Text)
                .string(20),
            Column::build("VersionMax")
                .primary_key()
                .nullable()
                .category(Category::Text)
                .string(20),
            Column::build("Language")
                .primary_key()
                .nullable()
                .category(Category::Text)
                .string(255),
            Column::build("Attributes").primary_key().int32(),
            Column::build("Remove")
                .nullable()
                .category(Category::Formatted)
                .string(255),
            Column::build("ActionProperty")
                .category(Category::UpperCase)
                .string(72),
        ],
    )?;
    package.create_table(
        "LaunchCondition",
        vec![
            Column::build("Condition")
                .primary_key()
                .category(Category::Condition)
                .string(255),
            Column::build("Description").text_string(255),
        ],
    )?;
    for table in ["InstallExecuteSequence", "InstallUISequence"] {
        package.create_table(
            table,
            vec![
                Column::build("Action").primary_key().id_string(72),
                Column::build("Condition")
                    .nullable()
                    .category(Category::Condition)
                    .string(255),
                Column::build("Sequence").nullable().int16(),
            ],
        )?;
    }
    Ok(())
}
