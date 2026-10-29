Name:           vivar
Version:        %{version}
Release:        1
Summary:        Fast, license-safe Python environment manager
License:        Proprietary
URL:            https://github.com/nextria-ca/vivar
BuildArch:      %{arch}
AutoReqProv:    no

%description
vivar manages Python versions, virtual environments, dependencies, lockfiles,
portable bundles and policy for a team. Single static binary; uv is used for
dependency resolution.

%install
mkdir -p %{buildroot}/usr/bin
install -m 755 %{binary} %{buildroot}/usr/bin/vivar

%files
/usr/bin/vivar
