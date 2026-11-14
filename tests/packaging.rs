//! The packaging metadata has to stay in step with the crate.
//!
//! A version that is bumped in `Cargo.toml` but not in the spec, the recipe
//! or the changelog produces a package with the wrong version — the kind of
//! mistake that only shows up after a release.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn crate_version() -> String {
    let manifest = std::fs::read_to_string(root().join("Cargo.toml")).unwrap();
    manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = "))
        .map(|value| value.trim_matches('"').to_string())
        .expect("the manifest has a version")
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative))
        .unwrap_or_else(|e| panic!("cannot read {relative}: {e}"))
}

#[test]
fn the_package_versions_match_the_crate() {
    let version = crate_version();

    let spec = read("packaging/rpm/icloud-for-linux.spec");
    let spec_version = spec
        .lines()
        .find_map(|line| line.strip_prefix("Version:"))
        .map(|value| value.trim().to_string())
        .expect("the spec has a version");
    assert_eq!(spec_version, version, "the RPM spec is out of date");

    let pkgbuild = read("packaging/aur/PKGBUILD");
    let aur_version = pkgbuild
        .lines()
        .find_map(|line| line.strip_prefix("pkgver="))
        .map(|value| value.trim().to_string())
        .expect("the recipe has a version");
    assert_eq!(aur_version, version, "the AUR recipe is out of date");

    let nix = read("nix/package.nix");
    let nix_version = nix
        .lines()
        .find_map(|line| line.trim().strip_prefix("version = "))
        .map(|value| value.trim_end_matches(';').trim_matches('"').to_string())
        .expect("the nix package has a version");
    assert_eq!(nix_version, version, "the nix package is out of date");

    let changelog = read("debian/changelog");
    assert!(
        changelog.starts_with(&format!("icloud-for-linux ({version})")),
        "the Debian changelog is out of date"
    );

    let man = read("docs/icloudctl.1");
    assert!(
        man.contains(&format!("icloud-for-linux {version}")),
        "the man page is out of date"
    );
}

#[test]
fn the_packages_ship_the_same_files() {
    // The files every package has to carry.
    let expected = [
        "src/main.rs",
        "docs/icloudctl.1",
        "config.example.yaml",
        "packaging/systemd/icloud-for-linux.service",
    ];
    for file in expected {
        assert!(root().join(file).exists(), "{file} is missing");
    }

    // The service unit points at the installed binary.
    let unit_path = root().join("packaging/systemd/icloud-for-linux.service");
    assert!(unit_path.exists());
    let unit = read("packaging/systemd/icloud-for-linux.service");
    assert!(
        unit.contains("ExecStart=/usr/bin/icloudctl mount"),
        "the unit must start the installed binary"
    );
    assert!(
        unit.contains("WantedBy=default.target"),
        "the unit has to be a user unit"
    );
}

#[test]
fn the_source_tarball_contains_the_packaging() {
    // Everything the packages are built from has to be in the tarball.
    let script = read("Makefile");
    for entry in [
        "src",
        "tests",
        "docs",
        "packaging",
        "debian",
        "nix",
        "flake.nix",
        "Cargo.lock",
    ] {
        assert!(
            script.contains(entry),
            "the source tarball does not include {entry}"
        );
    }
    assert!(Path::new(&root().join("flake.nix")).exists());
}
