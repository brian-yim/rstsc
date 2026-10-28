# Build the RPM (from repo root; results under ~/rpmbuild/RPMS/):
#   git archive --format=tar.gz --prefix=rstsc-0.1.0/ -o ~/rpmbuild/SOURCES/rstsc-0.1.0.tar.gz HEAD
#   mkdir -p ~/rpmbuild/SPECS && cp packaging/rpm/rstsc.spec ~/rpmbuild/SPECS/
#   rpmbuild -bb ~/rpmbuild/SPECS/rstsc.spec
#
# Notes:
# - Requires rustc >= 1.94 in the build env ($HOME/.cargo/bin on PATH works
#   from rustup). Distro rust (1.92 on RHEL/Rocky 9) is too old.
# - First build of the pinned IronRDP git deps needs network access for cargo.
# - rpm-build not installed on the host? Build inside a container:
#     docker run -d --name rstsc-build -v "$PWD":/src:Z rockylinux:9 sleep infinity
#     docker exec rstsc-build dnf install -y rpm-build curl gcc alsa-lib-devel
#     docker exec rstsc-build bash -c 'curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
#     docker exec rstsc-build bash -c 'mkdir -p ~/rpmbuild/SOURCES ~/rpmbuild/SPECS && \
#       cp /src/packaging/rpm/rstsc.spec ~/rpmbuild/SPECS/ && \
#       cp /src/packaging/rpm/SOURCES/rstsc-0.1.0.tar.gz ~/rpmbuild/SOURCES/ && \
#       ~/.cargo/bin/rpmbuild -bb ~/rpmbuild/SPECS/rstsc.spec'


Name:           rstsc
Version:        0.1.0
Release:        1%{?dist}
Summary:        Linux Remote Desktop client for Windows hosts (mstsc-style), IronRDP engine

License:        MIT OR Apache-2.0
URL:            https://github.com/brianyim/rstsc
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  gcc
BuildRequires:  gcc-c++
# cargo/rust: distro rustc (1.92) is < 1.94, so the toolchain comes from
# rustup in the build env. Install distro `cargo`/`rust` only to satisfy
# these rpm-level BuildRequires, or build with --nodeps:
# BuildRequires:  cargo
# BuildRequires:  rust
BuildRequires:  alsa-lib-devel
Requires:       alsa-lib
Requires:       (mesa-libGL if mesa-libGL)
Requires:       gtk3

# First build of the pinned IronRDP git deps needs network access for cargo.
%global _lto_cflags %{nil}
%undefine _auto_set_build_flags
# Rust binaries: find-debuginfo finds no C sources -> empty debugsource pkg.
%define debug_package %{nil}

%description
rstsc is a Linux RDP client for Windows hosts, replicating the look and behavior
of Microsoft's mstsc (Remote Desktop Connection). Built on IronRDP (pure Rust);
contributes an egui GUI, session embedding, connection manager, Linux
X11/Wayland clipboard integration, drive redirection, RD Gateway, and optional
remote audio/microphone redirection (ALSA).

%global icons rstsc-512 rstsc-256 rstsc-128 rstsc-96 rstsc-64 rstsc-48 rstsc-32 rstsc-24 rstsc-16

%prep
%setup -q

%build
cargo build --release

%install
install -D -m 755 target/release/rstsc %{buildroot}%{_bindir}/rstsc
install -D -m 644 assets/rstsc.desktop %{buildroot}%{_datadir}/applications/rstsc.desktop
install -D -m 644 assets/rstsc.png %{buildroot}%{_datadir}/icons/hicolor/512x512/apps/rstsc.png
for s in 256 128 96 64 48 32 24 16; do
  install -D -m 644 assets/rstsc-${s}.png %{buildroot}%{_datadir}/icons/hicolor/${s}x${s}/apps/rstsc.png
done

%files
%doc README.md
%{_bindir}/rstsc
%{_datadir}/applications/rstsc.desktop
%{_datadir}/icons/hicolor/*/apps/rstsc.png

%changelog
* Wed Oct 28 2026 brianyim <brianyim@localhost> - 0.1.0-1
- Initial packaging.
