//! Directory `src` entries: a tree installs under its `dest`, and symlinks
//! inside it ship as symlinks. `.app` bundles depend on both (frameworks use
//! `Versions/Current` links), so a tree that silently flattens or follows
//! links installs a broken bundle.
//!
//! Symlink encodings were pinned against `pkgbuild` on macOS 27.2: an embala
//! build of the same tree matched pkgbuild's cpio entries and BOM path
//! records byte-for-byte (mtime aside) and produced identical `lsbom` output.

use std::io::{Cursor, Read as _};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use apple_bom::ParsedBom;
use apple_xar::reader::XarReader;
use embala_pkg::{DirSpec, Error, FileSpec, PkgSpec, ProductSpec};

fn spec(files: Vec<FileSpec>) -> PkgSpec {
    PkgSpec {
        name: "tree".to_string(),
        identifier: "t.tree".to_string(),
        version: "1.0.0".to_string(),
        install_location: "/usr/local".to_string(),
        files,
        directories: vec![],
        scripts: None,
    }
}

fn product(components: Vec<PkgSpec>) -> ProductSpec {
    ProductSpec {
        name: "tree".to_string(),
        display_name: "Tree".to_string(),
        enable_user_home: false,
        components,
    }
}

/// Build `spec` as the only component of a product.
fn build(spec: &PkgSpec, out: &Path) -> embala_pkg::Result<()> {
    embala_pkg::build(&product(vec![spec.clone()]), out)
}

fn entry(src: &Path, dest: &str) -> FileSpec {
    FileSpec {
        src: src.to_path_buf(),
        dest: dest.to_string(),
        mode: None,
    }
}

fn write(path: &Path, bytes: &[u8], mode: u32) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// The pin fixture's shape: nested dirs, an empty dir, a +x file, and
/// relative, directory, absolute, dangling, and nested symlinks.
fn fixture_tree(root: &Path) {
    write(&root.join("bin/tool"), b"#!/bin/sh\necho hi\n", 0o755);
    write(&root.join("share/doc/readme.txt"), b"hello\n", 0o644);
    std::fs::create_dir_all(root.join("share/empty")).unwrap();
    std::fs::create_dir_all(root.join("share/doc/sub")).unwrap();
    std::fs::create_dir_all(root.join("lib")).unwrap();
    symlink("../share/doc/readme.txt", root.join("bin/readme-link")).unwrap();
    symlink("../share/doc", root.join("lib/doclink")).unwrap();
    symlink("/usr/bin/true", root.join("bin/abslink")).unwrap();
    symlink("does-not-exist", root.join("bin/dangling")).unwrap();
    symlink("../readme.txt", root.join("share/doc/sub/nested")).unwrap();
}

struct Built {
    archive: Vec<u8>,
}

impl Built {
    fn new(spec: &PkgSpec, dir: &Path) -> Self {
        let out = dir.join("out.pkg");
        build(spec, &out).unwrap();
        Self {
            archive: std::fs::read(out).unwrap(),
        }
    }

    fn member(&self, name: &str) -> Vec<u8> {
        let mut reader = XarReader::new(Cursor::new(self.archive.clone())).unwrap();
        reader
            .get_file_data_from_path(&format!("tree.pkg/{name}"))
            .unwrap()
            .unwrap()
    }

    /// `(name, mode, data)` per cpio entry, in archive order, no trailer.
    fn cpio(&self) -> Vec<(String, u32, Vec<u8>)> {
        let mut cpio = Vec::new();
        flate2::read::GzDecoder::new(&self.member("Payload")[..])
            .read_to_end(&mut cpio)
            .unwrap();
        let octal = |b: &[u8]| usize::from_str_radix(std::str::from_utf8(b).unwrap(), 8).unwrap();
        let (mut out, mut at) = (Vec::new(), 0);
        loop {
            let header = &cpio[at..at + 76];
            assert_eq!(&header[..6], b"070707");
            let (mode, namesize, filesize) = (
                octal(&header[18..24]),
                octal(&header[59..65]),
                octal(&header[65..76]),
            );
            let name = String::from_utf8(cpio[at + 76..at + 75 + namesize].to_vec()).unwrap();
            let data_at = at + 76 + namesize;
            if name == "TRAILER!!!" {
                return out;
            }
            out.push((
                name,
                mode as u32,
                cpio[data_at..data_at + filesize].to_vec(),
            ));
            at = data_at + filesize;
        }
    }

    fn cpio_entry(&self, name: &str) -> (u32, Vec<u8>) {
        let (_, mode, data) = self.cpio().into_iter().find(|e| e.0 == name).unwrap();
        (mode, data)
    }

    fn bom_paths(&self) -> Vec<apple_bom::BomPath> {
        ParsedBom::parse(&self.member("Bom"))
            .unwrap()
            .paths()
            .unwrap()
    }
}

#[test]
fn tree_installs_nested_dirs_under_dest() {
    // Why: a directory src is how a whole bundle or prefix ships; every
    // file, nested dir, and empty dir must land under dest with the mode
    // its exec bit implies, and dest's own ancestors must exist.
    let tmp = tempfile::tempdir().unwrap();
    fixture_tree(&tmp.path().join("root"));
    let built = Built::new(
        &spec(vec![entry(&tmp.path().join("root"), "opt/tree")]),
        tmp.path(),
    );
    let modes: Vec<(String, u32)> = built.cpio().into_iter().map(|(n, m, _)| (n, m)).collect();
    for expected in [
        ("./opt", 0o40755),
        ("./opt/tree", 0o40755),
        ("./opt/tree/bin/tool", 0o100755),
        ("./opt/tree/share/doc/readme.txt", 0o100644),
        ("./opt/tree/share/doc/sub", 0o40755),
        ("./opt/tree/share/empty", 0o40755),
    ] {
        assert!(
            modes.contains(&(expected.0.to_string(), expected.1)),
            "{expected:?} missing from {modes:?}"
        );
    }
    let bom: Vec<String> = built
        .bom_paths()
        .iter()
        .map(|p| p.path().to_string())
        .collect();
    let mut cpio: Vec<String> = modes.into_iter().map(|(n, _)| n).collect();
    cpio.sort();
    let mut bom_sorted = bom.clone();
    bom_sorted.sort();
    assert_eq!(bom_sorted, cpio, "BOM and Payload must list the same tree");
}

#[test]
fn symlinks_ship_as_symlinks_in_cpio_and_bom() {
    // Why: following a link would duplicate a framework's contents or, for
    // dangling and absolute links, fail or capture the build host's files.
    // Encodings are pkgbuild's (macOS 27.2): cpio mode 0o120755 with the
    // bare target as data; BOM size = target length, checksum = cksum of
    // the target.
    let tmp = tempfile::tempdir().unwrap();
    fixture_tree(&tmp.path().join("root"));
    let built = Built::new(
        &spec(vec![entry(&tmp.path().join("root"), ".")]),
        tmp.path(),
    );

    for (name, target) in [
        ("./bin/readme-link", "../share/doc/readme.txt"),
        ("./lib/doclink", "../share/doc"),
        ("./bin/abslink", "/usr/bin/true"),
        ("./bin/dangling", "does-not-exist"),
        ("./share/doc/sub/nested", "../readme.txt"),
    ] {
        assert_eq!(
            built.cpio_entry(name),
            (0o120755, target.as_bytes().to_vec()),
            "{name}"
        );
        let bom = built.bom_paths();
        let path = bom.iter().find(|p| p.path() == name).unwrap();
        assert_eq!(path.file_mode(), 0o120755, "{name}");
        assert_eq!(path.link_name(), Some(target), "{name}");
        assert_eq!(path.size(), target.len(), "{name}");
    }
    // Checksums lsbom printed for pkgbuild's records of the same links.
    let bom = built.bom_paths();
    let crc = |name: &str| bom.iter().find(|p| p.path() == name).unwrap().crc32();
    assert_eq!(crc("./bin/abslink"), Some(1_210_233_159));
    assert_eq!(crc("./bin/dangling"), Some(434_248_690));

    // pkgbuild's full link record: header, NUL-terminated target, then 8
    // zero bytes. lsbom reads that tail; without it the next block shifts.
    let mut record = vec![0x03, 0x01, 0x00, 0x0f, 0xa1, 0xed];
    record.extend([0u8; 8]); // uid, gid
    record.extend(946_684_800u32.to_be_bytes()); // embala's fixed mtime
    record.extend(13u32.to_be_bytes());
    record.push(0x01);
    record.extend(1_210_233_159u32.to_be_bytes());
    record.extend(14u32.to_be_bytes());
    record.extend(b"/usr/bin/true\0");
    record.extend([0u8; 8]);
    let bom = built.member("Bom");
    assert!(
        bom.windows(record.len()).any(|w| w == record),
        "pinned link record missing"
    );
}

#[test]
fn symlinks_count_toward_install_kbytes_like_files() {
    // Why: installKBytes drives Installer's free-space check. pkgbuild
    // (macOS 27.2) charges a link ceil(len(target)/512) blocks: a 600-byte
    // target costs two, a short one one.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    symlink("0".repeat(600), root.join("long")).unwrap();
    symlink("a", root.join("short")).unwrap();
    let built = Built::new(&spec(vec![entry(&root, ".")]), tmp.path());
    let info = String::from_utf8(built.member("PackageInfo")).unwrap();
    // 2 + 1 blocks = 1.5 KiB, rounded up.
    assert!(
        info.contains(r#"<payload numberOfFiles="3" installKBytes="2"/>"#),
        "{info}"
    );
}

#[test]
fn dot_dest_mirrors_the_tree_at_the_install_location() {
    // Why: `dest = "."` is the munkipkg-style "this dir is the install
    // root". A "." must never reach a path: the BOM rejects it and cpio
    // would write `././bin`.
    let tmp = tempfile::tempdir().unwrap();
    fixture_tree(&tmp.path().join("root"));
    let built = Built::new(
        &spec(vec![entry(&tmp.path().join("root"), ".")]),
        tmp.path(),
    );
    let names: Vec<String> = built.cpio().into_iter().map(|e| e.0).collect();
    assert_eq!(names[0], ".");
    assert!(names.contains(&"./bin/tool".to_string()), "{names:?}");
    assert!(
        names.iter().skip(1).all(|n| !n.starts_with("./.")),
        "{names:?}"
    );
    assert!(
        built
            .bom_paths()
            .iter()
            .all(|p| !p.path().starts_with("./."))
    );

    // A file src has no contents to mirror.
    let file = tmp.path().join("root/bin/tool");
    let err = build(&spec(vec![entry(&file, ".")]), &tmp.path().join("x.pkg")).unwrap_err();
    assert!(matches!(err, Error::DotDestNotDir(_)), "{err}");
}

#[test]
fn dot_segments_elsewhere_in_a_dest_are_rejected() {
    // Why: only a lone "." has a meaning; `a/./b` would ship a literal "."
    // path component.
    let tmp = tempfile::tempdir().unwrap();
    write(&tmp.path().join("f"), b"x", 0o644);
    for dest in ["a/./b", "./a", "a/."] {
        let err = build(
            &spec(vec![entry(&tmp.path().join("f"), dest)]),
            &tmp.path().join("x.pkg"),
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidDest(_)), "{dest}: {err}");
    }
}

#[test]
fn a_tree_file_colliding_with_another_entry_is_an_error() {
    // Why: expansion happens after validate(); without a second check the
    // later entry would silently overwrite the tree's file (or a file
    // would replace a directory the tree needs).
    let tmp = tempfile::tempdir().unwrap();
    fixture_tree(&tmp.path().join("root"));
    write(&tmp.path().join("other"), b"other\n", 0o644);
    let tree = entry(&tmp.path().join("root"), ".");
    let out = tmp.path().join("x.pkg");

    let dup = spec(vec![
        tree.clone(),
        entry(&tmp.path().join("other"), "bin/tool"),
    ]);
    let err = build(&dup, &out).unwrap_err();
    assert!(
        matches!(&err, Error::DuplicateDest(d) if d == "bin/tool"),
        "{err}"
    );

    let on_dir = spec(vec![
        tree.clone(),
        entry(&tmp.path().join("other"), "share/doc"),
    ]);
    let err = build(&on_dir, &out).unwrap_err();
    assert!(
        matches!(&err, Error::DestConflict(d) if d == "share/doc"),
        "{err}"
    );

    // A symlink is a leaf too: nothing may install beneath it.
    let under_link = spec(vec![
        tree.clone(),
        entry(&tmp.path().join("other"), "lib/doclink/x"),
    ]);
    let err = build(&under_link, &out).unwrap_err();
    assert!(
        matches!(&err, Error::DestConflict(d) if d == "lib/doclink"),
        "{err}"
    );

    // Directories may overlap: a second tree installed at the first tree's
    // `bin/` merges into it, and a declared directory may set a tree
    // directory's mode.
    let second = tmp.path().join("second");
    write(&second.join("extra"), b"x", 0o755);
    let mut merged = spec(vec![tree, entry(&second, "bin")]);
    merged.directories.push(DirSpec {
        dest: "share/doc".to_string(),
        mode: Some(0o750),
    });
    let built = Built::new(&merged, tmp.path());
    assert_eq!(built.cpio_entry("./bin/extra").0, 0o100755);
    assert_eq!(built.cpio_entry("./share/doc").0, 0o40750);
}

#[test]
fn mode_on_a_directory_src_is_an_error() {
    // Why: one mode cannot fit both a tree's executables and its data
    // files; silently ignoring it would ship modes the config did not ask for.
    let tmp = tempfile::tempdir().unwrap();
    fixture_tree(&tmp.path().join("root"));
    let mut file = entry(&tmp.path().join("root"), "opt/tree");
    file.mode = Some(0o700);
    let err = build(&spec(vec![file]), &tmp.path().join("x.pkg")).unwrap_err();
    assert!(
        matches!(&err, Error::DirSrcMode(d) if d == "opt/tree"),
        "{err}"
    );
}

#[test]
fn nested_symlinked_dirs_are_not_followed_but_a_symlinked_src_is() {
    // Why: following a nested link would duplicate the target's contents
    // and, for a link back up the tree, never terminate. A top-level src
    // symlink keeps its pre-tree meaning (resolve it, ship what it names).
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    write(&root.join("real/file"), b"x", 0o644);
    symlink("real", root.join("alias")).unwrap();
    symlink(".", root.join("loop")).unwrap();
    symlink(&root, tmp.path().join("root-link")).unwrap();

    let built = Built::new(
        &spec(vec![entry(&tmp.path().join("root-link"), ".")]),
        tmp.path(),
    );
    let names: Vec<String> = built.cpio().into_iter().map(|e| e.0).collect();
    assert_eq!(
        names,
        [".", "./alias", "./loop", "./real", "./real/file"],
        "links are leaves; nothing is walked through them"
    );
    assert_eq!(built.cpio_entry("./alias"), (0o120755, b"real".to_vec()));
}

#[test]
fn tree_file_colliding_with_another_component_is_an_error() {
    // Why: validate() compares declared dests only, and a tree's contents are
    // known only once read. Two components writing the same absolute path
    // would leave whichever installs last, and both receipts would claim it.
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    write(&tree.join("LaunchDaemons/foo.plist"), b"<plist/>", 0o644);
    let plist = tmp.path().join("foo.plist");
    write(&plist, b"<plist/>", 0o644);

    let mut whole = spec(vec![entry(&tree, ".")]);
    whole.name = "root".to_string();
    whole.identifier = "t.root".to_string();
    whole.install_location = "/Library".to_string();
    let mut daemon = spec(vec![entry(&plist, "foo.plist")]);
    daemon.name = "daemon".to_string();
    daemon.identifier = "t.daemon".to_string();
    daemon.install_location = "/Library/LaunchDaemons".to_string();

    let out = tmp.path().join("x.pkg");
    let err = embala_pkg::build(&product(vec![whole, daemon]), &out).unwrap_err();
    assert!(
        matches!(&err, Error::ComponentCollision { path, .. } if path == "/Library/LaunchDaemons/foo.plist"),
        "{err}"
    );
}

/// A component named `name` installing `files` at `location`.
fn component(name: &str, location: &str, files: Vec<FileSpec>) -> PkgSpec {
    let mut spec = spec(files);
    spec.name = name.to_string();
    spec.identifier = format!("t.{name}");
    spec.install_location = location.to_string();
    spec
}

#[test]
fn disjoint_dot_trees_may_share_an_install_location() {
    // Why: splitting one staging root into components (bin and share both
    // under /usr/local) is the point of `dest = "."`; only real file
    // overlaps should fail.
    let tmp = tempfile::tempdir().unwrap();
    write(&tmp.path().join("bin/bin/tool"), b"x", 0o755);
    write(&tmp.path().join("share/share/doc"), b"x", 0o644);
    let product = product(vec![
        component(
            "bin",
            "/usr/local",
            vec![entry(&tmp.path().join("bin"), ".")],
        ),
        component(
            "share",
            "/usr/local/",
            vec![entry(&tmp.path().join("share"), ".")],
        ),
    ]);
    embala_pkg::build(&product, &tmp.path().join("x.pkg")).unwrap();
}

#[test]
fn dotfiles_in_a_tree_ship() {
    // Why: pkgbuild ships dotfiles; a tree's contents aren't typed by the
    // user, so a stray `.keep` must not fail the build at the BOM stage.
    let tmp = tempfile::tempdir().unwrap();
    write(&tmp.path().join("root/.zshrc"), b"x", 0o644);
    write(&tmp.path().join("root/etc/.keep"), b"", 0o644);
    let built = Built::new(
        &spec(vec![entry(&tmp.path().join("root"), ".")]),
        tmp.path(),
    );
    let names: Vec<String> = built.cpio().into_iter().map(|e| e.0).collect();
    assert!(names.contains(&"./.zshrc".to_string()), "{names:?}");
    assert!(names.contains(&"./etc/.keep".to_string()), "{names:?}");
    let bom: Vec<String> = built
        .bom_paths()
        .iter()
        .map(|p| p.path().to_string())
        .collect();
    assert!(bom.iter().any(|p| p.ends_with(".zshrc")), "{bom:?}");
}

#[test]
fn install_locations_that_alias_another_are_rejected() {
    // Why: `/usr//local` and `/usr/local/.` install where `/usr/local` does
    // but compare unequal, so they would slip past the collision check.
    let tmp = tempfile::tempdir().unwrap();
    write(&tmp.path().join("f"), b"x", 0o644);
    for location in ["/usr//local", "/usr/local/.", "/usr/../local", "usr/local"] {
        let spec = component("c", location, vec![entry(&tmp.path().join("f"), "f")]);
        let err = build(&spec, &tmp.path().join("x.pkg")).unwrap_err();
        assert!(
            matches!(err, Error::InvalidInstallLocation(_)),
            "{location}: {err}"
        );
    }
    for location in ["/", "/usr/local/"] {
        let spec = component("c", location, vec![entry(&tmp.path().join("f"), "f")]);
        build(&spec, &tmp.path().join("x.pkg")).unwrap();
    }
}

#[test]
fn components_differing_only_in_case_collide() {
    // Why: macOS volumes are case-insensitive by default, so Foo.app and
    // foo.app are one bundle and the later component would overwrite it.
    let tmp = tempfile::tempdir().unwrap();
    write(&tmp.path().join("f"), b"x", 0o755);
    let product = product(vec![
        component(
            "a",
            "/Applications",
            vec![entry(&tmp.path().join("f"), "Foo.app/Contents/MacOS/foo")],
        ),
        component(
            "b",
            "/Applications",
            vec![entry(&tmp.path().join("f"), "foo.app/Contents/MacOS/foo")],
        ),
    ]);
    let err = embala_pkg::validate(&product).unwrap_err();
    assert!(
        matches!(&err, Error::ComponentCollision { path, .. } if path == "/Applications/foo.app/Contents/MacOS/foo"),
        "{err}"
    );
}

#[test]
fn a_declared_dot_directory_is_rejected() {
    // Why: "." is the install root itself, not a directory to declare; it
    // used to pass validation and fail later with a BOM path error.
    let tmp = tempfile::tempdir().unwrap();
    write(&tmp.path().join("f"), b"x", 0o644);
    let mut spec = spec(vec![entry(&tmp.path().join("f"), "f")]);
    spec.directories = vec![DirSpec {
        dest: ".".to_string(),
        mode: None,
    }];
    let err = embala_pkg::validate(&product(vec![spec])).unwrap_err();
    assert!(matches!(err, Error::InvalidDest(_)), "{err}");
}
