%global crate icloud-for-linux
# The release build has no debug symbols, so the automatic debuginfo package
# would be empty.
%global debug_package %{nil}

Name:           %{crate}
Version:        0.6.0
Release:        1%{?dist}
Summary:        Mount iCloud Drive on Linux as a local-first FUSE filesystem

License:        GPL-3.0-only
URL:            https://github.com/einyx/icloud-for-linux
Source0:        %{crate}-%{version}.tar.gz

BuildArch:      x86_64 aarch64

Requires:       fuse3

%description
icloud-for-linux mounts an iCloud Drive as an ordinary folder on Linux.

The filesystem is local-first. The metadata is cached in a SQLite index, the
content is kept in a content-addressed store, and every local change is
written to a journal before it is uploaded. Reads are served from the local
store and never wait for the network; a path is resolved when it is first
used instead of crawling the whole drive.

%prep
%autosetup -p1 -n %{crate}-%{version}

%build
export CARGO_HOME="%{_builddir}/cargo-home"
export RUSTFLAGS="${RUSTFLAGS:-} -C debuginfo=0"
# The build directory stays inside the build root, so the package does not
# depend on a cargo configuration of the build machine.
cargo build --release --locked --target-dir target --features completions

%install
install -Dm755 target/release/icloudctl %{buildroot}%{_bindir}/icloudctl
install -Dm644 docs/icloudctl.1 %{buildroot}%{_mandir}/man1/icloudctl.1
install -Dm644 config.example.yaml %{buildroot}%{_sysconfdir}/%{crate}/config.yaml.example
install -Dm644 packaging/systemd/icloud-for-linux.service \
    %{buildroot}%{_prefix}/lib/systemd/user/%{crate}.service
# The completion script is generated from the command line definition.
mkdir -p %{buildroot}%{_datadir}/bash-completion/completions
target/release/icloudctl-completions bash \
    > %{buildroot}%{_datadir}/bash-completion/completions/%{crate}

%check
# The tests are only compiled here: the build environment has no FUSE device.
cargo test --release --locked --no-run --target-dir target

%files
%license LICENSE
%doc README.md
%doc NOTICE
%doc CHANGELOG.md
%{_bindir}/icloudctl
%{_mandir}/man1/icloudctl.1*
%config(noreplace) %{_sysconfdir}/%{crate}/config.yaml.example
%{_prefix}/lib/systemd/user/%{crate}.service
%{_datadir}/bash-completion/completions/%{crate}

%changelog
* Sat Nov 14 2026 Alessio <alessio@linux.com> - 0.6.0-1
- Rust rewrite of the iCloud Drive FUSE filesystem, licensed under GPL-3.0-only.
- Local-first core: content addressed store, lazy path index, write ahead
  journal.
- Apple protocol client: SRP sign in, two factor authentication, drive service.
- Background hydration, on demand sync over SIGUSR1, conflict export.
